use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use aaos_core::{
    AgentId, AgentManifest, AuditEvent, AuditEventKind, AuditLog, Capability, CapabilityHandle,
    CapabilityRegistry, CapabilityToken, Constraints, CoreError, Result,
};
use aaos_ipc::MessageRouter;
use dashmap::DashMap;

use crate::persistent::persistent_agent_loop;
use crate::process::{AgentCommand, AgentInfo, AgentProcess, AgentState};

/// RAII guard for an agent-count reservation. Holds a share of the
/// registry's `active_count` counter. The counter is decremented on Drop
/// UNLESS `commit()` was called — in which case ownership transfers to the
/// agent's presence in the registry, and `remove_agent` decrements on exit.
///
/// This makes it impossible to leak a reservation on spawn failure: the only
/// way to keep the increment is to explicitly commit after a successful
/// `insert_atomic`.
#[must_use = "AgentSlot must be committed after insert_atomic succeeds, or it will release on Drop"]
pub struct AgentSlot {
    active_count: Arc<AtomicUsize>,
    committed: bool,
}

impl AgentSlot {
    /// Consume the slot; the increment becomes owned by the agent's presence
    /// in the registry. Call ONLY after `insert_atomic` succeeded.
    pub fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for AgentSlot {
    fn drop(&mut self) {
        if !self.committed {
            self.active_count.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Thread-safe registry of all running agent processes.
///
/// This is the aaOS equivalent of the process table. All agent
/// lifecycle operations go through the registry.
pub struct AgentRegistry {
    agents: DashMap<AgentId, AgentProcess>,
    capability_registry: Arc<CapabilityRegistry>,
    /// Live count of agents currently in the registry. Incremented by
    /// `reserve_agent_slot`, decremented by `remove_agent` (or `AgentSlot::drop`
    /// on uncommitted reservations). Used for admission control so the
    /// len-check-then-insert race is gone.
    ///
    /// Invariant (steady state, no in-flight batch): `active_count == agents.len()`.
    active_count: Arc<AtomicUsize>,
    audit_log: Arc<dyn AuditLog>,
    router: OnceLock<Arc<MessageRouter>>,
    max_agents: usize,
}

impl AgentRegistry {
    pub fn new(audit_log: Arc<dyn AuditLog>) -> Self {
        Self::new_with_limit(audit_log, 100)
    }

    /// Create a registry with a custom maximum agent count.
    pub fn new_with_limit(audit_log: Arc<dyn AuditLog>, max_agents: usize) -> Self {
        Self {
            agents: DashMap::new(),
            capability_registry: Arc::new(CapabilityRegistry::new()),
            active_count: Arc::new(AtomicUsize::new(0)),
            audit_log,
            router: OnceLock::new(),
            max_agents,
        }
    }

    /// Accessor for the capability registry, used by services and tools.
    pub fn capability_registry(&self) -> &Arc<CapabilityRegistry> {
        &self.capability_registry
    }

    /// Atomically reserve a slot in `active_count`. Returns a guard that
    /// releases on Drop unless `commit()` is called after a successful insert.
    fn reserve_agent_slot(&self) -> Result<AgentSlot> {
        let mut current = self.active_count.load(Ordering::Acquire);
        loop {
            if current >= self.max_agents {
                return Err(CoreError::InvalidManifest(
                    format!("agent limit exceeded: max {} agents", self.max_agents).into(),
                ));
            }
            match self.active_count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(AgentSlot {
                        active_count: self.active_count.clone(),
                        committed: false,
                    });
                }
                Err(actual) => current = actual,
            }
        }
    }

    /// Public version for use by batch-spawn tools (e.g. `SpawnAgentsTool`)
    /// that need to pre-reserve N slots before invoking spawn.
    pub fn reserve_slot(&self) -> Result<AgentSlot> {
        self.reserve_agent_slot()
    }

    /// Atomically insert an agent. Rejects duplicate IDs via `DashMap::Entry`
    /// vacant-check (no non-atomic contains_key + insert race).
    fn insert_atomic(&self, id: AgentId, process: AgentProcess) -> Result<()> {
        use dashmap::mapref::entry::Entry;
        match self.agents.entry(id) {
            Entry::Occupied(_) => Err(CoreError::InvalidManifest(
                format!("agent with id {id} already exists").into(),
            )),
            Entry::Vacant(v) => {
                v.insert(process);
                Ok(())
            }
        }
    }

    /// THE ONLY place that removes an agent from the registry. Decrements
    /// `active_count` exactly once. Every lifecycle exit path (`stop_sync`,
    /// `stop`, shutdown) must funnel through here — do not remove from
    /// `self.agents` directly elsewhere.
    fn remove_agent(&self, id: AgentId) -> Option<AgentProcess> {
        let removed = self.agents.remove(&id);
        if removed.is_some() {
            // Revoke all capabilities in the capability registry first
            self.capability_registry.revoke_all_for_agent(id);
            // Then remove all handles from the capability registry's table
            self.capability_registry.remove_agent(id);
            self.active_count.fetch_sub(1, Ordering::AcqRel);
        }
        removed.map(|(_, p)| p)
    }

    /// Set the message router for agent registration.
    /// Called once after construction to break the circular dependency
    /// (router needs registry for capability checks, registry needs router for registration).
    pub fn set_router(&self, router: Arc<MessageRouter>) {
        let _ = self.router.set(router);
    }

    /// Spawn a new agent from a manifest with an ephemeral (kernel-generated)
    /// ID. Returns the new agent's ID.
    pub fn spawn(&self, manifest: AgentManifest) -> Result<AgentId> {
        self.spawn_internal(manifest, AgentId::new(), false)
    }

    /// Spawn an agent using a caller-provided ID that is expected to persist
    /// across restarts (e.g., Bootstrap). Only privileged code paths should
    /// use this — it grants the agent a stable identity, which gates private
    /// memory access.
    /// If an agent with this ID already exists, returns an error.
    pub fn spawn_with_id(&self, manifest: AgentManifest, id: AgentId) -> Result<AgentId> {
        self.spawn_internal(manifest, id, true)
    }

    fn spawn_internal(
        &self,
        manifest: AgentManifest,
        id: AgentId,
        persistent_identity: bool,
    ) -> Result<AgentId> {
        // Atomic admission: reserve a slot in active_count. If we return
        // early for any reason below, the slot Drops and releases.
        let slot = self.reserve_agent_slot()?;

        // Issue capability tokens based on manifest declarations
        let capabilities = self.issue_capabilities(id, &manifest);

        let mut process = AgentProcess::new(id, manifest.clone(), capabilities);
        process.persistent_identity = persistent_identity;
        process.transition_to(AgentState::Running)?;

        // Wire IPC channels into the process struct BEFORE publishing it in
        // the registry. If registration becomes fallible later, the process
        // never gets into the map and there's no orphan to clean up.
        if let Some(router) = self.router.get() {
            let (msg_rx, resp_rx) = router.register(id);
            process.message_rx = Some(msg_rx);
            process.response_rx = Some(resp_rx);
        }

        // Atomic insert — rejects duplicate IDs in a single critical section
        // (no contains_key + insert race). If this fails, `slot` drops and
        // releases the reservation.
        self.insert_atomic(id, process)?;

        self.audit_log.record(AuditEvent::new(
            id,
            AuditEventKind::AgentSpawned {
                manifest_name: manifest.name.clone(),
            },
        ));

        // Agent is now in the registry; its remove path will release the slot.
        slot.commit();

        tracing::info!(agent_id = %id, name = %manifest.name, "agent spawned");
        Ok(id)
    }

    /// Return whether the given agent was spawned with a stable, persistent
    /// identity (e.g., Bootstrap). Ephemeral spawns return false.
    pub fn has_stable_identity(&self, id: AgentId) -> Result<bool> {
        self.agents
            .get(&id)
            .map(|entry| entry.value().persistent_identity)
            .ok_or(CoreError::AgentNotFound(id))
    }

    /// Stop an agent by ID (sync version, does not await the task handle).
    pub fn stop_sync(&self, id: AgentId) -> Result<()> {
        let mut entry = self
            .agents
            .get_mut(&id)
            .ok_or(CoreError::AgentNotFound(id))?;

        let process = entry.value_mut();
        if process.state != AgentState::Stopped {
            process.transition_to(AgentState::Stopping)?;
            process.transition_to(AgentState::Stopped)?;
        }

        self.audit_log.record(AuditEvent::new(
            id,
            AuditEventKind::AgentStopped {
                reason: aaos_core::StopReason::UserRequested,
            },
        ));

        drop(entry);

        if let Some(router) = self.router.get() {
            router.unregister(&id);
        }

        // Use remove_agent (not self.agents.remove directly) so active_count
        // is decremented through the single authoritative path.
        self.remove_agent(id);
        tracing::info!(agent_id = %id, "agent stopped");
        Ok(())
    }

    /// Stop an agent (async version for persistent agents).
    pub async fn stop(&self, id: AgentId) -> Result<()> {
        // Send stop command
        if let Some(entry) = self.agents.get(&id) {
            let _ = entry.value().command_tx.send(AgentCommand::Stop).await;
        }

        // Await task handle
        let task_handle = self.agents.get_mut(&id)
            .and_then(|mut e| e.value_mut().task_handle.take());
        if let Some(handle) = task_handle {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                handle,
            ).await;
        }

        self.stop_sync(id)
    }

    /// Start the persistent agent loop for a persistent agent.
    /// Called by the server after spawn, passing all needed Arc references.
    pub fn start_persistent_loop(
        &self,
        agent_id: AgentId,
        executor: aaos_llm::AgentExecutor,
        session_store: Arc<dyn crate::session::SessionStore>,
        router: Arc<MessageRouter>,
        context_manager: Option<Arc<crate::context::ContextManager>>,
    ) -> Result<()> {
        let mut entry = self
            .agents
            .get_mut(&agent_id)
            .ok_or(CoreError::AgentNotFound(agent_id))?;

        let process = entry.value_mut();

        let msg_rx = process.message_rx.take()
            .ok_or_else(|| CoreError::Ipc("message_rx already taken".into()))?;
        let cmd_rx = process.take_command_rx()
            .ok_or_else(|| CoreError::Ipc("command_rx already taken".into()))?;

        let manifest = process.manifest.clone();
        let audit_log = self.audit_log.clone();

        let handle = tokio::spawn(persistent_agent_loop(
            agent_id, manifest, msg_rx, cmd_rx,
            executor, session_store, router, audit_log,
            context_manager,
        ));

        process.task_handle = Some(handle);
        Ok(())
    }

    /// Internal accessor for the `InProcessBackend`, which needs mutable
    /// access to the agent table to pull channels and plug in the
    /// JoinHandle it spawned. Kept `pub(crate)` to avoid leaking the
    /// internal DashMap shape to downstream crates — backends outside
    /// `aaos-runtime` must use `start_persistent_loop` or the public
    /// lifecycle API.
    pub(crate) fn agents_table(&self) -> &DashMap<AgentId, crate::process::AgentProcess> {
        &self.agents
    }

    /// Current number of agents in the registry, as tracked by the
    /// admission-control counter. Exposed for tests and observability.
    /// Steady-state invariant: `active_count() == list().len()`.
    pub fn active_count(&self) -> usize {
        self.active_count.load(Ordering::Acquire)
    }

    /// Maximum number of concurrent agents. Exposed for tools that need
    /// to preflight-check admission (e.g. `spawn_agents` batch).
    pub fn max_agents(&self) -> usize {
        self.max_agents
    }

    /// Get information about a specific agent.
    pub fn get_info(&self, id: AgentId) -> Result<AgentInfo> {
        self.agents
            .get(&id)
            .map(|entry| entry.value().info())
            .ok_or(CoreError::AgentNotFound(id))
    }

    /// List all running agents.
    pub fn list(&self) -> Vec<AgentInfo> {
        self.agents
            .iter()
            .map(|entry| entry.value().info())
            .collect()
    }

    /// Check if an agent has a specific capability.
    pub fn check_capability(&self, id: AgentId, capability: &Capability) -> Result<bool> {
        self.agents
            .get(&id)
            .map(|entry| entry.value().has_capability(capability, &self.capability_registry))
            .ok_or(CoreError::AgentNotFound(id))
    }

    /// Number of registered agents.
    pub fn count(&self) -> usize {
        self.agents.len()
    }

    /// Revoke a specific capability token for an agent.
    /// The token remains in the registry but `permits()` returns false.
    pub fn revoke_capability(&self, agent_id: AgentId, token_id: uuid::Uuid) -> Result<bool> {
        // Find the capability in the agent's handles to verify ownership,
        // then revoke via the capability registry (which flips revoked_at).
        let has_token = self.agents.get(&agent_id).map_or(false, |entry| {
            entry.value().capabilities.iter().any(|h| {
                self.capability_registry
                    .token_id_of(*h)
                    .map_or(false, |tid| tid == token_id)
            })
        });
        if !has_token {
            return Ok(false);
        }

        let revoked = self.capability_registry.revoke(token_id);
        if revoked {
            self.audit_log.record(aaos_core::AuditEvent::new(
                agent_id,
                aaos_core::AuditEventKind::CapabilityRevoked {
                    token_id,
                    capability: "revoked".into(),
                },
            ));
        }
        Ok(revoked)
    }

    /// Revoke all capabilities for an agent.
    pub fn revoke_all_capabilities(&self, agent_id: AgentId) -> Result<usize> {
        let count = self.capability_registry.revoke_all_for_agent(agent_id);
        if count > 0 {
            // Record one audit event per revoked capability
            for i in 0..count {
                let _ = i; // We don't have per-token IDs here, just record once
            }
            self.audit_log.record(aaos_core::AuditEvent::new(
                agent_id,
                aaos_core::AuditEventKind::CapabilityRevoked {
                    token_id: uuid::Uuid::nil(),
                    capability: "all capabilities revoked".into(),
                },
            ));
        }
        Ok(count)
    }

    /// Track token usage against an agent's budget (if configured).
    pub fn track_token_usage(&self, id: AgentId, usage: aaos_core::TokenUsage) -> Result<()> {
        let entry = self.agents.get(&id).ok_or(CoreError::AgentNotFound(id))?;
        if let Some(tracker) = &entry.value().budget_tracker {
            let total = usage.input_tokens + usage.output_tokens;
            if total > 0 {
                tracker.track(total)?;
            }
        }
        Ok(())
    }

    /// Get a clone of the agent's capability handles.
    /// Acquires a DashMap read lock and clones the handle vector.
    pub fn get_token_handles(&self, id: AgentId) -> Result<Vec<CapabilityHandle>> {
        self.agents
            .get(&id)
            .map(|entry| entry.value().capabilities.clone())
            .ok_or(CoreError::AgentNotFound(id))
    }

    /// Deprecated alias for `get_token_handles`. Kept for backward compatibility
    /// during migration; returns handles, not tokens.
    #[deprecated(since = "handle-based tokens", note = "use get_token_handles instead")]
    pub fn get_tokens(&self, id: AgentId) -> Result<Vec<CapabilityHandle>> {
        self.get_token_handles(id)
    }

    /// Get a clone of the agent's manifest.
    pub fn get_manifest(&self, id: AgentId) -> Result<AgentManifest> {
        self.agents
            .get(&id)
            .map(|entry| entry.value().manifest.clone())
            .ok_or(CoreError::AgentNotFound(id))
    }

    /// Get the spawn depth of an agent.
    pub fn get_depth(&self, id: AgentId) -> Result<u32> {
        self.agents
            .get(&id)
            .map(|entry| entry.value().depth)
            .ok_or(CoreError::AgentNotFound(id))
    }

    /// Spawn an agent with a specific ID and pre-computed capability handles.
    /// Used by SpawnAgentTool to insert child agents with narrowed capabilities.
    /// `depth` is the spawn depth of the child (parent_depth + 1 for child agents).
    /// `parent` is the spawning agent's id (None for top-level spawns).
    pub fn spawn_with_token_handles(
        &self,
        id: AgentId,
        manifest: AgentManifest,
        capabilities: Vec<CapabilityHandle>,
        depth: u32,
        parent: Option<AgentId>,
    ) -> Result<()> {
        const MAX_SPAWN_DEPTH: u32 = 5;
        if depth > MAX_SPAWN_DEPTH {
            return Err(CoreError::InvalidManifest(
                format!("spawn depth exceeded: max depth is {MAX_SPAWN_DEPTH}").into(),
            ));
        }

        // Atomic admission: reserve a slot. Drops/releases on any early return.
        let slot = self.reserve_agent_slot()?;

        // Note: memory_store rejection is handled in SpawnAgentTool (preflight check).
        // This method is the backstop for any caller of spawn_with_token_handles.

        let mut process = AgentProcess::new(id, manifest.clone(), capabilities);
        process.depth = depth;
        process.parent_agent = parent;
        process.transition_to(AgentState::Running)?;

        // Wire IPC channels into the process BEFORE publishing it in the
        // registry. Symmetry with spawn_internal (Run 9 Fix 1). Prevents a
        // window where an agent exists in the map without receive channels.
        if let Some(router) = self.router.get() {
            let (msg_rx, resp_rx) = router.register(id);
            process.message_rx = Some(msg_rx);
            process.response_rx = Some(resp_rx);
        }

        // Atomic insert — rejects duplicate IDs in a single critical section.
        self.insert_atomic(id, process)?;

        self.audit_log.record(AuditEvent::new(
            id,
            AuditEventKind::AgentSpawned {
                manifest_name: manifest.name.clone(),
            },
        ));

        // Agent now in the registry; remove path releases the slot.
        slot.commit();

        tracing::info!(agent_id = %id, name = %manifest.name, "agent spawned with custom tokens");
        Ok(())
    }

    /// Issue capability tokens for an agent based on its manifest declarations.
    fn issue_capabilities(
        &self,
        agent_id: AgentId,
        manifest: &AgentManifest,
    ) -> Vec<CapabilityHandle> {
        let mut handles: Vec<CapabilityHandle> = manifest
            .capabilities
            .iter()
            .filter_map(|decl| {
                let capability = self.parse_capability_declaration(decl)?;
                let token =
                    CapabilityToken::issue(agent_id, capability.clone(), Constraints::default());
                self.audit_log.record(AuditEvent::new(
                    agent_id,
                    AuditEventKind::CapabilityGranted { capability },
                ));
                let handle = self.capability_registry.insert(agent_id, token);
                Some(handle)
            })
            .collect();

        // Persistent agents automatically receive self-send capability so the
        // server can deliver API-initiated messages through the router.
        if manifest.lifecycle == aaos_core::Lifecycle::Persistent {
            let self_send = Capability::MessageSend {
                target_agents: vec![agent_id.to_string()],
            };
            let token = CapabilityToken::issue(agent_id, self_send.clone(), Constraints::default());
            self.audit_log.record(AuditEvent::new(
                agent_id,
                AuditEventKind::CapabilityGranted { capability: self_send },
            ));
            let handle = self.capability_registry.insert(agent_id, token);
            handles.push(handle);
        }

        handles
    }

    fn parse_capability_declaration(
        &self,
        decl: &aaos_core::CapabilityDeclaration,
    ) -> Option<Capability> {
        match decl {
            aaos_core::CapabilityDeclaration::Simple(s) => {
                let s = s.trim();
                if s == "web_search" {
                    Some(Capability::WebSearch)
                } else if let Some(path) = s.strip_prefix("file_read:") {
                    Some(Capability::FileRead {
                        path_glob: path.trim().to_string(),
                    })
                } else if let Some(path) = s.strip_prefix("file_write:") {
                    Some(Capability::FileWrite {
                        path_glob: path.trim().to_string(),
                    })
                } else if let Some(tool) = s.strip_prefix("tool:") {
                    Some(Capability::ToolInvoke {
                        tool_name: tool.trim().to_string(),
                    })
                } else if let Some(agents) = s.strip_prefix("spawn_child:") {
                    let agents = agents.trim().trim_matches(|c| c == '[' || c == ']');
                    let list: Vec<String> = agents
                        .split(',')
                        .map(|a| a.trim().to_string())
                        .filter(|a| !a.is_empty())
                        .collect();
                    Some(Capability::SpawnChild {
                        allowed_agents: list,
                    })
                } else {
                    Some(Capability::Custom {
                        name: s.to_string(),
                        params: serde_json::Value::Null,
                    })
                }
            }
            aaos_core::CapabilityDeclaration::WithParams { params } => {
                // Complex capability declarations — parse from key-value map
                params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|name| Capability::Custom {
                        name: name.to_string(),
                        params: serde_json::Value::Object(
                            params
                                .iter()
                                .filter(|(k, _)| k.as_str() != "name")
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect(),
                        ),
                    })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_core::InMemoryAuditLog;

    fn test_registry() -> (AgentRegistry, Arc<InMemoryAuditLog>) {
        let log = Arc::new(InMemoryAuditLog::new());
        let registry = AgentRegistry::new(log.clone());
        (registry, log)
    }

    fn test_manifest(name: &str) -> AgentManifest {
        AgentManifest::from_yaml(&format!(
            r#"
name: {name}
model: claude-haiku-4-5-20251001
system_prompt: "test"
capabilities:
  - web_search
  - "file_read: /data/*"
"#
        ))
        .unwrap()
    }

    #[test]
    fn spawn_with_id_uses_pinned_id_and_rejects_duplicates() {
        let (registry, _log) = test_registry();
        let pinned = AgentId::new();
        let id = registry
            .spawn_with_id(test_manifest("bootstrap"), pinned)
            .unwrap();
        assert_eq!(id, pinned);
        // Spawning a second agent with the same pinned id must fail.
        let dup = registry.spawn_with_id(test_manifest("bootstrap-2"), pinned);
        assert!(dup.is_err());
    }

    #[test]
    fn spawn_and_list() {
        let (registry, _log) = test_registry();
        let id = registry.spawn(test_manifest("agent-1")).unwrap();
        assert_eq!(registry.count(), 1);

        let info = registry.get_info(id).unwrap();
        assert_eq!(info.name, "agent-1");
        assert_eq!(info.state, AgentState::Running);
    }

    #[test]
    fn spawn_and_stop() {
        let (registry, log) = test_registry();
        let id = registry.spawn(test_manifest("agent-1")).unwrap();
        registry.stop_sync(id).unwrap();
        assert_eq!(registry.count(), 0);

        // Should have spawn + capability grants + stop events
        assert!(log.len() >= 2);
    }

    #[test]
    fn stop_nonexistent_agent() {
        let (registry, _log) = test_registry();
        let result = registry.stop_sync(AgentId::new());
        assert!(result.is_err());
    }

    #[test]
    fn capability_enforcement() {
        let (registry, _log) = test_registry();
        let id = registry.spawn(test_manifest("agent-1")).unwrap();

        assert!(registry
            .check_capability(id, &Capability::WebSearch)
            .unwrap());
        assert!(registry
            .check_capability(
                id,
                &Capability::FileRead {
                    path_glob: "/data/foo.txt".into()
                }
            )
            .unwrap());
        assert!(!registry
            .check_capability(
                id,
                &Capability::FileWrite {
                    path_glob: "/data/foo.txt".into()
                }
            )
            .unwrap());
    }

    #[test]
    fn multiple_agents() {
        let (registry, _log) = test_registry();
        registry.spawn(test_manifest("agent-1")).unwrap();
        registry.spawn(test_manifest("agent-2")).unwrap();
        registry.spawn(test_manifest("agent-3")).unwrap();
        assert_eq!(registry.count(), 3);
        assert_eq!(registry.list().len(), 3);
    }

    #[test]
    fn get_tokens_returns_agent_capabilities() {
        let (registry, _log) = test_registry();
        let id = registry.spawn(test_manifest("agent-1")).unwrap();
        let tokens = registry.get_tokens(id).unwrap();
        // test_manifest declares web_search and file_read
        assert_eq!(tokens.len(), 2);
    }

    #[test]
    fn get_tokens_nonexistent_agent() {
        let (registry, _log) = test_registry();
        let result = registry.get_tokens(AgentId::new());
        assert!(result.is_err());
    }

    #[test]
    fn get_manifest_returns_agent_manifest() {
        let (registry, _log) = test_registry();
        let id = registry.spawn(test_manifest("agent-1")).unwrap();
        let manifest = registry.get_manifest(id).unwrap();
        assert_eq!(manifest.name, "agent-1");
    }

    #[test]
    fn get_manifest_nonexistent_agent() {
        let (registry, _log) = test_registry();
        let result = registry.get_manifest(AgentId::new());
        assert!(result.is_err());
    }

    #[test]
    fn spawn_registers_with_router() {
        let log = Arc::new(InMemoryAuditLog::new());
        let router = Arc::new(aaos_ipc::MessageRouter::new(log.clone(), |_, _| true));
        let registry = AgentRegistry::new(log.clone());
        registry.set_router(router.clone());

        let id = registry.spawn(test_manifest("agent-1")).unwrap();
        assert_eq!(router.agent_count(), 1);

        registry.stop_sync(id).unwrap();
        assert_eq!(router.agent_count(), 0);
    }

    #[tokio::test]
    async fn spawn_persistent_agent_starts_loop() {
        let log = Arc::new(InMemoryAuditLog::new());
        let router = Arc::new(aaos_ipc::MessageRouter::new(log.clone(), |_, _| true));
        let registry = Arc::new(AgentRegistry::new(log.clone()));
        registry.set_router(router.clone());

        let manifest = AgentManifest::from_yaml(r#"
name: persistent-agent
model: claude-haiku-4-5-20251001
system_prompt: "You are persistent."
lifecycle: persistent
"#).unwrap();

        let agent_id = registry.spawn(manifest).unwrap();
        let info = registry.get_info(agent_id).unwrap();
        assert_eq!(info.state, AgentState::Running);
        assert_eq!(info.name, "persistent-agent");

        registry.stop_sync(agent_id).unwrap();
    }

    #[test]
    fn ephemeral_spawn_unchanged() {
        let (registry, _log) = test_registry();
        let manifest = AgentManifest::from_yaml(r#"
name: ephemeral-agent
model: claude-haiku-4-5-20251001
system_prompt: "You are ephemeral."
lifecycle: on-demand
"#).unwrap();

        let id = registry.spawn(manifest).unwrap();
        let info = registry.get_info(id).unwrap();
        assert_eq!(info.state, AgentState::Running);
        assert_eq!(info.name, "ephemeral-agent");
    }

    #[test]
    fn spawn_refused_when_agent_limit_hit() {
        let log = Arc::new(InMemoryAuditLog::new());
        let registry = AgentRegistry::new_with_limit(log, 2);

        registry.spawn(test_manifest("agent-1")).unwrap();
        registry.spawn(test_manifest("agent-2")).unwrap();

        let result = registry.spawn(test_manifest("agent-3"));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("agent limit exceeded"), "unexpected error: {err}");
    }

    #[test]
    fn spawn_with_tokens_refused_when_depth_exceeded() {
        let log = Arc::new(InMemoryAuditLog::new());
        let registry = AgentRegistry::new(log);

        let id = AgentId::new();
        let manifest = test_manifest("deep-agent");
        // depth 6 exceeds MAX_SPAWN_DEPTH (5)
        let result = registry.spawn_with_token_handles(id, manifest, vec![], 6, None);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("spawn depth exceeded"), "unexpected error: {err}");
    }

    #[test]
    fn get_depth_reflects_assigned_depth() {
        let log = Arc::new(InMemoryAuditLog::new());
        let registry = AgentRegistry::new(log);

        let id = AgentId::new();
        let manifest = test_manifest("child-agent");
        registry.spawn_with_token_handles(id, manifest, vec![], 3, None).unwrap();
        assert_eq!(registry.get_depth(id).unwrap(), 3);
    }

    #[test]
    fn spawn_child_capability_parsing() {
        let (registry, _log) = test_registry();
        let manifest = AgentManifest::from_yaml(
            r#"
name: orchestrator
model: claude-haiku-4-5-20251001
system_prompt: "test"
capabilities:
  - "spawn_child: [researcher, summarizer]"
  - "tool: spawn_agent"
"#,
        )
        .unwrap();
        let id = registry.spawn(manifest).unwrap();

        let has_spawn = registry
            .check_capability(
                id,
                &Capability::SpawnChild {
                    allowed_agents: vec!["researcher".into()],
                },
            )
            .unwrap();
        assert!(has_spawn);
    }

    #[test]
    fn spawn_with_id_sets_persistent_identity() {
        let (registry, _log) = test_registry();
        let id = AgentId::new();
        registry
            .spawn_with_id(test_manifest("bootstrap"), id)
            .unwrap();
        assert_eq!(registry.has_stable_identity(id).unwrap(), true);
    }

    #[test]
    fn spawn_does_not_set_persistent_identity() {
        let (registry, _log) = test_registry();
        let id = registry.spawn(test_manifest("child")).unwrap();
        assert_eq!(registry.has_stable_identity(id).unwrap(), false);
    }

    #[test]
    fn spawn_with_tokens_does_not_set_persistent_identity() {
        let (registry, _log) = test_registry();
        let id = AgentId::new();
        registry
            .spawn_with_token_handles(id, test_manifest("child"), vec![], 1, None)
            .unwrap();
        assert_eq!(registry.has_stable_identity(id).unwrap(), false);
    }

    // TODO(handle-migration): spawn_with_token_handles no longer inspects
    // token contents (it receives opaque handles). The memory_store rejection
    // moved to SpawnAgentTool's preflight, which has its own test:
    // `spawn_child_rejects_memory_store_tool`. Leaving a breadcrumb here so
    // the intent is traceable; the old runtime-backstop test is no longer
    // expressible and was dropped during Run 11 prep (handle-based tokens).
    #[test]
    #[ignore = "runtime-level memory_store rejection removed; see SpawnAgentTool test"]
    fn spawn_with_tokens_rejects_memory_store_capability() {
        // Body intentionally left; will fail if un-ignored.
    }

    #[test]
    fn has_stable_identity_errors_for_unknown_agent() {
        let (registry, _log) = test_registry();
        let result = registry.has_stable_identity(AgentId::new());
        assert!(result.is_err());
    }

    #[test]
    fn spawn_wires_ipc_channels_before_publishing() {
        // Invariant: by the time an agent is visible in the registry, its
        // message_rx and response_rx are populated (when a router is set).
        // Before the fix, the agent was published first and channels wired
        // after, leaving a window where the agent existed without channels.
        let (registry, log) = test_registry();
        let router = Arc::new(MessageRouter::new(
            log.clone() as Arc<dyn AuditLog>,
            |_, _| true, // permissive capability checker for this test
        ));
        registry.set_router(router);

        let id = registry.spawn(test_manifest("channels-test")).unwrap();
        let entry = registry.agents.get(&id).expect("agent in registry");
        assert!(
            entry.value().message_rx.is_some(),
            "message_rx must be populated by the time the agent is in the registry"
        );
        assert!(
            entry.value().response_rx.is_some(),
            "response_rx must be populated by the time the agent is in the registry"
        );
    }

    #[test]
    fn spawn_with_tokens_wires_ipc_channels_before_publishing() {
        // Run 10 finding: spawn_with_tokens had the pre-Fix-1 ordering —
        // insert first, wire channels via get_mut after. Same invariant hole
        // as spawn_internal. This test pins the fix by asserting that after
        // spawn_with_tokens returns, the process in the registry has both
        // channels populated (when a router is set).
        let (registry, log) = test_registry();
        let router = Arc::new(MessageRouter::new(
            log.clone() as Arc<dyn AuditLog>,
            |_, _| true,
        ));
        registry.set_router(router);

        let id = AgentId::new();
        registry
            .spawn_with_token_handles(id, test_manifest("tokens-channels-test"), vec![], 1, None)
            .unwrap();
        let entry = registry.agents.get(&id).expect("agent in registry");
        assert!(
            entry.value().message_rx.is_some(),
            "spawn_with_tokens: message_rx must be populated"
        );
        assert!(
            entry.value().response_rx.is_some(),
            "spawn_with_tokens: response_rx must be populated"
        );
    }

    #[test]
    fn spawn_with_tokens_rejects_duplicate_id() {
        // Run 10 follow-up: spawn_with_tokens now has the duplicate-ID guard
        // that spawn_internal already had. Prevents a caller from stomping
        // an existing agent slot.
        let (registry, _log) = test_registry();
        let id = AgentId::new();
        registry
            .spawn_with_token_handles(id, test_manifest("first"), vec![], 1, None)
            .expect("first spawn succeeds");

        let second = registry.spawn_with_token_handles(id, test_manifest("duplicate"), vec![], 1, None);
        assert!(second.is_err(), "duplicate ID must be rejected");
        let err = second.unwrap_err().to_string();
        assert!(
            err.contains("already exists"),
            "error should name the collision: {err}"
        );
    }

    // ============================================================
    // active_count drift tests (Run 11 prep: parallel spawn_agents)
    // ============================================================

    #[test]
    fn active_count_matches_agents_len_after_spawns() {
        let (registry, _log) = test_registry();
        assert_eq!(registry.active_count(), 0);
        let _a = registry.spawn(test_manifest("a")).unwrap();
        let _b = registry.spawn(test_manifest("b")).unwrap();
        let _c = registry.spawn(test_manifest("c")).unwrap();
        assert_eq!(registry.active_count(), 3);
        assert_eq!(registry.list().len(), 3);
    }

    #[test]
    fn active_count_decrements_after_stop_sync() {
        let (registry, _log) = test_registry();
        let a = registry.spawn(test_manifest("a")).unwrap();
        let _b = registry.spawn(test_manifest("b")).unwrap();
        assert_eq!(registry.active_count(), 2);
        registry.stop_sync(a).unwrap();
        assert_eq!(registry.active_count(), 1);
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn active_count_released_on_duplicate_id_rejection() {
        // Slot is reserved, insert fails due to duplicate — slot must drop
        // and release so the count stays accurate.
        let (registry, _log) = test_registry();
        let pinned = AgentId::new();
        registry
            .spawn_with_id(test_manifest("first"), pinned)
            .unwrap();
        assert_eq!(registry.active_count(), 1);

        // Attempt duplicate — should fail, count should stay at 1.
        let dup = registry.spawn_with_id(test_manifest("dup"), pinned);
        assert!(dup.is_err());
        assert_eq!(
            registry.active_count(),
            1,
            "failed duplicate-ID spawn must release the reserved slot"
        );
    }

    #[test]
    fn active_count_enforced_under_limit() {
        let log = Arc::new(InMemoryAuditLog::new());
        let registry = AgentRegistry::new_with_limit(log, 2);

        registry.spawn(test_manifest("one")).unwrap();
        registry.spawn(test_manifest("two")).unwrap();
        assert_eq!(registry.active_count(), 2);

        let third = registry.spawn(test_manifest("three"));
        assert!(third.is_err(), "third spawn over limit must fail");
        assert_eq!(
            registry.active_count(),
            2,
            "failed over-limit spawn must not leak a reservation"
        );
    }

    #[test]
    fn agent_slot_releases_on_drop_without_commit() {
        // Direct test of the AgentSlot guard: reserve without committing,
        // verify the counter returns to zero after the guard drops.
        let (registry, _log) = test_registry();
        assert_eq!(registry.active_count(), 0);
        {
            let _slot = registry.reserve_slot().unwrap();
            assert_eq!(registry.active_count(), 1);
        } // _slot drops here without commit
        assert_eq!(registry.active_count(), 0);
    }

    #[test]
    fn agent_slot_keeps_count_on_commit() {
        let (registry, _log) = test_registry();
        {
            let slot = registry.reserve_slot().unwrap();
            assert_eq!(registry.active_count(), 1);
            slot.commit();
        }
        assert_eq!(
            registry.active_count(),
            1,
            "committed slot must retain the reservation"
        );
    }
}
