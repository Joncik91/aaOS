use std::collections::VecDeque;

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

/// Classification of why context summarization failed.
///
/// Carried on `ContextSummarizationFailed` audit events alongside the free-form
/// `reason` text, so operators can pattern-match on the category without parsing
/// strings. When a new failure mode is added to `prepare_context`, add a variant
/// here and classify the error at its source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummarizationFailureKind {
    /// The summarization LLM call returned an error (network, rate limit, auth, 5xx, etc.).
    LlmCallFailed,
    /// The summarization LLM returned successfully but with empty content.
    EmptyResponse,
    /// No safe boundary for summarization could be selected from the history.
    BoundarySelection,
    /// The LLM reply was malformed or unparseable.
    ReplyParseError,
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
    CapabilityRevoked {
        token_id: Uuid,
        capability: String,
    },
    ToolInvoked {
        tool: String,
        input_hash: String,
        /// Truncated, human-readable preview of the tool input (capped at
        /// 200 bytes). Populated best-effort by the invocation layer;
        /// `None` for callers that emit the event directly without going
        /// through `ToolInvocation`. Kept alongside `input_hash` — the
        /// hash stays authoritative for correlation, the preview is for
        /// operator observability.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args_preview: Option<String>,
    },
    ToolResult {
        tool: String,
        success: bool,
        /// Truncated, human-readable preview of the tool result (capped
        /// at 200 bytes). For error results, the error message. For
        /// success results, a best-effort summary (first fields of a JSON
        /// object, first N chars of a string, byte count for large
        /// responses).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result_preview: Option<String>,
    },
    /// Fired when an agent calls the same `(tool, input_hash)` pair
    /// for the Nth time in a subtask, where N meets or exceeds
    /// AAOS_TOOL_REPEAT_THRESHOLD (default 3). Signals that the
    /// agent may be stuck in a retry loop without recognizing it.
    /// The hint that went back in the tool's result is not repeated
    /// here — this event is for the audit stream, not the LLM.
    ToolRepeat {
        tool: String,
        input_hash: String,
        attempt_count: u32,
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
    ContextSummarized {
        messages_summarized: u32,
        source_range: (usize, usize),
        tokens_saved_estimate: u32,
    },
    ContextSummarizationFailed {
        /// Free-form error message for humans/logs.
        reason: String,
        /// What the runtime did instead (e.g., "original_history", "hard_truncation").
        fallback: String,
        /// Structured classification of the failure for programmatic consumers.
        failure_kind: SummarizationFailureKind,
    },
    MemoryStored {
        memory_id: Uuid,
        category: String,
        content_hash: String,
    },
    MemoryQueried {
        query_hash: String,
        results_count: usize,
    },
    /// A persistent session-store operation failed. The in-memory history is
    /// still intact; this signals that the on-disk copy is stale or at risk
    /// of divergence until the next successful summarization cycle.
    SessionStoreError {
        /// Which operation failed: "clear" or "append". String (not &'static)
        /// so the variant derives Deserialize; callers should pass literals.
        operation: String,
        /// The error message reported by the store.
        message: String,
    },
    PlanProduced {
        subtask_count: u32,
        replans_used: u32,
    },
    PlanReplanned {
        reason: String,
    },
    SubtaskStarted {
        subtask_id: String,
        role: String,
    },
    SubtaskCompleted {
        subtask_id: String,
        success: bool,
    },
    SubtaskTtlExpired {
        subtask_id: String,
        /// Short machine-readable reason: "hops_exhausted" | "wall_clock_exceeded" | "dependency_ttl_cascade".
        reason: String,
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

/// Audit log that writes JSON-lines to stdout.
pub struct StdoutAuditLog;

impl AuditLog for StdoutAuditLog {
    fn record(&self, event: AuditEvent) {
        if let Ok(json) = serde_json::to_string(&event) {
            println!("{json}");
        }
    }
}

/// In-memory audit log for testing. Unbounded by default; opt-in cap via
/// `with_cap()` for long-running test harnesses where unbounded growth would
/// OOM. Uses VecDeque so rollover is O(1) when a cap is set.
#[derive(Debug, Default)]
pub struct InMemoryAuditLog {
    events: std::sync::Mutex<VecDeque<AuditEvent>>,
    max_events: Option<usize>,
}

impl InMemoryAuditLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a capped log. When the cap is reached, the oldest events are
    /// dropped (O(1) via VecDeque::pop_front). Callers must pass `max >= 1`.
    pub fn with_cap(max: usize) -> Self {
        debug_assert!(max >= 1, "InMemoryAuditLog cap must be >= 1");
        Self {
            events: std::sync::Mutex::new(VecDeque::with_capacity(max)),
            max_events: Some(max),
        }
    }

    pub fn events(&self) -> Vec<AuditEvent> {
        self.events.lock().unwrap().iter().cloned().collect()
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
        let mut events = self.events.lock().unwrap();
        if let Some(max) = self.max_events {
            while events.len() >= max {
                events.pop_front();
            }
        }
        events.push_back(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdout_audit_log_does_not_panic() {
        let log = StdoutAuditLog;
        let agent = AgentId::new();
        // Should not panic even if stdout is captured
        log.record(AuditEvent::new(
            agent,
            AuditEventKind::AgentSpawned {
                manifest_name: "stdout-test".into(),
            },
        ));
    }

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
                args_preview: None,
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

    #[test]
    fn context_summarized_event_roundtrips_json() {
        let event = AuditEvent::new(
            AgentId::new(),
            AuditEventKind::ContextSummarized {
                messages_summarized: 20,
                source_range: (0, 19),
                tokens_saved_estimate: 15000,
            },
        );
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AuditEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.id, parsed.id);
    }

    #[test]
    fn context_summarization_failed_event_roundtrips_json() {
        let event = AuditEvent::new(
            AgentId::new(),
            AuditEventKind::ContextSummarizationFailed {
                reason: "LLM timeout".into(),
                fallback: "hard_truncation".into(),
                failure_kind: SummarizationFailureKind::LlmCallFailed,
            },
        );
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AuditEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.id, parsed.id);
    }

    #[test]
    fn memory_stored_event_roundtrips_json() {
        let event = AuditEvent::new(
            AgentId::new(),
            AuditEventKind::MemoryStored {
                memory_id: Uuid::new_v4(),
                category: "fact".into(),
                content_hash: "abc123".into(),
            },
        );
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AuditEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.id, parsed.id);
    }

    #[test]
    fn memory_queried_event_roundtrips_json() {
        let event = AuditEvent::new(
            AgentId::new(),
            AuditEventKind::MemoryQueried {
                query_hash: "def456".into(),
                results_count: 3,
            },
        );
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AuditEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.id, parsed.id);
    }

    #[test]
    fn in_memory_audit_log_capped_drops_oldest() {
        // Fix 6: with_cap() should retain the most recent N events and
        // drop older ones in O(1) via VecDeque::pop_front().
        let log = InMemoryAuditLog::with_cap(3);
        let agent = AgentId::new();
        for i in 0..5u32 {
            log.record(AuditEvent::new(
                agent,
                AuditEventKind::ToolInvoked {
                    tool: format!("tool-{i}"),
                    input_hash: "h".into(),
                    args_preview: None,
                },
            ));
        }
        assert_eq!(log.len(), 3, "should be capped at 3");
        let events = log.events();
        // Newest three tools retained; tool-0 and tool-1 dropped.
        let tools: Vec<&str> = events
            .iter()
            .filter_map(|e| match &e.event {
                AuditEventKind::ToolInvoked { tool, .. } => Some(tool.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(tools, vec!["tool-2", "tool-3", "tool-4"]);
    }

    #[test]
    fn in_memory_audit_log_unbounded_by_default() {
        let log = InMemoryAuditLog::new();
        let agent = AgentId::new();
        for _ in 0..50 {
            log.record(AuditEvent::new(
                agent,
                AuditEventKind::AgentSpawned {
                    manifest_name: "x".into(),
                },
            ));
        }
        assert_eq!(log.len(), 50);
    }

    #[test]
    fn plan_audit_events_round_trip() {
        let events = vec![
            AuditEvent::new(
                AgentId::new(),
                AuditEventKind::PlanProduced {
                    subtask_count: 3,
                    replans_used: 0,
                },
            ),
            AuditEvent::new(
                AgentId::new(),
                AuditEventKind::PlanReplanned {
                    reason: "unknown role".into(),
                },
            ),
            AuditEvent::new(
                AgentId::new(),
                AuditEventKind::SubtaskStarted {
                    subtask_id: "fetch-hn".into(),
                    role: "fetcher".into(),
                },
            ),
            AuditEvent::new(
                AgentId::new(),
                AuditEventKind::SubtaskCompleted {
                    subtask_id: "fetch-hn".into(),
                    success: true,
                },
            ),
        ];
        for e in events {
            let s = serde_json::to_string(&e).unwrap();
            let back: AuditEvent = serde_json::from_str(&s).unwrap();
            let original = serde_json::to_string(&e.event).unwrap();
            let rebuilt = serde_json::to_string(&back.event).unwrap();
            assert_eq!(original, rebuilt);
        }
    }

    #[test]
    fn subtask_ttl_expired_variant_roundtrips() {
        let e = AuditEventKind::SubtaskTtlExpired {
            subtask_id: "s1".into(),
            reason: "wall_clock_exceeded".into(),
        };
        let s = serde_json::to_string(&e).unwrap();
        let back: AuditEventKind = serde_json::from_str(&s).unwrap();
        match back {
            AuditEventKind::SubtaskTtlExpired { subtask_id, reason } => {
                assert_eq!(subtask_id, "s1");
                assert_eq!(reason, "wall_clock_exceeded");
            }
            _ => panic!("wrong variant"),
        }
    }
}
