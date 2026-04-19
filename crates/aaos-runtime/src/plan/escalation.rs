//! Escalation signal taxonomy for dynamic model routing.
//!
//! The plan executor watches for three observable signals during a subtask's
//! execution. When a subtask fails AND a configured signal fired, the
//! executor bumps the subtask's model tier for the next replan attempt.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationSignal {
    ReplanRetry,
    ToolRepeatGuard,
    MaxTokens,
}

impl EscalationSignal {
    /// Machine-readable reason string used in SubtaskModelEscalated audit
    /// events. Stable — downstream log consumers may match on these.
    pub fn reason(&self) -> &'static str {
        match self {
            EscalationSignal::ReplanRetry => "replan_retry",
            EscalationSignal::ToolRepeatGuard => "tool_repeat_guard",
            EscalationSignal::MaxTokens => "max_tokens",
        }
    }
}

pub fn default_escalation_signals() -> Vec<EscalationSignal> {
    vec![
        EscalationSignal::ReplanRetry,
        EscalationSignal::ToolRepeatGuard,
        EscalationSignal::MaxTokens,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_case_serde_roundtrip() {
        for s in [
            EscalationSignal::ReplanRetry,
            EscalationSignal::ToolRepeatGuard,
            EscalationSignal::MaxTokens,
        ] {
            let j = serde_json::to_string(&s).unwrap();
            let expected = format!("\"{}\"", s.reason());
            assert_eq!(j, expected, "serialize must use snake_case reason string");
            let back: EscalationSignal = serde_json::from_str(&j).unwrap();
            assert_eq!(back, s);
        }
    }

    #[test]
    fn unknown_signal_is_an_error() {
        let r: Result<EscalationSignal, _> = serde_json::from_str("\"ragequit\"");
        assert!(r.is_err(), "unknown signal strings must error, not default");
    }

    #[test]
    fn default_set_contains_all_three() {
        let defaults = default_escalation_signals();
        assert_eq!(defaults.len(), 3);
        assert!(defaults.contains(&EscalationSignal::ReplanRetry));
        assert!(defaults.contains(&EscalationSignal::ToolRepeatGuard));
        assert!(defaults.contains(&EscalationSignal::MaxTokens));
    }
}

use crate::plan::Plan;
use aaos_core::{AuditEvent, AuditEventKind};

/// Information carried from a failed execution attempt back to the replan
/// loop so it can decide whether to escalate each failed subtask's model
/// tier. Attached to `ExecutorError::Correctable` instead of a free-form
/// reason string when replan-eligible failures are the cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedSubtask {
    pub subtask_id: String,
    pub role: String,
    /// What the executor saw for this subtask during the failed attempt.
    pub observed_signals: Vec<EscalationSignal>,
}

/// Pure function: given a failed subtask's context and the subtask's role
/// configuration, decide which escalation signal (if any) should trigger
/// a tier bump. Returns the highest-priority configured signal that
/// actually fired — priority order is ReplanRetry > MaxTokens >
/// ToolRepeatGuard (failure-first heuristic).
pub fn decide_escalation(
    failed: &FailedSubtask,
    configured: &[EscalationSignal],
    ladder_len: usize,
    current_tier: u8,
) -> Option<EscalationSignal> {
    if ladder_len <= 1 {
        return None;
    }
    if (current_tier as usize) >= ladder_len - 1 {
        return None;
    }
    // Fixed priority for deterministic escalation-event emission: whichever
    // signal best explains the failure wins.
    for candidate in &[
        EscalationSignal::ReplanRetry,
        EscalationSignal::MaxTokens,
        EscalationSignal::ToolRepeatGuard,
    ] {
        if configured.contains(candidate) && failed.observed_signals.contains(candidate) {
            return Some(*candidate);
        }
    }
    None
}

/// Scan an audit-event slice for per-subtask signals. Used by the executor
/// after a failed batch to populate `FailedSubtask::observed_signals`.
pub fn signals_for_subtask(
    subtask_id: &str,
    subtask_agent_ids: &[aaos_core::AgentId],
    events: &[AuditEvent],
) -> Vec<EscalationSignal> {
    let mut out = Vec::new();
    for ev in events {
        match &ev.event {
            AuditEventKind::SubtaskCompleted {
                subtask_id: sid,
                success: false,
            } if sid == subtask_id => {
                if !out.contains(&EscalationSignal::ReplanRetry) {
                    out.push(EscalationSignal::ReplanRetry);
                }
            }
            AuditEventKind::ToolRepeatGuardFired { agent_id, .. }
                if subtask_agent_ids.contains(agent_id) =>
            {
                if !out.contains(&EscalationSignal::ToolRepeatGuard) {
                    out.push(EscalationSignal::ToolRepeatGuard);
                }
            }
            AuditEventKind::AgentExecutionCompleted { stop_reason, .. }
                if stop_reason == "MaxTokens" && subtask_agent_ids.contains(&ev.agent_id) =>
            {
                if !out.contains(&EscalationSignal::MaxTokens) {
                    out.push(EscalationSignal::MaxTokens);
                }
            }
            _ => {}
        }
    }
    out
}

/// After a replan produces a new plan, carry forward the escalated
/// `current_model_tier` from the old plan by matching subtask ids. Subtasks
/// in the new plan whose ids are NOT in the old plan keep tier 0. Any
/// `failed_tier_bumps` map takes precedence over the carryover — that's
/// where `decide_escalation`'s increment is applied.
pub fn carry_tiers_forward(
    new_plan: &mut Plan,
    old_plan: &Plan,
    failed_tier_bumps: &std::collections::HashMap<String, u8>,
) {
    use std::collections::HashMap;
    let old_tiers: HashMap<&str, u8> = old_plan
        .subtasks
        .iter()
        .map(|s| (s.id.as_str(), s.current_model_tier))
        .collect();
    for s in new_plan.subtasks.iter_mut() {
        if let Some(&bumped) = failed_tier_bumps.get(&s.id) {
            s.current_model_tier = bumped;
        } else if let Some(&prev) = old_tiers.get(s.id.as_str()) {
            s.current_model_tier = prev;
        }
    }
}

#[cfg(test)]
mod decide_tests {
    use super::*;

    #[test]
    fn ladder_too_short_returns_none() {
        let f = FailedSubtask {
            subtask_id: "a".into(),
            role: "writer".into(),
            observed_signals: vec![EscalationSignal::ReplanRetry],
        };
        assert_eq!(
            decide_escalation(&f, &default_escalation_signals(), 1, 0),
            None
        );
    }

    #[test]
    fn top_of_ladder_returns_none() {
        let f = FailedSubtask {
            subtask_id: "a".into(),
            role: "writer".into(),
            observed_signals: vec![EscalationSignal::ReplanRetry],
        };
        // Ladder len 2, already at tier 1 (top).
        assert_eq!(
            decide_escalation(&f, &default_escalation_signals(), 2, 1),
            None
        );
    }

    #[test]
    fn replan_retry_wins_over_tool_repeat_when_both_fired() {
        let f = FailedSubtask {
            subtask_id: "a".into(),
            role: "writer".into(),
            observed_signals: vec![
                EscalationSignal::ToolRepeatGuard,
                EscalationSignal::ReplanRetry,
            ],
        };
        assert_eq!(
            decide_escalation(&f, &default_escalation_signals(), 2, 0),
            Some(EscalationSignal::ReplanRetry)
        );
    }

    #[test]
    fn configured_off_signal_does_not_fire() {
        let f = FailedSubtask {
            subtask_id: "a".into(),
            role: "writer".into(),
            observed_signals: vec![EscalationSignal::ReplanRetry],
        };
        // Only MaxTokens is configured; ReplanRetry not listed.
        assert_eq!(
            decide_escalation(&f, &[EscalationSignal::MaxTokens], 2, 0),
            None
        );
    }

    #[test]
    fn carry_tiers_forward_preserves_survivors_and_applies_bumps() {
        use crate::plan::{Plan, Subtask};
        let old = Plan {
            subtasks: vec![
                Subtask {
                    id: "a".into(),
                    role: "writer".into(),
                    params: serde_json::json!({}),
                    depends_on: vec![],
                    ttl: None,
                    current_model_tier: 1, // was tier 1
                },
                Subtask {
                    id: "b".into(),
                    role: "writer".into(),
                    params: serde_json::json!({}),
                    depends_on: vec![],
                    ttl: None,
                    current_model_tier: 0,
                },
            ],
            final_output: "a".into(),
        };
        let mut new_plan = Plan {
            subtasks: vec![
                Subtask {
                    id: "a".into(), // survives
                    role: "writer".into(),
                    params: serde_json::json!({}),
                    depends_on: vec![],
                    ttl: None,
                    current_model_tier: 0,
                },
                Subtask {
                    id: "b".into(), // survives, bumped
                    role: "writer".into(),
                    params: serde_json::json!({}),
                    depends_on: vec![],
                    ttl: None,
                    current_model_tier: 0,
                },
                Subtask {
                    id: "c".into(), // brand new — stays at tier 0
                    role: "writer".into(),
                    params: serde_json::json!({}),
                    depends_on: vec![],
                    ttl: None,
                    current_model_tier: 0,
                },
            ],
            final_output: "c".into(),
        };

        let mut bumps = std::collections::HashMap::new();
        bumps.insert("b".to_string(), 1u8);

        carry_tiers_forward(&mut new_plan, &old, &bumps);

        // "a" inherits its previous tier (1)
        assert_eq!(new_plan.subtasks[0].current_model_tier, 1);
        // "b" applies the bump (1), ignoring its previous tier
        assert_eq!(new_plan.subtasks[1].current_model_tier, 1);
        // "c" is brand new — stays at tier 0
        assert_eq!(new_plan.subtasks[2].current_model_tier, 0);
    }
}
