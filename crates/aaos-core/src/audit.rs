use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent_id::AgentId;
use crate::capability::Capability;

/// Reason an agent was stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Completed,
    UserRequested,
    Error(String),
    CapabilityRevoked,
    Timeout,
}

/// The kind of event that occurred in the system.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuditEventKind {
    AgentSpawned {
        manifest_name: String,
    },
    AgentStopped {
        reason: StopReason,
    },
    CapabilityGranted {
        capability: Capability,
    },
    CapabilityDenied {
        capability: Capability,
        reason: String,
    },
    ToolInvoked {
        tool: String,
        input_hash: String,
    },
    ToolResult {
        tool: String,
        success: bool,
    },
    MessageSent {
        from: AgentId,
        to: AgentId,
    },
    MessageDelivered {
        from: AgentId,
        to: AgentId,
    },
    HumanApprovalRequested {
        description: String,
    },
    HumanApprovalGranted,
    HumanApprovalDenied {
        reason: String,
    },
    UsageReported {
        input_tokens: u64,
        output_tokens: u64,
    },
    AgentExecutionStarted {
        message_preview: String,
    },
    AgentExecutionCompleted {
        stop_reason: String,
        total_iterations: u32,
    },
    AgentLoopStarted {
        lifecycle: String,
    },
    AgentLoopStopped {
        reason: String,
        messages_processed: u64,
    },
    AgentMessageReceived {
        trace_id: Uuid,
        method: String,
    },
}

/// A single entry in the system-wide audit trail.
///
/// Every action in aaOS produces an audit event. This is a kernel
/// guarantee, not an application-level concern. You can always answer:
/// what happened, why, and what did it cost.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub agent_id: AgentId,
    pub event: AuditEventKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_event: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<Uuid>,
}

impl AuditEvent {
    /// Create a new audit event.
    pub fn new(agent_id: AgentId, event: AuditEventKind) -> Self {
        Self {
            id: Uuid::new_v4(),
            timestamp: Utc::now(),
            agent_id,
            event,
            parent_event: None,
            trace_id: None,
        }
    }

    /// Set the parent event for causal tracing.
    pub fn with_parent(mut self, parent: Uuid) -> Self {
        self.parent_event = Some(parent);
        self
    }

    /// Set the trace ID for request-level tracing.
    pub fn with_trace(mut self, trace: Uuid) -> Self {
        self.trace_id = Some(trace);
        self
    }
}

/// Trait for audit event sinks.
pub trait AuditLog: Send + Sync {
    fn record(&self, event: AuditEvent);
}

/// In-memory audit log for testing.
#[derive(Debug, Default)]
pub struct InMemoryAuditLog {
    events: std::sync::Mutex<Vec<AuditEvent>>,
}

impl InMemoryAuditLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn events(&self) -> Vec<AuditEvent> {
        self.events.lock().unwrap().clone()
    }

    pub fn len(&self) -> usize {
        self.events.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl AuditLog for InMemoryAuditLog {
    fn record(&self, event: AuditEvent) {
        self.events.lock().unwrap().push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_event_creation() {
        let agent = AgentId::new();
        let event = AuditEvent::new(
            agent,
            AuditEventKind::AgentSpawned {
                manifest_name: "test".into(),
            },
        );
        assert_eq!(event.agent_id, agent);
        assert!(event.parent_event.is_none());
    }

    #[test]
    fn in_memory_audit_log() {
        let log = InMemoryAuditLog::new();
        let agent = AgentId::new();
        log.record(AuditEvent::new(
            agent,
            AuditEventKind::AgentSpawned {
                manifest_name: "a".into(),
            },
        ));
        log.record(AuditEvent::new(
            agent,
            AuditEventKind::AgentStopped {
                reason: StopReason::Completed,
            },
        ));
        assert_eq!(log.len(), 2);
    }

    #[test]
    fn audit_event_roundtrips_json() {
        let event = AuditEvent::new(
            AgentId::new(),
            AuditEventKind::ToolInvoked {
                tool: "web_search".into(),
                input_hash: "abc123".into(),
            },
        );
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AuditEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.id, parsed.id);
    }

    #[test]
    fn usage_reported_event_roundtrips_json() {
        let event = AuditEvent::new(
            AgentId::new(),
            AuditEventKind::UsageReported {
                input_tokens: 1500,
                output_tokens: 300,
            },
        );
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AuditEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.id, parsed.id);
    }

    #[test]
    fn execution_started_event_roundtrips_json() {
        let event = AuditEvent::new(
            AgentId::new(),
            AuditEventKind::AgentExecutionStarted {
                message_preview: "Analyze this data...".into(),
            },
        );
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AuditEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.id, parsed.id);
    }

    #[test]
    fn execution_completed_event_roundtrips_json() {
        let event = AuditEvent::new(
            AgentId::new(),
            AuditEventKind::AgentExecutionCompleted {
                stop_reason: "complete".into(),
                total_iterations: 3,
            },
        );
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AuditEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.id, parsed.id);
    }

    #[test]
    fn agent_loop_started_event_roundtrips_json() {
        let event = AuditEvent::new(
            AgentId::new(),
            AuditEventKind::AgentLoopStarted {
                lifecycle: "persistent".into(),
            },
        );
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AuditEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.id, parsed.id);
    }

    #[test]
    fn agent_loop_stopped_event_roundtrips_json() {
        let event = AuditEvent::new(
            AgentId::new(),
            AuditEventKind::AgentLoopStopped {
                reason: "user_requested".into(),
                messages_processed: 42,
            },
        );
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AuditEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.id, parsed.id);
    }

    #[test]
    fn agent_message_received_event_roundtrips_json() {
        let event = AuditEvent::new(
            AgentId::new(),
            AuditEventKind::AgentMessageReceived {
                trace_id: Uuid::new_v4(),
                method: "agent.run".into(),
            },
        );
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AuditEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.id, parsed.id);
    }
}
