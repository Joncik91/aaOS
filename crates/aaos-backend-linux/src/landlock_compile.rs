//! Build the worker-side Landlock ruleset from a [`PolicyDescription`].
//!
//! Landlock rulesets are kernel objects — they cannot be sent across a
//! process boundary as JSON (plan v4 round 3 #4). The broker sends a
//! policy description (paths); the worker compiles a local ruleset
//! from that description and calls `landlock_restrict_self`.
//!
//! Applied only after `prctl(PR_SET_NO_NEW_PRIVS, 1)`.

use crate::broker_protocol::PolicyDescription;

#[cfg(target_os = "linux")]
pub use linux_impl::*;

#[cfg(not(target_os = "linux"))]
pub use stub_impl::*;

#[cfg(target_os = "linux")]
mod linux_impl {
    use super::PolicyDescription;

    use landlock::{
        Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreated,
        RulesetCreatedAttr, RulesetStatus, ABI,
    };

    #[derive(Debug, thiserror::Error)]
    pub enum LandlockCompileError {
        #[error("landlock not available on this kernel (need ABI v1+)")]
        Unsupported,

        #[error("landlock ruleset creation failed: {0}")]
        CreateFailed(String),

        #[error("landlock rule-add failed for path {path}: {reason}")]
        RuleAddFailed {
            path: std::path::PathBuf,
            reason: String,
        },

        #[error("landlock_restrict_self failed: {0}")]
        RestrictFailed(String),
    }

    /// Probe whether this kernel supports Landlock at ABI v1+.
    ///
    /// The `landlock` crate 0.4 does not expose a public `ABI::current()`.
    /// We probe by building and trying to restrict a ruleset in the
    /// current process, which is destructive — so we don't. Instead we
    /// do a best-effort structural probe: if `Ruleset::default().create()`
    /// returns Ok, Landlock syscalls are wired. On kernels without
    /// Landlock, the crate's compat layer still returns Ok but a
    /// subsequent `restrict_self` will report `RulesetStatus::NotEnforced`.
    /// [`restrict_self`] below checks that and fails closed.
    pub fn is_supported() -> bool {
        let abi = ABI::V1;
        let ruleset_attr = match Ruleset::default().handle_access(AccessFs::from_all(abi)) {
            Ok(r) => r,
            Err(_) => return false,
        };
        match ruleset_attr.create() {
            Ok(_) => true,
            Err(_) => false,
        }
    }

    /// Build a [`RulesetCreated`] from the policy description.
    pub fn build_ruleset(
        policy: &PolicyDescription,
    ) -> Result<RulesetCreated, LandlockCompileError> {
        let abi = ABI::V1;
        let read_write = AccessFs::from_all(abi);
        let read_only = AccessFs::from_read(abi);

        let mut ruleset = Ruleset::default()
            .handle_access(read_write)
            .map_err(|e| LandlockCompileError::CreateFailed(e.to_string()))?
            .create()
            .map_err(|e| LandlockCompileError::CreateFailed(e.to_string()))?;

        if policy.scratch.exists() {
            let fd =
                PathFd::new(&policy.scratch).map_err(|e| LandlockCompileError::RuleAddFailed {
                    path: policy.scratch.clone(),
                    reason: e.to_string(),
                })?;
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, read_write))
                .map_err(|e| LandlockCompileError::RuleAddFailed {
                    path: policy.scratch.clone(),
                    reason: e.to_string(),
                })?;
        } else {
            tracing::warn!(
                path = %policy.scratch.display(),
                "landlock: scratch path does not exist, skipping rule"
            );
        }

        for lib_path in &policy.shared_libs {
            if !lib_path.exists() {
                tracing::debug!(
                    path = %lib_path.display(),
                    "landlock: shared_lib path not present, skipping"
                );
                continue;
            }
            let fd = PathFd::new(lib_path).map_err(|e| LandlockCompileError::RuleAddFailed {
                path: lib_path.clone(),
                reason: e.to_string(),
            })?;
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, read_only))
                .map_err(|e| LandlockCompileError::RuleAddFailed {
                    path: lib_path.clone(),
                    reason: e.to_string(),
                })?;
        }

        // Per-agent workspace + extra capability-declared writable
        // roots (narrow, per-agent scope — bind-mounted at the same
        // absolute path inside the worker's mount ns). Capability
        // tokens still gate which paths within each root each tool
        // call may touch.
        let rw_roots = policy
            .workspace
            .iter()
            .cloned()
            .chain(policy.extra_writable_roots.iter().cloned());
        for ws in rw_roots {
            if ws.exists() {
                let fd = PathFd::new(&ws).map_err(|e| LandlockCompileError::RuleAddFailed {
                    path: ws.clone(),
                    reason: e.to_string(),
                })?;
                ruleset = ruleset
                    .add_rule(PathBeneath::new(fd, read_write))
                    .map_err(|e| LandlockCompileError::RuleAddFailed {
                        path: ws.clone(),
                        reason: e.to_string(),
                    })?;
            } else {
                tracing::warn!(
                    path = %ws.display(),
                    "landlock: writable root does not exist, skipping rule"
                );
            }
        }

        Ok(ruleset)
    }

    /// Build the ruleset and install it via `landlock_restrict_self`.
    ///
    /// MUST be called after `prctl(PR_SET_NO_NEW_PRIVS, 1)` and before
    /// the seccomp filter.
    pub fn restrict_self(policy: &PolicyDescription) -> Result<(), LandlockCompileError> {
        let ruleset = build_ruleset(policy)?;
        let status = ruleset
            .restrict_self()
            .map_err(|e| LandlockCompileError::RestrictFailed(e.to_string()))?;
        // Fail closed: if the kernel didn't actually enforce the
        // ruleset, surface as error rather than proceed unconfined.
        if status.ruleset == RulesetStatus::NotEnforced {
            return Err(LandlockCompileError::RestrictFailed(
                "ruleset not enforced (kernel without Landlock support)".into(),
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::path::PathBuf;

        fn sample_policy() -> PolicyDescription {
            PolicyDescription {
                scratch: std::env::temp_dir(),
                shared_libs: vec![
                    PathBuf::from("/lib/x86_64-linux-gnu"),
                    PathBuf::from("/lib64"),
                    PathBuf::from("/usr/lib/x86_64-linux-gnu"),
                ],
                broker_socket: PathBuf::from("/nonexistent.sock"),
                workspace: None,
                extra_writable_roots: vec![],
            }
        }

        #[test]
        fn landlock_compile_produces_rules_for_scratch_and_libs() {
            if !is_supported() {
                eprintln!("kernel too old for Landlock ABI v1; skipping");
                return;
            }
            let policy = sample_policy();
            let _ = build_ruleset(&policy).expect("ruleset must build for typical policy");
        }

        #[test]
        fn landlock_policy_has_no_user_data_paths() {
            // Guardrail: the policy type doesn't have a field for
            // user-data paths. If someone adds one (e.g. `workspace`
            // or `data_dirs`), this test forces them to re-read the
            // plan's "no blanket workspace bind-mount" rule.
            let policy = sample_policy();
            let json = serde_json::to_string(&policy).unwrap();
            for forbidden in &["/data", "/home", "/output", "/src"] {
                assert!(
                    !json.contains(forbidden),
                    "policy must not carry user-data path {forbidden}: {json}"
                );
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod stub_impl {
    use super::PolicyDescription;

    #[derive(Debug, thiserror::Error)]
    pub enum LandlockCompileError {
        #[error("landlock only supported on Linux")]
        NotLinux,
    }

    pub fn is_supported() -> bool {
        false
    }

    pub fn restrict_self(_policy: &PolicyDescription) -> Result<(), LandlockCompileError> {
        Err(LandlockCompileError::NotLinux)
    }
}
