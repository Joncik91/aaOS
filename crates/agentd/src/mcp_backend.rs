use aaos_core::{AgentId, AuditEvent};
use aaos_mcp::server::{McpServerBackend, RunStatus};
use aaos_runtime::AgentState;
use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::server::Server;

#[async_trait]
impl McpServerBackend for Server {
    /// Submit a goal by ensuring Bootstrap is running and routing the goal to it.
    /// Returns the bootstrap agent's `AgentId` as the run handle.
    async fn submit_goal(
        &self,
        goal: String,
        role: Option<String>,
    ) -> anyhow::Result<AgentId> {
        if let Some(r) = &role {
            tracing::warn!(role = %r, "role parameter in submit_goal is not yet supported — submitting to default bootstrap");
        }
        let bootstrap_id = self.ensure_bootstrap_running().await?;
        self.route_goal_to(bootstrap_id, &goal).await?;
        Ok(bootstrap_id)
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
                AgentState::Stopping | AgentState::Stopped => {
                    RunStatus::Completed { result: None }
                }
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
