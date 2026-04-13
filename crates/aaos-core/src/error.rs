use thiserror::Error;

use crate::agent_id::AgentId;
use crate::capability::Capability;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("agent not found: {0}")]
    AgentNotFound(AgentId),

    #[error("capability denied: agent {agent_id} lacks {capability:?}: {reason}")]
    CapabilityDenied {
        agent_id: AgentId,
        capability: Capability,
        reason: String,
    },

    #[error("invalid manifest: {0}")]
    InvalidManifest(String),

    #[error("invalid state transition: {from} -> {to}")]
    InvalidStateTransition { from: String, to: String },

    #[error("tool not found: {0}")]
    ToolNotFound(String),

    #[error("schema validation failed: {0}")]
    SchemaValidation(String),

    #[error("ipc error: {0}")]
    Ipc(String),

    #[error("mailbox full for agent {0}")]
    MailboxFull(AgentId),

    #[error("request timed out after {0:?}")]
    Timeout(std::time::Duration),

    #[error("token budget exceeded: {0}")]
    BudgetExceeded(#[from] crate::budget::BudgetExceeded),

    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, CoreError>;
