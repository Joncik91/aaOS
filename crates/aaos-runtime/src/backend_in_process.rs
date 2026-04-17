//! In-process `AgentBackend` implementation.
//!
//! Wraps today's `tokio::spawn(persistent_agent_loop)` path behind the
//! trait introduced in `aaos_core::backend`. Semantically identical to
//! the pre-refactor code in `AgentRegistry::start_persistent_loop` —
//! same channels, same executor construction, same audit events — but
//! now fronted by the `AgentBackend` interface.
//!
//! This is commit 1 of `plans/2026-04-15-namespaced-backend-v4.md`. A
//! later commit adds `aaos-backend-linux::NamespacedBackend`, which
//! plugs into the same trait without touching this file.

use std::sync::Arc;

use aaos_core::{
    AgentBackend, AgentLaunchHandle, AgentLaunchSpec, AuditLog, BackendHealth, CoreError, Result,
    TokenBudget,
};
use aaos_ipc::MessageRouter;
use aaos_llm::{AgentExecutor, ExecutorConfig, LlmClient};
use async_trait::async_trait;
use dashmap::DashMap;
use tokio::task::AbortHandle;

use crate::context::ContextManager;
use crate::registry::AgentRegistry;
use crate::session::SessionStore;

/// Opaque state carried by `AgentLaunchHandle` for this backend.
///
/// Holds an `AbortHandle` so `stop()` can cancel the spawned task.
/// The full `JoinHandle` is kept in `AgentProcess::task_handle` by
/// `AgentRegistry` (unchanged from the pre-refactor behavior) so
/// existing stop paths continue to `await` the task. Keeping the
/// handles in both places is deliberate: it preserves the exact
/// lifecycle sequencing the test suite verifies while letting the
/// backend trait offer an independent `stop`/`health` surface for
/// future callers.
#[derive(Debug)]
struct InProcessState {
    abort: AbortHandle,
}

/// Configuration knobs for `InProcessBackend` that aren't per-launch.
/// Exposed mainly so tests can dial down the summarization threshold
/// without smuggling it through a manifest.
#[derive(Debug, Clone)]
pub struct InProcessBackendConfig {
    /// Default model name used for summarization when the manifest
    /// does not set one. Matches the value the server used inline
    /// prior to the refactor.
    pub default_summarization_model: String,
    /// Default ratio (0..=1) of the context window at which
    /// summarization kicks in.
    pub default_summarization_threshold: f32,
}

impl Default for InProcessBackendConfig {
    fn default() -> Self {
        Self {
            default_summarization_model: "claude-haiku-4-5-20251001".to_string(),
            default_summarization_threshold: 0.7,
        }
    }
}

/// `AgentBackend` that runs every agent as a `tokio` task in the
/// `agentd` process. This is the default backend and the only one
/// available before commit 2 lands `NamespacedBackend`.
pub struct InProcessBackend {
    registry: Arc<AgentRegistry>,
    session_store: Arc<dyn SessionStore>,
    router: Arc<MessageRouter>,
    audit_log: Arc<dyn AuditLog>,
    llm_client: Option<Arc<dyn LlmClient>>,
    services_builder: Arc<dyn Fn() -> Arc<dyn aaos_core::AgentServices> + Send + Sync>,
    config: InProcessBackendConfig,
    /// Tracks live tasks so `stop`/`health` have something to consult
    /// independent of the registry's `AgentProcess` entry. The value is
    /// the task's `AbortHandle`; the full `JoinHandle` still lives on
    /// `AgentProcess::task_handle` and is awaited through the existing
    /// `AgentRegistry::stop` path.
    abort_handles: DashMap<aaos_core::AgentId, AbortHandle>,
}

impl InProcessBackend {
    /// Construct a backend. All arguments mirror what the server
    /// used to pass to `AgentRegistry::start_persistent_loop` per
    /// call; gathering them here once removes that per-call wiring.
    ///
    /// `services_builder` is a closure that returns a fresh
    /// `AgentServices` for each launched agent. The server builds it
    /// from the same `InProcessAgentServices::new` arguments it uses
    /// today.
    pub fn new(
        registry: Arc<AgentRegistry>,
        session_store: Arc<dyn SessionStore>,
        router: Arc<MessageRouter>,
        audit_log: Arc<dyn AuditLog>,
        llm_client: Option<Arc<dyn LlmClient>>,
        services_builder: Arc<dyn Fn() -> Arc<dyn aaos_core::AgentServices> + Send + Sync>,
    ) -> Self {
        Self::with_config(
            registry,
            session_store,
            router,
            audit_log,
            llm_client,
            services_builder,
            InProcessBackendConfig::default(),
        )
    }

    /// Variant of `new` that accepts explicit configuration.
    pub fn with_config(
        registry: Arc<AgentRegistry>,
        session_store: Arc<dyn SessionStore>,
        router: Arc<MessageRouter>,
        audit_log: Arc<dyn AuditLog>,
        llm_client: Option<Arc<dyn LlmClient>>,
        services_builder: Arc<dyn Fn() -> Arc<dyn aaos_core::AgentServices> + Send + Sync>,
        config: InProcessBackendConfig,
    ) -> Self {
        Self {
            registry,
            session_store,
            router,
            audit_log,
            llm_client,
            services_builder,
            config,
            abort_handles: DashMap::new(),
        }
    }
}

#[async_trait]
impl AgentBackend for InProcessBackend {
    async fn launch(&self, spec: AgentLaunchSpec) -> Result<AgentLaunchHandle> {
        let llm = self.llm_client.as_ref().ok_or_else(|| {
            CoreError::Ipc("InProcessBackend::launch: no LLM client configured".into())
        })?;

        let agent_id = spec.agent_id;
        let manifest = spec.manifest;

        // Pull the message/command channels off the AgentProcess the
        // registry already created. This mirrors the pre-refactor
        // `start_persistent_loop` exactly — the backend doesn't mint
        // a new process entry, it plugs a task into the one the
        // registry is already holding.
        let (msg_rx, cmd_rx) = {
            let mut entry = self
                .registry
                .agents_table()
                .get_mut(&agent_id)
                .ok_or(CoreError::AgentNotFound(agent_id))?;
            let process = entry.value_mut();
            let msg_rx = process
                .message_rx
                .take()
                .ok_or_else(|| CoreError::Ipc("message_rx already taken".into()))?;
            let cmd_rx = process
                .take_command_rx()
                .ok_or_else(|| CoreError::Ipc("command_rx already taken".into()))?;
            (msg_rx, cmd_rx)
        };

        // Build the executor and optional ContextManager the same way
        // the server used to inline in `spawn_from_yaml_with_id`.
        let services = (self.services_builder)();
        let executor = AgentExecutor::new(llm.clone(), services, ExecutorConfig::default());

        let summarization_model = manifest
            .memory
            .summarization_model
            .clone()
            .unwrap_or_else(|| self.config.default_summarization_model.clone());
        let threshold = manifest
            .memory
            .summarization_threshold
            .unwrap_or(self.config.default_summarization_threshold);
        let model_max = llm.max_context_tokens(&manifest.model);
        let budget = TokenBudget::from_config(&manifest.memory.context_window, model_max)?;
        let context_manager = Some(Arc::new(ContextManager::new(
            llm.clone(),
            budget,
            summarization_model,
            threshold,
        )));

        // Spawn the same persistent loop the registry used to spawn.
        let join = tokio::spawn(crate::persistent::persistent_agent_loop(
            agent_id,
            manifest,
            msg_rx,
            cmd_rx,
            executor,
            self.session_store.clone(),
            self.router.clone(),
            self.audit_log.clone(),
            context_manager,
        ));

        let abort = join.abort_handle();

        // Plug the JoinHandle back into the registry so `stop()` /
        // `stop_sync()` keep working exactly as they did. This is the
        // "no behavior change" invariant — only the task-spawning
        // code moved, not the lifecycle bookkeeping.
        {
            let mut entry = self
                .registry
                .agents_table()
                .get_mut(&agent_id)
                .ok_or(CoreError::AgentNotFound(agent_id))?;
            entry.value_mut().task_handle = Some(join);
        }

        self.abort_handles.insert(agent_id, abort.clone());

        Ok(AgentLaunchHandle::new(
            agent_id,
            "in_process",
            InProcessState { abort },
        ))
    }

    async fn stop(&self, handle: &AgentLaunchHandle) -> Result<()> {
        // Idempotent: if the agent already exited (or was never
        // launched via this backend), return Ok.
        if let Some((_, abort)) = self.abort_handles.remove(&handle.agent_id) {
            abort.abort();
        } else if let Some(state) = handle.state::<InProcessState>() {
            // Handle came in but the map entry was already drained;
            // still safe to abort a second time.
            state.abort.abort();
        }
        Ok(())
    }

    async fn health(&self, handle: &AgentLaunchHandle) -> BackendHealth {
        // Fast path: the abort-handle map is the source of truth for
        // "backend still tracks this agent". If the handle knows an
        // abort that hasn't finished, call it Healthy.
        if let Some(entry) = self.abort_handles.get(&handle.agent_id) {
            if entry.value().is_finished() {
                return BackendHealth::Exited(0);
            }
            return BackendHealth::Healthy;
        }
        if let Some(state) = handle.state::<InProcessState>() {
            if state.abort.is_finished() {
                return BackendHealth::Exited(0);
            }
        }
        BackendHealth::Unknown("no tracking entry".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use aaos_core::{
        AgentId, AgentManifest, AgentServices, ApprovalResult, ApprovalService, InMemoryAuditLog,
        NoOpApprovalService, TokenUsage, ToolDefinition,
    };
    use aaos_ipc::MessageRouter;
    use aaos_llm::{
        CompletionRequest, CompletionResponse, ContentBlock, LlmClient, LlmError, LlmStopReason,
    };
    use aaos_tools::{EchoTool, ToolInvocation, ToolRegistry};
    use async_trait::async_trait;
    use serde_json::Value;

    use crate::services::InProcessAgentServices;
    use crate::session::InMemorySessionStore;

    // ---- Test doubles ----

    struct MockLlm {
        text: String,
    }

    #[async_trait]
    impl LlmClient for MockLlm {
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> std::result::Result<CompletionResponse, LlmError> {
            Ok(CompletionResponse {
                content: vec![ContentBlock::Text {
                    text: self.text.clone(),
                }],
                stop_reason: LlmStopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 5,
                    output_tokens: 5,
                },
            })
        }

        fn max_context_tokens(&self, _model: &str) -> u32 {
            200_000
        }
    }

    fn test_manifest(name: &str) -> AgentManifest {
        AgentManifest::from_yaml(&format!(
            r#"
name: {name}
model: claude-haiku-4-5-20251001
system_prompt: "test"
lifecycle: persistent
"#
        ))
        .unwrap()
    }

    fn build_backend() -> (Arc<InProcessBackend>, Arc<AgentRegistry>) {
        let audit_log: Arc<dyn AuditLog> = Arc::new(InMemoryAuditLog::new());
        let registry = Arc::new(AgentRegistry::new(audit_log.clone()));
        let router = Arc::new(MessageRouter::new(audit_log.clone(), |_, _| true));
        registry.set_router(router.clone());

        let tool_registry = Arc::new(ToolRegistry::new());
        tool_registry.register(Arc::new(EchoTool));
        let tool_invocation = Arc::new(ToolInvocation::new(
            tool_registry.clone(),
            audit_log.clone(),
            registry.capability_registry().clone(),
        ));

        let session_store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
        let llm_client: Arc<dyn LlmClient> = Arc::new(MockLlm {
            text: "Hello back!".into(),
        });

        let registry_for_builder = registry.clone();
        let tool_invocation_for_builder = tool_invocation.clone();
        let tool_registry_for_builder = tool_registry.clone();
        let audit_for_builder = audit_log.clone();
        let router_for_builder = router.clone();
        let services_builder: Arc<dyn Fn() -> Arc<dyn AgentServices> + Send + Sync> =
            Arc::new(move || {
                let approval: Arc<dyn ApprovalService> = Arc::new(NoOpApprovalService);
                Arc::new(InProcessAgentServices::new(
                    registry_for_builder.clone(),
                    tool_invocation_for_builder.clone(),
                    tool_registry_for_builder.clone(),
                    audit_for_builder.clone(),
                    router_for_builder.clone(),
                    approval,
                )) as Arc<dyn AgentServices>
            });

        let backend = Arc::new(InProcessBackend::new(
            registry.clone(),
            session_store,
            router,
            audit_log,
            Some(llm_client),
            services_builder,
        ));

        (backend, registry)
    }

    // ---- Tests ----

    #[tokio::test]
    async fn in_process_backend_launches_agent() {
        let (backend, registry) = build_backend();

        let manifest = test_manifest("launch-test");
        let agent_id = registry.spawn(manifest.clone()).unwrap();
        let caps = registry.get_token_handles(agent_id).unwrap();

        let spec = AgentLaunchSpec {
            agent_id,
            manifest,
            capability_handles: caps,
            workspace_path: std::path::PathBuf::from("/tmp/aaos-test"),
            budget_config: None,
        };

        let handle = backend.launch(spec).await.expect("launch succeeds");
        assert_eq!(handle.agent_id, agent_id);
        assert_eq!(handle.backend_kind, "in_process");

        // Right after launch the task is still alive.
        let health = backend.health(&handle).await;
        assert_eq!(health, BackendHealth::Healthy);

        // Clean up through the registry so the task drains cleanly
        // (mirrors server.rs behavior).
        registry.stop(agent_id).await.unwrap();
    }

    #[tokio::test]
    async fn in_process_backend_stop_is_idempotent() {
        let (backend, registry) = build_backend();

        let manifest = test_manifest("stop-idemp");
        let agent_id = registry.spawn(manifest.clone()).unwrap();
        let caps = registry.get_token_handles(agent_id).unwrap();

        let spec = AgentLaunchSpec {
            agent_id,
            manifest,
            capability_handles: caps,
            workspace_path: std::path::PathBuf::from("/tmp/aaos-test"),
            budget_config: None,
        };

        let handle = backend.launch(spec).await.expect("launch succeeds");

        backend.stop(&handle).await.expect("first stop ok");
        // Second stop must not error even though the abort handle is
        // gone from the internal map.
        backend.stop(&handle).await.expect("second stop ok");

        // After stop, health should no longer report Healthy. It's
        // either Exited(0) (if the task observed the abort) or
        // Unknown (if the map entry is drained).
        let health = backend.health(&handle).await;
        assert_ne!(health, BackendHealth::Healthy);

        // The registry's stop path also must not error after the
        // backend already cancelled the task.
        let _ = registry.stop(agent_id).await;
    }

    #[tokio::test]
    async fn in_process_backend_health_detects_finish() {
        let (backend, registry) = build_backend();

        let manifest = test_manifest("health-test");
        let agent_id = registry.spawn(manifest.clone()).unwrap();
        let caps = registry.get_token_handles(agent_id).unwrap();

        let spec = AgentLaunchSpec {
            agent_id,
            manifest,
            capability_handles: caps,
            workspace_path: std::path::PathBuf::from("/tmp/aaos-test"),
            budget_config: None,
        };

        let handle = backend.launch(spec).await.unwrap();
        backend.stop(&handle).await.unwrap();

        // Give the aborted task a chance to finalize.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // is_finished() flips once the task observes the abort.
        let health = backend.health(&handle).await;
        // Either Exited(0) (mapped from "finished") or Unknown if the
        // tracking entry was already drained — both are acceptable
        // non-Healthy outcomes.
        assert!(
            matches!(health, BackendHealth::Exited(0) | BackendHealth::Unknown(_)),
            "unexpected health variant: {health:?}"
        );

        let _ = registry.stop(agent_id).await;
    }

    #[tokio::test]
    async fn in_process_backend_missing_llm_client_errors() {
        let audit_log: Arc<dyn AuditLog> = Arc::new(InMemoryAuditLog::new());
        let registry = Arc::new(AgentRegistry::new(audit_log.clone()));
        let router = Arc::new(MessageRouter::new(audit_log.clone(), |_, _| true));
        registry.set_router(router.clone());
        let session_store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());

        let services_builder: Arc<dyn Fn() -> Arc<dyn AgentServices> + Send + Sync> =
            Arc::new(|| Arc::new(StubServices) as Arc<dyn AgentServices>);

        let backend = InProcessBackend::new(
            registry.clone(),
            session_store,
            router,
            audit_log,
            None,
            services_builder,
        );

        let manifest = test_manifest("no-llm");
        let agent_id = registry.spawn(manifest.clone()).unwrap();
        let caps = registry.get_token_handles(agent_id).unwrap();

        let spec = AgentLaunchSpec {
            agent_id,
            manifest,
            capability_handles: caps,
            workspace_path: std::path::PathBuf::from("/tmp/aaos-test"),
            budget_config: None,
        };

        let err = backend.launch(spec).await.unwrap_err();
        assert!(err.to_string().contains("no LLM client"));
    }

    /// Minimal AgentServices stub for tests that never invoke tools.
    struct StubServices;

    #[async_trait]
    impl AgentServices for StubServices {
        async fn invoke_tool(
            &self,
            _agent_id: AgentId,
            _tool: &str,
            _input: Value,
        ) -> Result<Value> {
            Ok(Value::Null)
        }
        async fn send_message(&self, _agent_id: AgentId, _message: Value) -> Result<Value> {
            Ok(Value::Null)
        }
        async fn send_and_wait(
            &self,
            _agent_id: AgentId,
            _recipient: AgentId,
            _method: String,
            _params: Value,
            _timeout: Duration,
        ) -> Result<Value> {
            Ok(Value::Null)
        }
        async fn request_approval(
            &self,
            _agent_id: AgentId,
            _description: String,
            _timeout: Duration,
        ) -> Result<ApprovalResult> {
            Ok(ApprovalResult::Approved)
        }
        async fn report_usage(&self, _agent_id: AgentId, _usage: TokenUsage) -> Result<()> {
            Ok(())
        }
        async fn list_tools(&self, _agent_id: AgentId) -> Result<Vec<ToolDefinition>> {
            Ok(vec![])
        }
    }
}
