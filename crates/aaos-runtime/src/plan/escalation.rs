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
