use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent_id::AgentId;
use crate::error::Result;
use crate::tool_definition::ToolDefinition;

/// Token usage from a single LLM call or accumulated across a run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// Result of a human approval request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalResult {
    Approved,
    Denied { reason: String },
    Timeout,
}

/// Uniform interface for kernel services provided to agents.
///
/// Both internal agents (running inside agentd) and future external agents
/// (connecting via Unix socket) use this same interface. The in-process
/// implementation goes through the same capability checks and audit logging
/// that the socket implementation will.
#[async_trait]
pub trait AgentServices: Send + Sync {
    /// Invoke a tool on behalf of an agent, with full capability enforcement and audit logging.
    ///
    /// Tokens are looked up by agent_id from the registry, not passed per-call.
    /// This ensures checks are always against current state (revoked tokens fail immediately).
    ///
    /// NOTE: A future `invoke_tool_with_scope` variant may be needed for delegated
    /// invocations, where agent A invokes a tool on behalf of agent B with a restricted
    /// subset of capabilities. Not needed until orchestration layer (Phase 04).
    async fn invoke_tool(&self, agent_id: AgentId, tool: &str, input: Value) -> Result<Value>;

    /// Send a structured message to another agent.
    /// The message Value must be a valid MCP message envelope (JSON-RPC 2.0 with metadata).
    /// The implementation deserializes and routes it via the MessageRouter.
    ///
    /// Agent-to-agent messaging is deferred for Phase A. This exists on the trait to
    /// establish the interface for Phase B external agents.
    async fn send_message(&self, agent_id: AgentId, message: Value) -> Result<Value>;

    /// Request human approval. Blocks until approved, denied, or timeout.
    /// Semantically distinct from send_message — approval has blocking semantics
    /// with explicit timeout behavior.
    async fn request_approval(
        &self,
        agent_id: AgentId,
        description: String,
        timeout: Duration,
    ) -> Result<ApprovalResult>;

    /// Report token usage for cost tracking and budget enforcement.
    async fn report_usage(&self, agent_id: AgentId, usage: TokenUsage) -> Result<()>;

    /// List tools available to this agent (filtered by capabilities).
    /// Returns only tools the agent has capability tokens for.
    ///
    /// This is the PRIMARY mechanism for scoping tool access — the LLM never sees tools
    /// the agent can't use. Filtering at the schema level improves LLM tool selection
    /// performance. Capability enforcement at invocation time is the safety net.
    async fn list_tools(&self, agent_id: AgentId) -> Result<Vec<ToolDefinition>>;
}

/// Service for requesting human approval before sensitive actions.
#[async_trait]
pub trait ApprovalService: Send + Sync {
    async fn request(
        &self,
        agent_id: AgentId,
        agent_name: String,
        description: String,
        tool: Option<String>,
        input: Option<Value>,
    ) -> Result<ApprovalResult>;
}

/// Default approval service that auto-approves everything.
pub struct NoOpApprovalService;

#[async_trait]
impl ApprovalService for NoOpApprovalService {
    async fn request(
        &self,
        _agent_id: AgentId,
        _agent_name: String,
        _description: String,
        _tool: Option<String>,
        _input: Option<Value>,
    ) -> Result<ApprovalResult> {
        Ok(ApprovalResult::Approved)
    }
}
