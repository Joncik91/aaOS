use std::sync::Arc;

use aaos_core::{AgentId, CapabilityHandle, CapabilityRegistry};

/// Context passed to a tool during invocation.
/// Contains the invoking agent's ID and capability handles
/// relevant to this tool (pre-filtered by ToolInvocation).
pub struct InvocationContext {
    pub agent_id: AgentId,
    pub tokens: Vec<CapabilityHandle>,
    /// Arc because Tool::invoke is async and may outlive the caller's stack;
    /// the registry is runtime-lifetime anyway. Clone is cheap (atomic bump).
    pub capability_registry: Arc<CapabilityRegistry>,
}
