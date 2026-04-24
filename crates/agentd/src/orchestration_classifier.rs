//! Cheap single-shot LLM classifier that picks an [`OrchestrationMode`] from
//! a submitted goal string.
//!
//! ## How it works
//!
//! The production impl (`LlmOrchestrationClassifier`) sends the goal to the
//! configured LLM with a short routing prompt and expects exactly one word
//! back — `plan` or `persistent`.  The prompt is intentionally minimal so
//! the call falls entirely inside the provider's prompt-cache hit window on
//! repeated submits (~50 input tokens, 1 output token).
//!
//! ## Fallback policy
//!
//! Any parse failure or LLM error → [`OrchestrationMode::Plan`].  This keeps
//! the system backwards-compatible: operators who never set an explicit
//! `--orchestration` flag get the same default they had before the classifier
//! was introduced.

use std::sync::Arc;

use async_trait::async_trait;
use tracing::{instrument, warn};

use crate::orchestration::OrchestrationMode;

/// Routing classifier for [`OrchestrationMode`].
///
/// Implement this trait with a mock in tests — the trait is the only surface
/// the server touches; no real LLM calls are ever made in unit tests.
#[async_trait]
pub trait OrchestrationClassifier: Send + Sync {
    /// Inspect `goal` and return the recommended [`OrchestrationMode`].
    async fn classify(&self, goal: &str) -> OrchestrationMode;
}

// ---------------------------------------------------------------------------
// Production implementation
// ---------------------------------------------------------------------------

/// Production classifier backed by an `Arc<dyn LlmClient>`.
///
/// Uses the model named by `AAOS_PLANNER_MODEL` (defaulting to `deepseek-chat`
/// if the variable is absent), matching the model the Planner already uses so
/// the shared instruction prefix is likely cached.
pub struct LlmOrchestrationClassifier {
    llm: Arc<dyn aaos_llm::LlmClient>,
}

impl LlmOrchestrationClassifier {
    pub fn new(llm: Arc<dyn aaos_llm::LlmClient>) -> Self {
        Self { llm }
    }

    fn model() -> String {
        std::env::var("AAOS_PLANNER_MODEL").unwrap_or_else(|_| "deepseek-chat".into())
    }
}

#[async_trait]
impl OrchestrationClassifier for LlmOrchestrationClassifier {
    #[instrument(skip(self), fields(goal_len = goal.len()))]
    async fn classify(&self, goal: &str) -> OrchestrationMode {
        use aaos_core::AgentId;
        use aaos_llm::{CompletionRequest, ContentBlock, Message};

        let system = concat!(
            "You route goals to one of two execution modes for the aaOS agent runtime:\n",
            "- plan: structured goals with clear data flow — fetch/scrape/compute/write, ",
            "comparisons, transformations, \"write X to Y\" tasks. Decomposes into a ",
            "parallel subtask DAG.\n",
            "- persistent: open-ended investigation, exploration, code-reading, bug-hunting, ",
            "\"find/analyse/understand X\" tasks. One long-lived agent manages its own ",
            "context across many turns.\n\n",
            "Respond with exactly one word: `plan` or `persistent`. No other text.",
        );

        let req = CompletionRequest {
            agent_id: AgentId::new(),
            model: Self::model(),
            system: system.into(),
            messages: vec![Message::User {
                content: format!("Goal: {goal}"),
            }],
            tools: vec![],
            max_tokens: 5,
        };

        let raw = match self.llm.complete(req).await {
            Ok(resp) => resp
                .content
                .iter()
                .filter_map(|b| {
                    if let ContentBlock::Text { text } = b {
                        Some(text.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(""),
            Err(e) => {
                warn!(error = %e, "orchestration classifier LLM call failed — defaulting to plan");
                return OrchestrationMode::Plan;
            }
        };

        parse_classifier_response(&raw)
    }
}

/// Parse a raw classifier response into an [`OrchestrationMode`].
///
/// Applies liberal matching — case-insensitive, leading/trailing whitespace
/// stripped, substring search so a model that replies "I think persistent is
/// best" still routes correctly.  Falls back to [`OrchestrationMode::Plan`]
/// on anything that doesn't contain a recognisable keyword.
fn parse_classifier_response(raw: &str) -> OrchestrationMode {
    let lower = raw.to_lowercase();
    let trimmed = lower.trim();

    if trimmed == "plan" {
        return OrchestrationMode::Plan;
    }
    if trimmed == "persistent" {
        return OrchestrationMode::Persistent;
    }

    // Substring fallback: model added a sentence but the keyword is still there.
    if trimmed.contains("persistent") {
        return OrchestrationMode::Persistent;
    }

    if trimmed.contains("plan") {
        return OrchestrationMode::Plan;
    }

    // No keyword found — log and default.
    warn!(
        raw = raw,
        "orchestration classifier returned unrecognised response — defaulting to plan"
    );
    OrchestrationMode::Plan
}

// ---------------------------------------------------------------------------
// No-op classifier — used when no LLM client is configured.
// ---------------------------------------------------------------------------

/// Always returns [`OrchestrationMode::Plan`] without making any LLM call.
///
/// Installed when the daemon starts without an API key.
pub struct NoopOrchestrationClassifier;

#[async_trait]
impl OrchestrationClassifier for NoopOrchestrationClassifier {
    async fn classify(&self, _goal: &str) -> OrchestrationMode {
        OrchestrationMode::Plan
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    use aaos_core::TokenUsage;
    use aaos_llm::{CompletionRequest, CompletionResponse, ContentBlock, LlmResult, LlmStopReason};

    /// Mock LLM that returns scripted string responses.
    struct ScriptedLlm {
        responses: Mutex<Vec<LlmResult<CompletionResponse>>>,
    }

    impl ScriptedLlm {
        fn returning(text: &str) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(vec![Ok(CompletionResponse {
                    content: vec![ContentBlock::Text { text: text.into() }],
                    stop_reason: LlmStopReason::EndTurn,
                    usage: TokenUsage {
                        input_tokens: 50,
                        output_tokens: 1,
                    },
                })]),
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(vec![Err(aaos_llm::LlmError::AuthError)]),
            })
        }
    }

    #[async_trait]
    impl aaos_llm::LlmClient for ScriptedLlm {
        fn max_context_tokens(&self, _model: &str) -> u32 {
            200_000
        }

        async fn complete(&self, _req: CompletionRequest) -> LlmResult<CompletionResponse> {
            self.responses.lock().unwrap().remove(0)
        }
    }

    // ---- parse_classifier_response unit tests (sync, no async overhead) ----

    #[test]
    fn parses_exact_plan() {
        assert_eq!(parse_classifier_response("plan"), OrchestrationMode::Plan);
    }

    #[test]
    fn parses_exact_persistent() {
        assert_eq!(
            parse_classifier_response("persistent"),
            OrchestrationMode::Persistent
        );
    }

    #[test]
    fn parses_plan_with_newline() {
        assert_eq!(parse_classifier_response("plan\n"), OrchestrationMode::Plan);
    }

    #[test]
    fn parses_persistent_with_spaces() {
        assert_eq!(
            parse_classifier_response("  Persistent "),
            OrchestrationMode::Persistent
        );
    }

    #[test]
    fn parses_persistent_substring() {
        assert_eq!(
            parse_classifier_response("I think persistent is best"),
            OrchestrationMode::Persistent
        );
    }

    #[test]
    fn parses_unrecognised_falls_back_to_plan() {
        assert_eq!(
            parse_classifier_response("whatever"),
            OrchestrationMode::Plan
        );
    }

    // ---- async classifier tests via mock LLM ----

    #[tokio::test]
    async fn classifier_returns_plan_on_plan_response() {
        let clf = LlmOrchestrationClassifier::new(ScriptedLlm::returning("plan\n"));
        assert_eq!(
            clf.classify("fetch HN top 5").await,
            OrchestrationMode::Plan
        );
    }

    #[tokio::test]
    async fn classifier_returns_persistent_on_persistent_response() {
        let clf = LlmOrchestrationClassifier::new(ScriptedLlm::returning("  Persistent "));
        assert_eq!(
            clf.classify("read the codebase and find bugs").await,
            OrchestrationMode::Persistent
        );
    }

    #[tokio::test]
    async fn classifier_returns_persistent_on_substring_match() {
        let clf =
            LlmOrchestrationClassifier::new(ScriptedLlm::returning("I think persistent is best"));
        assert_eq!(
            clf.classify("explore the architecture").await,
            OrchestrationMode::Persistent
        );
    }

    #[tokio::test]
    async fn classifier_falls_back_to_plan_on_unrecognised_response() {
        let clf = LlmOrchestrationClassifier::new(ScriptedLlm::returning("whatever"));
        assert_eq!(clf.classify("some goal").await, OrchestrationMode::Plan);
    }

    #[tokio::test]
    async fn classifier_falls_back_to_plan_on_llm_error() {
        let clf = LlmOrchestrationClassifier::new(ScriptedLlm::failing());
        assert_eq!(clf.classify("some goal").await, OrchestrationMode::Plan);
    }

    #[tokio::test]
    async fn noop_classifier_always_returns_plan() {
        let clf = NoopOrchestrationClassifier;
        assert_eq!(
            clf.classify("anything at all").await,
            OrchestrationMode::Plan
        );
    }
}
