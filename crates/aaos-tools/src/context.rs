use aaos_core::{AgentId, CapabilityToken};

/// Context passed to a tool during invocation.
/// Contains the invoking agent's ID and capability tokens
/// relevant to this tool (pre-filtered by ToolInvocation).
pub struct InvocationContext {
    pub agent_id: AgentId,
    pub tokens: Vec<CapabilityToken>,
}
