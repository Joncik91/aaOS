use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use chrono::Utc;
use dashmap::DashMap;
use uuid::Uuid;

use crate::agent_id::AgentId;
#[cfg(any(test, debug_assertions))]
use crate::capability::CapabilitySnapshot;
use crate::capability::{
    Capability, CapabilityDenied, CapabilityHandle, CapabilityToken, Constraints, FileAccess,
};

/// Hook called by [`CapabilityRegistry::revoke`] when a token is revoked.
///
/// Implementations live in backend crates that need to forward revocations
/// to active worker sessions (e.g. `aaos-backend-linux`). The registry
/// itself stays backend-agnostic: it calls the notifier after marking the
/// token revoked, and the notifier pushes a `RevokeToken` frame to any
/// workers that may hold that token.
///
/// For the daemon-internal in-process backend no notifier is needed — workers
/// share the same `Arc<CapabilityRegistry>`, so the revocation is visible
/// immediately.
pub trait RevokeNotifier: Send + Sync {
    fn revoke_published(&self, token_id: Uuid);
}

/// Runtime-owned table of issued capability tokens. Agents and tools hold
/// `CapabilityHandle` values; the underlying `CapabilityToken` and its
/// mutable state are never exposed outside runtime code.
pub struct CapabilityRegistry {
    table: DashMap<CapabilityHandle, OwnedEntry>,
    next_id: AtomicU64,
    notifier: OnceLock<Arc<dyn RevokeNotifier>>,
}

struct OwnedEntry {
    agent_id: AgentId,
    token: CapabilityToken,
}

impl Default for CapabilityRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self {
            table: DashMap::new(),
            next_id: AtomicU64::new(0),
            notifier: OnceLock::new(),
        }
    }

    /// Construct a registry that calls `notifier.revoke_published(token_id)`
    /// whenever a token is revoked. Used by the daemon-side `NamespacedBackend`
    /// to push revocations to active worker sessions.
    pub fn new_with_notifier(notifier: Arc<dyn RevokeNotifier>) -> Self {
        let lock = OnceLock::new();
        let _ = lock.set(notifier);
        Self {
            table: DashMap::new(),
            next_id: AtomicU64::new(0),
            notifier: lock,
        }
    }

    /// Install a revoke notifier on a registry that was constructed without
    /// one (`CapabilityRegistry::new()`).  This exists because the daemon
    /// builds its `AgentRegistry` (which owns the `CapabilityRegistry`)
    /// before the namespaced backend's `SessionMap` exists, so the notifier
    /// must be wired in post-construction.
    ///
    /// Returns `Err(_)` if a notifier is already installed — callers should
    /// not race installations.
    pub fn set_notifier(
        &self,
        notifier: Arc<dyn RevokeNotifier>,
    ) -> std::result::Result<(), Arc<dyn RevokeNotifier>> {
        self.notifier.set(notifier)
    }

    // ------- Issuance (runtime-only; used by AgentRegistry) -------
    //
    // The methods below are marked `pub` for cross-crate callability from
    // `aaos-runtime`'s `AgentRegistry`, not because tool code should call them.
    // Rustdoc on each method names them as runtime-internal. Tool code should
    // only call `permits()` and `authorize_and_record()` — the read-only
    // authorization surface.

    /// RUNTIME-INTERNAL. Issue a handle for a token. Called from
    /// `AgentRegistry::issue_capabilities`. Tool code must not call this.
    #[doc(hidden)]
    pub fn insert(&self, agent_id: AgentId, token: CapabilityToken) -> CapabilityHandle {
        let h = CapabilityHandle::from_raw(self.next_id.fetch_add(1, Ordering::AcqRel));
        self.table.insert(h, OwnedEntry { agent_id, token });
        h
    }

    /// RUNTIME-INTERNAL. Narrow: produce a new handle for a narrowed copy of
    /// the parent's token, owned by the child agent. Called from
    /// `SpawnAgentTool`. Tool code other than spawn must not call this.
    #[doc(hidden)]
    pub fn narrow(
        &self,
        parent_handle: CapabilityHandle,
        parent_agent: AgentId,
        child_agent: AgentId,
        additional: Constraints,
    ) -> Option<CapabilityHandle> {
        let narrowed = {
            let entry = self.table.get(&parent_handle)?;
            if entry.agent_id != parent_agent {
                return None;
            }
            entry.token.narrow(additional)
        };
        Some(self.insert(child_agent, narrowed))
    }

    /// Narrow a parent token into a child handle with a more-specific
    /// capability AND inherit the parent's constraints (intersected with
    /// any additional ones the caller supplies).  Used by spawn_agent /
    /// spawn_agents to delegate a subset capability to a child without
    /// dropping the parent's `max_invocations`, rate limits, or expiry.
    ///
    /// Returns `None` if the parent handle doesn't resolve, doesn't
    /// belong to `parent_agent`, or the child's `requested_capability`
    /// is not a subset of the parent's capability.
    ///
    /// Bug 27 fix (v0.1.7): replaces the spawn-path pattern of issuing
    /// fresh tokens with `Constraints::default()` (which silently
    /// dropped parent constraints).
    pub fn narrow_with_capability(
        &self,
        parent_handle: CapabilityHandle,
        parent_agent: AgentId,
        child_agent: AgentId,
        requested_capability: Capability,
        additional: Constraints,
    ) -> Option<CapabilityHandle> {
        let narrowed = {
            let entry = self.table.get(&parent_handle)?;
            if entry.agent_id != parent_agent {
                return None;
            }
            entry
                .token
                .narrow_with_capability(child_agent, requested_capability, additional)?
        };
        Some(self.insert(child_agent, narrowed))
    }

    // ------- Authorization (the hot path — tools call this) -------

    /// Atomic permit-check. Does NOT count as usage; use `authorize_and_record`
    /// for the tool-invocation path. Returns whether the handle belongs to
    /// `requesting_agent` AND holds a non-revoked, non-exhausted token that
    /// permits the requested capability.
    pub fn permits(
        &self,
        handle: CapabilityHandle,
        requesting_agent: AgentId,
        requested: &Capability,
    ) -> bool {
        let Some(entry) = self.table.get(&handle) else {
            return false;
        };
        if entry.agent_id != requesting_agent {
            return false;
        }
        entry.token.permits(requested)
    }

    /// TOCTOU-safe variant of [`permits`] for `FileRead`/`FileWrite`. Takes
    /// a kernel-derived canonical path (typically from
    /// `aaos_tools::path_safe::safe_open_for_capability`) and skips the
    /// extra `fs::canonicalize` that opens a symlink-swap window.
    /// Authorization is bound to the inode the caller already holds an
    /// fd on.
    pub fn permits_canonical_file(
        &self,
        handle: CapabilityHandle,
        requesting_agent: AgentId,
        kind: FileAccess,
        canonical: &str,
    ) -> bool {
        let Some(entry) = self.table.get(&handle) else {
            return false;
        };
        if entry.agent_id != requesting_agent {
            return false;
        }
        entry.token.permits_canonical_file(kind, canonical)
    }

    /// Atomic permit + record-use. This is what tool implementations should
    /// call when invoking a capability — it ensures max_invocations counts
    /// are consumed exactly once per successful check. Returns `Ok(())` if
    /// allowed (and increments invocation_count), `Err(reason)` otherwise.
    pub fn authorize_and_record(
        &self,
        handle: CapabilityHandle,
        requesting_agent: AgentId,
        requested: &Capability,
    ) -> Result<(), CapabilityDenied> {
        let mut entry = self
            .table
            .get_mut(&handle)
            .ok_or(CapabilityDenied::UnknownHandle)?;
        if entry.agent_id != requesting_agent {
            return Err(CapabilityDenied::WrongAgent);
        }
        if !entry.token.permits(requested) {
            // Determine specific denial reason
            if entry.token.is_revoked() {
                return Err(CapabilityDenied::Revoked);
            }
            if entry.token.is_exhausted() {
                return Err(CapabilityDenied::Exhausted);
            }
            return Err(CapabilityDenied::NotPermitted);
        }
        entry.token.record_use();
        Ok(())
    }

    // ------- Mutation (runtime-only) -------

    /// RUNTIME-INTERNAL. Revoke by token_id (the UUID on CapabilityToken).
    /// Matches the current `AgentRegistry::revoke_capability` signature. Tool
    /// code must not call this.
    ///
    /// If a [`RevokeNotifier`] is installed, `revoke_published` is called after
    /// the in-memory state is updated so active broker sessions can push a
    /// `RevokeToken` frame to their workers.
    #[doc(hidden)]
    pub fn revoke(&self, token_id: Uuid) -> bool {
        let mut revoked = false;
        for mut entry in self.table.iter_mut() {
            if entry.token.id == token_id && entry.token.revoked_at.is_none() {
                entry.token.revoked_at = Some(Utc::now());
                revoked = true;
            }
        }
        if revoked {
            if let Some(n) = self.notifier.get() {
                n.revoke_published(token_id);
            }
        }
        revoked
    }

    /// RUNTIME-INTERNAL. Revoke every token owned by the given agent. Used
    /// on capability-wipe and on agent removal. Tool code must not call this.
    /// Notifies the revoke hook for each revoked token so any in-flight
    /// worker session forgets the revoked tokens before the agent's session
    /// terminates.
    #[doc(hidden)]
    pub fn revoke_all_for_agent(&self, agent_id: AgentId) -> usize {
        let mut revoked_ids: Vec<Uuid> = Vec::new();
        for mut entry in self.table.iter_mut() {
            if entry.agent_id == agent_id && entry.token.revoked_at.is_none() {
                entry.token.revoked_at = Some(Utc::now());
                revoked_ids.push(entry.token.id);
            }
        }
        if let Some(n) = self.notifier.get() {
            for id in &revoked_ids {
                n.revoke_published(*id);
            }
        }
        revoked_ids.len()
    }

    /// RUNTIME-INTERNAL. Remove all handles belonging to an agent. Called
    /// from `AgentRegistry::remove_agent` after audit events for any
    /// revocations have been recorded. Tool code must not call this.
    #[doc(hidden)]
    pub fn remove_agent(&self, agent_id: AgentId) {
        self.table.retain(|_, entry| entry.agent_id != agent_id);
    }

    /// Read-only inspection for tests and debug. Does NOT return the token
    /// in a form tool code can use — returns a snapshot of fields relevant
    /// for testing (id, agent_id, revoked_at, invocation_count). Keeps
    /// CapabilityToken out of the public API.
    #[cfg(any(test, debug_assertions))]
    pub fn inspect(&self, handle: CapabilityHandle) -> Option<CapabilitySnapshot> {
        let entry = self.table.get(&handle)?;
        Some(CapabilitySnapshot {
            token_id: entry.token.id,
            agent_id: entry.agent_id,
            revoked: entry.token.revoked_at.is_some(),
            invocations_used: entry.token.invocation_count,
        })
    }

    /// Resolve a handle to just its underlying token id. Always compiled;
    /// needed by the runtime to map a caller-supplied token_id back to the
    /// handle that issued it (e.g. for revoke flows). Unlike `inspect`, does
    /// not expose other token fields.
    pub fn token_id_of(&self, handle: CapabilityHandle) -> Option<uuid::Uuid> {
        self.table.get(&handle).map(|entry| entry.token.id)
    }

    /// RUNTIME-INTERNAL. Resolve a slice of handles to their full
    /// `CapabilityToken` structs for the given agent. Handles that are
    /// unknown or belong to a different agent are silently skipped (fail-
    /// closed: the caller ends up with a smaller token set, not more).
    /// Revoked and expired tokens are also filtered out so that workers
    /// only ever receive tokens that are currently valid.
    ///
    /// Note: this is a point-in-time filter. A token revoked *after*
    /// `resolve_tokens` returns but before the worker invokes the tool
    /// will still be honoured by the in-flight worker call (residual race;
    /// closing it fully requires a push-revocation protocol — queued for
    /// v0.2.x as Bug 11 Option A).
    ///
    /// Used by `ToolInvocation` to collect the serializable token structs
    /// before forwarding them across the broker to a confined worker
    /// process. The worker rebuilds a local `CapabilityRegistry` from
    /// these structs and constructs an `InvocationContext` whose registry
    /// can satisfy the tool's internal `permits()` call.
    #[doc(hidden)]
    pub fn resolve_tokens(
        &self,
        handles: &[CapabilityHandle],
        agent_id: AgentId,
    ) -> Vec<CapabilityToken> {
        handles
            .iter()
            .filter_map(|h| {
                let entry = self.table.get(h)?;
                if entry.agent_id != agent_id {
                    return None;
                }
                // Filter out revoked or expired tokens before forwarding.
                if entry.token.is_revoked() || entry.token.is_expired() {
                    return None;
                }
                Some(entry.token.clone())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::sync::Barrier;

    fn test_agent(_name: &str) -> AgentId {
        AgentId::new()
    }

    struct MockNotifier {
        received: Mutex<Vec<Uuid>>,
    }

    impl MockNotifier {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                received: Mutex::new(vec![]),
            })
        }

        fn received(&self) -> Vec<Uuid> {
            self.received.lock().unwrap().clone()
        }
    }

    impl RevokeNotifier for MockNotifier {
        fn revoke_published(&self, token_id: Uuid) {
            self.received.lock().unwrap().push(token_id);
        }
    }

    #[test]
    fn revoke_notifies_subscriber() {
        let notifier = MockNotifier::new();
        let registry = CapabilityRegistry::new_with_notifier(notifier.clone());
        let agent = test_agent("a");
        let token = CapabilityToken::issue(agent, Capability::WebSearch, Constraints::default());
        let token_id = token.id;
        registry.insert(agent, token);

        assert!(registry.revoke(token_id));
        let received = notifier.received();
        assert_eq!(received.len(), 1, "notifier must be called exactly once");
        assert_eq!(
            received[0], token_id,
            "notifier must receive the correct token_id"
        );

        // Revoking an already-revoked token does not re-notify.
        assert!(!registry.revoke(token_id));
        assert_eq!(
            notifier.received().len(),
            1,
            "no second notification for already-revoked"
        );
    }

    #[test]
    fn authorize_records_use_atomically() {
        let registry = Arc::new(CapabilityRegistry::new());
        let agent = test_agent("a");
        let token = CapabilityToken::issue(
            agent,
            Capability::WebSearch,
            Constraints {
                max_invocations: Some(5),
                rate_limit: None,
            },
        );
        let handle = registry.insert(agent, token);

        let mut success_count = 0;
        for _ in 0..10 {
            if registry
                .authorize_and_record(handle, agent, &Capability::WebSearch)
                .is_ok()
            {
                success_count += 1;
            }
        }
        assert_eq!(success_count, 5); // max_invocations = 5
    }

    #[tokio::test]
    async fn authorize_records_use_atomically_concurrent() {
        let registry = Arc::new(CapabilityRegistry::new());
        let agent = test_agent("a");
        let token = CapabilityToken::issue(
            agent,
            Capability::WebSearch,
            Constraints {
                max_invocations: Some(10),
                rate_limit: None,
            },
        );
        let handle = registry.insert(agent, token);

        let num_tasks = 20;
        let barrier = Arc::new(Barrier::new(num_tasks));
        let mut handles = vec![];

        for _ in 0..num_tasks {
            let reg = registry.clone();
            let b = barrier.clone();
            let h = handle;
            let ag = agent;
            handles.push(tokio::spawn(async move {
                b.wait().await;
                reg.authorize_and_record(h, ag, &Capability::WebSearch)
                    .is_ok()
            }));
        }

        let results: Vec<bool> = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        let successes = results.into_iter().filter(|b| *b).count();
        assert_eq!(successes, 10);
    }

    #[test]
    fn permits_does_not_record_use() {
        let registry = CapabilityRegistry::new();
        let agent = test_agent("a");
        let token = CapabilityToken::issue(
            agent,
            Capability::WebSearch,
            Constraints {
                max_invocations: Some(3),
                rate_limit: None,
            },
        );
        let handle = registry.insert(agent, token);

        // Call permits many times
        for _ in 0..100 {
            assert!(registry.permits(handle, agent, &Capability::WebSearch));
        }
        // Still permits because permits doesn't consume invocations
        assert!(registry.permits(handle, agent, &Capability::WebSearch));
    }

    #[test]
    fn authorize_rejects_wrong_agent() {
        let registry = CapabilityRegistry::new();
        let agent_a = test_agent("a");
        let agent_b = test_agent("b");
        let token = CapabilityToken::issue(agent_a, Capability::WebSearch, Constraints::default());
        let handle = registry.insert(agent_a, token);

        let result = registry.authorize_and_record(handle, agent_b, &Capability::WebSearch);
        assert_eq!(result, Err(CapabilityDenied::WrongAgent));
    }

    #[test]
    fn authorize_rejects_unknown_handle() {
        let registry = CapabilityRegistry::new();
        let agent = test_agent("a");
        let forged = CapabilityHandle::from_raw(999999);

        let result = registry.authorize_and_record(forged, agent, &Capability::WebSearch);
        assert_eq!(result, Err(CapabilityDenied::UnknownHandle));
    }

    #[test]
    fn revoke_by_token_id_denies_future_authorize() {
        let registry = CapabilityRegistry::new();
        let agent = test_agent("a");
        let token = CapabilityToken::issue(agent, Capability::WebSearch, Constraints::default());
        let token_id = token.id;
        let handle = registry.insert(agent, token);

        // Before revocation: authorized
        assert!(registry
            .authorize_and_record(handle, agent, &Capability::WebSearch)
            .is_ok());

        // Revoke
        assert!(registry.revoke(token_id));

        // After revocation: denied with Revoked
        let result = registry.authorize_and_record(handle, agent, &Capability::WebSearch);
        assert_eq!(result, Err(CapabilityDenied::Revoked));
    }

    #[test]
    fn revoke_all_for_agent_affects_only_that_agent() {
        let registry = CapabilityRegistry::new();
        let agent_a = test_agent("a");
        let agent_b = test_agent("b");

        let token_a =
            CapabilityToken::issue(agent_a, Capability::WebSearch, Constraints::default());
        let token_b =
            CapabilityToken::issue(agent_b, Capability::WebSearch, Constraints::default());

        let handle_a = registry.insert(agent_a, token_a);
        let handle_b = registry.insert(agent_b, token_b);

        let revoked = registry.revoke_all_for_agent(agent_a);
        assert_eq!(revoked, 1);

        // Agent A's handle is revoked
        assert_eq!(
            registry.authorize_and_record(handle_a, agent_a, &Capability::WebSearch),
            Err(CapabilityDenied::Revoked)
        );
        // Agent B's handle still works
        assert!(registry
            .authorize_and_record(handle_b, agent_b, &Capability::WebSearch)
            .is_ok());
    }

    #[test]
    fn narrow_creates_distinct_handle_for_child() {
        let registry = CapabilityRegistry::new();
        let parent = test_agent("parent");
        let child = test_agent("child");

        let parent_token = CapabilityToken::issue(
            parent,
            Capability::WebSearch,
            Constraints {
                max_invocations: Some(100),
                rate_limit: None,
            },
        );
        let parent_handle = registry.insert(parent, parent_token);

        let child_handle = registry
            .narrow(
                parent_handle,
                parent,
                child,
                Constraints {
                    max_invocations: Some(10),
                    rate_limit: None,
                },
            )
            .expect("narrow should succeed");

        // Handles are distinct
        assert_ne!(parent_handle, child_handle);

        // Child handle is owned by child agent
        let snap = registry.inspect(child_handle).unwrap();
        assert_eq!(snap.agent_id, child);
    }

    #[test]
    fn remove_agent_drops_all_its_handles() {
        let registry = Arc::new(CapabilityRegistry::new());
        let agent = test_agent("a");
        let token = CapabilityToken::issue(agent, Capability::WebSearch, Constraints::default());
        let handle = registry.insert(agent, token);

        assert!(registry.inspect(handle).is_some());

        registry.remove_agent(agent);

        assert!(registry.inspect(handle).is_none());
    }
}
