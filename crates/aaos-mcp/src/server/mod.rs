pub mod handlers;
pub mod sse;

use aaos_core::{AgentId, AuditEvent};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::broadcast;

/// What the MCP server needs from `agentd`. Implemented by `agentd::Server`
/// under the `mcp` feature, and by a mock in tests.
#[async_trait]
pub trait McpServerBackend: Send + Sync {
    /// Submit a goal. Returns an `AgentId` representing the bootstrap agent.
    async fn submit_goal(
        &self,
        goal: String,
        role: Option<String>,
    ) -> anyhow::Result<AgentId>;

    /// Current status of a run.
    fn run_status(&self, agent_id: &AgentId) -> RunStatus;

    /// Cancel a running agent. Returns false if not found.
    async fn cancel(&self, agent_id: &AgentId) -> bool;

    /// Subscribe to the audit broadcast stream.
    fn subscribe_audit(&self) -> broadcast::Receiver<AuditEvent>;
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Running,
    Completed { result: Option<String> },
    Failed { error: String },
    NotFound,
}

pub struct McpServer {
    backend: Arc<dyn McpServerBackend>,
    bind: String,
}

impl McpServer {
    pub fn new(backend: Arc<dyn McpServerBackend>, bind: String) -> Self {
        Self { backend, bind }
    }

    pub async fn start(self) -> anyhow::Result<()> {
        use axum::{
            routing::{get, post},
            Router,
        };
        use std::net::SocketAddr;

        let backend = self.backend.clone();
        let app = Router::new()
            .route("/mcp", post(handlers::handle_jsonrpc))
            .route("/mcp/events", get(handlers::handle_sse))
            .with_state(backend);

        let addr: SocketAddr = self.bind.parse()?;
        tracing::info!("MCP server listening on {addr}");
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app).await?;
        Ok(())
    }
}
