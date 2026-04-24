use aaos_core::{AgentId, AuditEvent};
use aaos_mcp::server::{McpServerBackend, RunStatus};
use aaos_runtime::AgentState;
use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::orchestration_classifier::DecompositionMode;
use crate::server::{inline_direct_plan, Server};

#[async_trait]
impl McpServerBackend for Server {
    /// Submit a goal by running it through the unified orchestration path
    /// (classifier → Planner+PlanExecutor for `Decompose`, inline 1-node
    /// plan for `Direct`). Returns a run-handle `AgentId` derived from the
    /// run UUID; the background task drives execution and emits audit
    /// events observable via `subscribe_audit`.
    ///
    /// Note: the returned `AgentId` is a synthetic run handle, not a
    /// registry-resident agent, so `run_status` will report `NotFound`
    /// until a real per-subtask agent id appears in the audit stream.
    /// Callers that need fine-grained run tracking should use the
    /// `agent.submit_streaming` JSON-RPC method instead.
    async fn submit_goal(&self, goal: String, role: Option<String>) -> anyhow::Result<AgentId> {
        if let Some(r) = &role {
            tracing::warn!(
                role = %r,
                "role parameter in submit_goal is not yet supported — routing through the classifier-driven unified path"
            );
        }

        let plan_executor =
            self.plan_executor.get().cloned().ok_or_else(|| {
                anyhow::anyhow!("plan executor not installed — role catalog missing")
            })?;

        let mode = self.classifier.classify(&goal).await;
        let run_id = uuid::Uuid::new_v4();

        // Spawn in background — MCP clients poll via run_status / subscribe_audit.
        let planner = self.planner.clone();
        tokio::spawn(async move {
            let outcome = match mode {
                DecompositionMode::Decompose => plan_executor.run(&goal, run_id).await,
                DecompositionMode::Direct => {
                    let plan = inline_direct_plan(&goal, run_id);
                    plan_executor.run_with_plan(plan, &goal, run_id).await
                }
            };
            if let Err(e) = outcome {
                tracing::error!(error = %e, run_id = %run_id, "mcp_backend submit_goal run failed");
            }
            // Suppress unused-variable warning when mcp feature pulls in the
            // planner reference chain but this branch doesn't consume it.
            let _ = planner;
        });

        // Synthesize an AgentId from the run UUID. This is a handle, not a
        // registry entry; run_status will report NotFound for it, which is
        // acceptable for the current MCP-backend use case.
        Ok(AgentId::from_uuid(run_id))
    }

    /// Map the registry's `AgentState` → `RunStatus`.
    /// `NotFound` if the agent is not in the registry.
    fn run_status(&self, agent_id: &AgentId) -> RunStatus {
        match self.registry.get_info(*agent_id) {
            Err(_) => RunStatus::NotFound,
            Ok(info) => match info.state {
                AgentState::Starting | AgentState::Running | AgentState::Paused => {
                    RunStatus::Running
                }
                AgentState::Stopping | AgentState::Stopped => RunStatus::Completed { result: None },
            },
        }
    }

    /// Cancel a running agent. Returns `false` if the agent was not found.
    async fn cancel(&self, agent_id: &AgentId) -> bool {
        self.registry.stop_sync(*agent_id).is_ok()
    }

    /// Subscribe to the audit broadcast stream so callers can observe live events.
    fn subscribe_audit(&self) -> broadcast::Receiver<AuditEvent> {
        self.broadcast_audit.subscribe()
    }
}
