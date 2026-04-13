use std::sync::{Arc, OnceLock};

use aaos_core::{
    AgentId, AgentManifest, AuditEvent, AuditEventKind, AuditLog, Capability, CapabilityToken,
    Constraints, CoreError, Result,
};
use aaos_ipc::MessageRouter;
use dashmap::DashMap;

use crate::persistent::persistent_agent_loop;
use crate::process::{AgentCommand, AgentInfo, AgentProcess, AgentState};

/// Thread-safe registry of all running agent processes.
///
/// This is the aaOS equivalent of the process table. All agent
/// lifecycle operations go through the registry.
pub struct AgentRegistry {
    agents: DashMap<AgentId, AgentProcess>,
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
            audit_log,
            router: OnceLock::new(),
            max_agents,
        }
    }

    /// Set the message router for agent registration.
    /// Called once after construction to break the circular dependency
    /// (router needs registry for capability checks, registry needs router for registration).
    pub fn set_router(&self, router: Arc<MessageRouter>) {
        let _ = self.router.set(router);
    }

    /// Spawn a new agent from a manifest. Returns the new agent's ID.
    pub fn spawn(&self, manifest: AgentManifest) -> Result<AgentId> {
        if self.agents.len() >= self.max_agents {
            return Err(CoreError::InvalidManifest(
                format!("agent limit exceeded: max {} agents", self.max_agents).into(),
            ));
        }

        let id = AgentId::new();

        // Issue capability tokens based on manifest declarations
        let capabilities = self.issue_capabilities(id, &manifest);

        let mut process = AgentProcess::new(id, manifest.clone(), capabilities);
        process.transition_to(AgentState::Running)?;

        self.audit_log.record(AuditEvent::new(
            id,
            AuditEventKind::AgentSpawned {
                manifest_name: manifest.name.clone(),
            },
        ));

        self.agents.insert(id, process);

        if let Some(router) = self.router.get() {
            let (msg_rx, resp_rx) = router.register(id);
            if let Some(mut entry) = self.agents.get_mut(&id) {
                entry.value_mut().message_rx = Some(msg_rx);
                entry.value_mut().response_rx = Some(resp_rx);
            }
        }

        tracing::info!(agent_id = %id, name = %manifest.name, "agent spawned");
        Ok(id)
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

        self.agents.remove(&id);
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
            .map(|entry| entry.value().has_capability(capability))
            .ok_or(CoreError::AgentNotFound(id))
    }

    /// Number of registered agents.
    pub fn count(&self) -> usize {
        self.agents.len()
    }

    /// Get a clone of the agent's capability tokens.
    /// Acquires a DashMap read lock and clones the token vector.
    pub fn get_tokens(&self, id: AgentId) -> Result<Vec<CapabilityToken>> {
        self.agents
            .get(&id)
            .map(|entry| entry.value().capabilities.clone())
            .ok_or(CoreError::AgentNotFound(id))
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

    /// Spawn an agent with a specific ID and pre-computed capability tokens.
    /// Used by SpawnAgentTool to insert child agents with narrowed capabilities.
    /// `depth` is the spawn depth of the child (parent_depth + 1 for child agents).
    pub fn spawn_with_tokens(
        &self,
        id: AgentId,
        manifest: AgentManifest,
        capabilities: Vec<CapabilityToken>,
        depth: u32,
    ) -> Result<()> {
        const MAX_SPAWN_DEPTH: u32 = 5;
        if depth > MAX_SPAWN_DEPTH {
            return Err(CoreError::InvalidManifest(
                format!("spawn depth exceeded: max depth is {MAX_SPAWN_DEPTH}").into(),
            ));
        }

        if self.agents.len() >= self.max_agents {
            return Err(CoreError::InvalidManifest(
                format!("agent limit exceeded: max {} agents", self.max_agents).into(),
            ));
        }

        let mut process = AgentProcess::new(id, manifest.clone(), capabilities);
        process.depth = depth;
        process.transition_to(AgentState::Running)?;

        self.audit_log.record(AuditEvent::new(
            id,
            AuditEventKind::AgentSpawned {
                manifest_name: manifest.name.clone(),
            },
        ));

        self.agents.insert(id, process);

        if let Some(router) = self.router.get() {
            let (msg_rx, resp_rx) = router.register(id);
            if let Some(mut entry) = self.agents.get_mut(&id) {
                entry.value_mut().message_rx = Some(msg_rx);
                entry.value_mut().response_rx = Some(resp_rx);
            }
        }

        tracing::info!(agent_id = %id, name = %manifest.name, "agent spawned with custom tokens");
        Ok(())
    }

    /// Issue capability tokens for an agent based on its manifest declarations.
    fn issue_capabilities(
        &self,
        agent_id: AgentId,
        manifest: &AgentManifest,
    ) -> Vec<CapabilityToken> {
        let mut tokens: Vec<CapabilityToken> = manifest
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
                Some(token)
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
            tokens.push(token);
        }

        tokens
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
        let result = registry.spawn_with_tokens(id, manifest, vec![], 6);
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
        registry.spawn_with_tokens(id, manifest, vec![], 3).unwrap();
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
}
