use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent_id::AgentId;

/// A specific permission that can be granted to an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Capability {
    FileRead {
        path_glob: String,
    },
    FileWrite {
        path_glob: String,
    },
    WebSearch,
    NetworkAccess {
        hosts: Vec<String>,
    },
    SpawnChild {
        allowed_agents: Vec<String>,
    },
    ToolInvoke {
        tool_name: String,
    },
    MessageSend {
        target_agents: Vec<String>,
    },
    Custom {
        name: String,
        params: serde_json::Value,
    },
}

/// Rate limiting configuration for a capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimit {
    pub max_per_minute: u32,
}

/// Constraints that narrow a capability's scope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraints {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_invocations: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
}

/// An unforgeable token granting a specific capability to a specific agent.
///
/// Tokens are created by the kernel at agent spawn time based on the
/// agent's manifest. They can only be narrowed (adding more constraints),
/// never escalated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityToken {
    pub id: Uuid,
    pub agent_id: AgentId,
    pub capability: Capability,
    pub constraints: Constraints,
    pub issued_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<DateTime<Utc>>,
    /// Number of times this token has been used. Compared against max_invocations.
    #[serde(default)]
    pub invocation_count: u64,
}

impl CapabilityToken {
    /// Issue a new capability token for an agent.
    pub fn issue(agent_id: AgentId, capability: Capability, constraints: Constraints) -> Self {
        Self {
            id: Uuid::new_v4(),
            agent_id,
            capability,
            constraints,
            issued_at: Utc::now(),
            expires_at: None,
            revoked_at: None,
            invocation_count: 0,
        }
    }

    /// Check if this token has expired.
    pub fn is_expired(&self) -> bool {
        self.expires_at.is_some_and(|exp| Utc::now() > exp)
    }

    /// Check if this token has been revoked.
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }

    /// Revoke this token. Once revoked, `permits()` always returns false.
    pub fn revoke(&mut self) {
        self.revoked_at = Some(Utc::now());
    }

    /// Check if this token grants the requested capability.
    /// Checks expiry, revocation, and max_invocations constraint.
    pub fn permits(&self, requested: &Capability) -> bool {
        if self.is_expired() || self.is_revoked() {
            return false;
        }
        // Check max_invocations constraint
        if let Some(max) = self.constraints.max_invocations {
            if self.invocation_count >= max {
                return false;
            }
        }
        self.capability_matches(requested)
    }

    /// Record a use of this token. Call after a successful operation.
    /// Returns false if the token has exhausted its invocations.
    pub fn record_use(&mut self) -> bool {
        self.invocation_count += 1;
        if let Some(max) = self.constraints.max_invocations {
            self.invocation_count <= max
        } else {
            true
        }
    }

    /// Check if this token has exhausted its invocation limit.
    pub fn is_exhausted(&self) -> bool {
        self.constraints
            .max_invocations
            .is_some_and(|max| self.invocation_count >= max)
    }

    fn capability_matches(&self, requested: &Capability) -> bool {
        match (&self.capability, requested) {
            (
                Capability::FileRead { path_glob: granted },
                Capability::FileRead { path_glob: req },
            ) => glob_matches(granted, req),
            (
                Capability::FileWrite { path_glob: granted },
                Capability::FileWrite { path_glob: req },
            ) => glob_matches(granted, req),
            (Capability::WebSearch, Capability::WebSearch) => true,
            (
                Capability::NetworkAccess { hosts: granted },
                Capability::NetworkAccess { hosts: req },
            ) => req.iter().all(|h| granted.contains(h)),
            (
                Capability::SpawnChild {
                    allowed_agents: granted,
                },
                Capability::SpawnChild {
                    allowed_agents: req,
                },
            ) => req.iter().all(|a| granted.contains(a)),
            (
                Capability::ToolInvoke { tool_name: granted },
                Capability::ToolInvoke { tool_name: req },
            ) => granted == req || granted == "*",
            (
                Capability::MessageSend {
                    target_agents: granted,
                },
                Capability::MessageSend { target_agents: req },
            ) => req
                .iter()
                .all(|a| granted.iter().any(|g| g == "*" || g == a)),
            (Capability::Custom { name: gn, .. }, Capability::Custom { name: rn, .. }) => gn == rn,
            _ => false,
        }
    }

    /// Create a narrowed copy of this token with additional constraints.
    pub fn narrow(&self, additional: Constraints) -> Self {
        let mut narrowed = self.clone();
        narrowed.id = Uuid::new_v4();
        if let Some(max) = additional.max_invocations {
            narrowed.constraints.max_invocations = Some(
                narrowed
                    .constraints
                    .max_invocations
                    .map_or(max, |existing| existing.min(max)),
            );
        }
        if let Some(rate) = additional.rate_limit {
            narrowed.constraints.rate_limit = Some(narrowed.constraints.rate_limit.map_or(
                rate.clone(),
                |existing| RateLimit {
                    max_per_minute: existing.max_per_minute.min(rate.max_per_minute),
                },
            ));
        }
        narrowed
    }
}

/// Glob matching with path canonicalization to prevent traversal and symlink
/// bypass attacks.
///
/// The *pattern* (from a trusted manifest) is normalized lexically only — its
/// path components are authoritative as written by the operator.
///
/// The *requested path* (from a potentially-adversarial agent) is first
/// canonicalized against the real filesystem (resolving symlinks and `..`),
/// then matched. For paths that don't exist yet (e.g. a new file about to be
/// written), canonicalization walks up to the nearest existing ancestor,
/// canonicalizes it, then re-appends the non-existent tail. This closes the
/// Run 9 finding: a symlink `/data/project -> /etc` no longer lets a grant
/// of `/data/*` reach `/etc/passwd`.
///
/// Caveat: canonicalize-then-open is not atomic (TOCTOU). An attacker who can
/// swap a symlink between the capability check and the actual open() can still
/// redirect. Stronger guarantees require `openat(AT_FDCWD, ..., O_NOFOLLOW)`
/// and comparing fstat against the grant, which is platform-specific. Tracked
/// as a follow-up in `docs/ideas.md`.
fn glob_matches(pattern: &str, path: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let canonical = canonical_for_match(path);
    if let Some(prefix) = pattern.strip_suffix('*') {
        let norm_prefix = normalize_path(prefix);
        canonical.starts_with(&norm_prefix)
    } else {
        let norm_pattern = normalize_path(pattern);
        norm_pattern == canonical
    }
}

/// Canonicalize a requested path for capability matching. Resolves symlinks
/// via the filesystem when possible. For paths that don't exist (e.g. a new
/// file about to be written), canonicalizes the nearest existing ancestor
/// with respect to the **lexically normalized** input — preserving the
/// traversal-blocking behavior of the pre-Fix-4 normalizer.
///
/// Why lexical-first then canonicalize: `PathBuf::pop()` + `push()` do not
/// round-trip `..` components reliably (push treats `..` as a literal
/// component when the base is absolute, so "foo/.." + "bar" stays "foo/../bar"
/// rather than collapsing). Normalizing lexically first removes `..` entirely,
/// leaving only real path components to feed to the filesystem.
fn canonical_for_match(path: &str) -> String {
    use std::path::{Path, PathBuf};

    // 1. Lexically normalize FIRST — resolves `..` / `.` so the filesystem
    //    sees the intended path, not a traversal attempt. After this step,
    //    "/data/../etc/passwd" is "/etc/passwd" regardless of whether /data
    //    exists.
    let normalized = normalize_path(path);

    // 2. Try to canonicalize the normalized path. If it exists and contains
    //    symlinks, this resolves them to their real targets.
    if let Ok(canonical) = std::fs::canonicalize(&normalized) {
        return canonical.to_string_lossy().into_owned();
    }

    // 3. Path doesn't exist yet (writing a new file, or a path inside a
    //    not-yet-created dir). Walk up the normalized path and canonicalize
    //    the nearest existing ancestor, then re-attach the remaining tail.
    //    Because the input is already lexically normalized, pop/push here is
    //    safe — no `..` components remain.
    let p = Path::new(&normalized);
    let mut ancestor: PathBuf = p.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Some(name) = ancestor.file_name() {
            tail.push(name.to_os_string());
        }
        if !ancestor.pop() {
            break;
        }
        if let Ok(mut canonical) = std::fs::canonicalize(&ancestor) {
            for seg in tail.iter().rev() {
                canonical.push(seg);
            }
            return canonical.to_string_lossy().into_owned();
        }
    }

    // 4. No ancestor resolved (e.g., entire path tree doesn't exist, as is
    //    common in unit tests or fresh test containers). Return the
    //    lexically-normalized form — still traversal-safe.
    normalized
}

/// Lexical path normalization: resolves `.` and `..` without filesystem access.
/// Prevents path traversal attacks while working inside containers where
/// paths may not exist yet (e.g., `/output/` before any file is written).
fn normalize_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => { parts.pop(); }
            other => parts.push(other),
        }
    }
    if path.starts_with('/') {
        format!("/{}", parts.join("/"))
    } else {
        parts.join("/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_agent() -> AgentId {
        AgentId::new()
    }

    #[test]
    fn file_read_glob_matching() {
        let token = CapabilityToken::issue(
            test_agent(),
            Capability::FileRead {
                path_glob: "/data/project/*".into(),
            },
            Constraints::default(),
        );
        assert!(token.permits(&Capability::FileRead {
            path_glob: "/data/project/foo.txt".into()
        }));
        assert!(!token.permits(&Capability::FileRead {
            path_glob: "/etc/passwd".into()
        }));
    }

    #[test]
    fn tool_invoke_wildcard() {
        let token = CapabilityToken::issue(
            test_agent(),
            Capability::ToolInvoke {
                tool_name: "*".into(),
            },
            Constraints::default(),
        );
        assert!(token.permits(&Capability::ToolInvoke {
            tool_name: "web_search".into()
        }));
    }

    #[test]
    fn capability_type_mismatch() {
        let token =
            CapabilityToken::issue(test_agent(), Capability::WebSearch, Constraints::default());
        assert!(!token.permits(&Capability::FileRead {
            path_glob: "/tmp/*".into()
        }));
    }

    #[test]
    fn narrowing_reduces_constraints() {
        let token = CapabilityToken::issue(
            test_agent(),
            Capability::WebSearch,
            Constraints {
                max_invocations: Some(100),
                rate_limit: None,
            },
        );
        let narrowed = token.narrow(Constraints {
            max_invocations: Some(10),
            rate_limit: None,
        });
        assert_eq!(narrowed.constraints.max_invocations, Some(10));
    }

    #[test]
    fn path_traversal_blocked() {
        let token = CapabilityToken::issue(
            test_agent(),
            Capability::FileRead { path_glob: "/data/*".into() },
            Constraints::default(),
        );
        // Direct traversal
        assert!(!token.permits(&Capability::FileRead {
            path_glob: "/data/../etc/passwd".into()
        }));
        // Double traversal
        assert!(!token.permits(&Capability::FileRead {
            path_glob: "/data/foo/../../etc/shadow".into()
        }));
        // Dot components
        assert!(!token.permits(&Capability::FileRead {
            path_glob: "/data/./../../etc/passwd".into()
        }));
        // Legitimate subpath still works
        assert!(token.permits(&Capability::FileRead {
            path_glob: "/data/project/file.txt".into()
        }));
    }

    #[test]
    fn normalize_path_works() {
        assert_eq!(normalize_path("/data/../etc/passwd"), "/etc/passwd");
        assert_eq!(normalize_path("/data/foo/../../etc"), "/etc");
        assert_eq!(normalize_path("/data/./file.txt"), "/data/file.txt");
        assert_eq!(normalize_path("/data/project/"), "/data/project");
        assert_eq!(normalize_path("/"), "/");
    }

    #[test]
    fn max_invocations_enforced() {
        let mut token = CapabilityToken::issue(
            test_agent(),
            Capability::WebSearch,
            Constraints {
                max_invocations: Some(3),
                rate_limit: None,
            },
        );
        // First 3 uses: permitted
        assert!(token.permits(&Capability::WebSearch));
        token.record_use();
        assert!(token.permits(&Capability::WebSearch));
        token.record_use();
        assert!(token.permits(&Capability::WebSearch));
        token.record_use();
        // 4th use: denied
        assert!(!token.permits(&Capability::WebSearch));
        assert!(token.is_exhausted());
    }

    #[test]
    fn no_max_invocations_unlimited() {
        let mut token = CapabilityToken::issue(
            test_agent(),
            Capability::WebSearch,
            Constraints::default(), // no max_invocations
        );
        for _ in 0..1000 {
            assert!(token.permits(&Capability::WebSearch));
            token.record_use();
        }
        assert!(!token.is_exhausted());
    }

    #[test]
    fn revoked_token_denies_access() {
        let mut token = CapabilityToken::issue(
            test_agent(),
            Capability::FileRead { path_glob: "/data/*".into() },
            Constraints::default(),
        );
        // Before revocation: permits
        assert!(token.permits(&Capability::FileRead { path_glob: "/data/file.txt".into() }));
        assert!(!token.is_revoked());

        // Revoke
        token.revoke();

        // After revocation: denies
        assert!(token.is_revoked());
        assert!(token.revoked_at.is_some());
        assert!(!token.permits(&Capability::FileRead { path_glob: "/data/file.txt".into() }));
    }

    #[test]
    fn revoked_token_roundtrips_json() {
        let mut token = CapabilityToken::issue(
            test_agent(),
            Capability::WebSearch,
            Constraints::default(),
        );
        token.revoke();
        let json = serde_json::to_string(&token).unwrap();
        let parsed: CapabilityToken = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_revoked());
        assert!(!parsed.permits(&Capability::WebSearch));
    }

    #[test]
    fn token_roundtrips_json() {
        let token = CapabilityToken::issue(
            test_agent(),
            Capability::NetworkAccess {
                hosts: vec!["api.example.com".into()],
            },
            Constraints::default(),
        );
        let json = serde_json::to_string(&token).unwrap();
        let parsed: CapabilityToken = serde_json::from_str(&json).unwrap();
        assert_eq!(token.capability, parsed.capability);
    }

    #[test]
    fn symlink_bypass_blocked() {
        // Run 9 Fix 4: a symlink inside a granted prefix must not redirect
        // out of it. Create a tmpdir, put a symlink in it pointing to /etc,
        // grant access only to the tmpdir, and verify a read-through-symlink
        // request is denied.
        use std::path::PathBuf;
        let base = std::env::temp_dir().join(format!("aaos-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&base).expect("create base dir");
        let link_path: PathBuf = base.join("evil-link");
        // Skip test if symlink creation fails (unusual filesystems / CI
        // containers without the needed permission). The cross-platform
        // guard keeps the test portable while still covering the case
        // everywhere we can.
        #[cfg(unix)]
        let created = std::os::unix::fs::symlink("/etc", &link_path).is_ok();
        #[cfg(not(unix))]
        let created = false;
        if !created {
            // Clean up and exit — symlinks aren't available here.
            let _ = std::fs::remove_dir_all(&base);
            return;
        }

        let grant = format!("{}/*", base.to_string_lossy());
        let token = CapabilityToken::issue(
            test_agent(),
            Capability::FileRead { path_glob: grant.clone() },
            Constraints::default(),
        );

        // Legit in-dir read: allowed (file doesn't exist but that's fine —
        // we canonicalize the parent and re-attach the tail).
        let legit = format!("{}/some-file.txt", base.to_string_lossy());
        assert!(
            token.permits(&Capability::FileRead { path_glob: legit.clone() }),
            "legitimate path in granted dir must still match: {legit} vs {grant}"
        );

        // Symlink bypass attempt: reading via evil-link/passwd would reach
        // /etc/passwd, which is OUTSIDE the grant. Must be denied.
        let bypass = format!("{}/evil-link/passwd", base.to_string_lossy());
        assert!(
            !token.permits(&Capability::FileRead { path_glob: bypass.clone() }),
            "symlink bypass must be blocked: {bypass} reaches /etc/passwd"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn canonicalize_falls_back_lexically_for_nonexistent_paths() {
        // Run 9 Fix 4: paths that don't exist (e.g. a new file about to be
        // written to /output/) must still match. Canonicalization walks up
        // to the nearest existing ancestor and re-attaches the tail.
        // Here both pattern and path reference a definitely-nonexistent
        // tree, so the lexical fallback kicks in.
        let token = CapabilityToken::issue(
            test_agent(),
            Capability::FileWrite {
                path_glob: "/nonexistent-aaos-root-xyz/*".into(),
            },
            Constraints::default(),
        );
        assert!(token.permits(&Capability::FileWrite {
            path_glob: "/nonexistent-aaos-root-xyz/new-file.txt".into()
        }));
    }
}
