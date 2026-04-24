//! Cheap single-shot LLM classifier that decides whether a goal should be
//! **decomposed** into a multi-node parallel DAG or handled **directly** by a
//! single multi-turn agent.
//!
//! ## How it works
//!
//! The production impl (`LlmOrchestrationClassifier`) sends the goal to the
//! configured LLM with a short routing prompt and expects exactly one word
//! back — `decompose` or `direct`. The prompt is intentionally minimal so
//! the call falls entirely inside the provider's prompt-cache hit window on
//! repeated submits (~50 input tokens, 1 output token).
//!
//! ## Fallback policy
//!
//! Any parse failure or LLM error → [`DecompositionMode::Direct`]. Direct is
//! the cheaper failure mode: it skips the Planner entirely and routes the goal
//! to a single multi-turn generalist agent, which is better than silently
//! spawning a malformed DAG.

use std::sync::Arc;

use async_trait::async_trait;
use tracing::{instrument, warn};

/// The two decomposition decisions the classifier can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecompositionMode {
    /// Goal has independent parallelisable subtasks; run Planner + PlanExecutor
    /// DAG to decompose it.
    Decompose,
    /// Goal is best handled by a single multi-turn agent; construct a 1-node
    /// inline Plan and route through PlanExecutor.
    Direct,
}

/// Routing classifier that picks a [`DecompositionMode`] from the goal string.
///
/// Implement this trait with a mock in tests — the trait is the only surface
/// the server touches; no real LLM calls are ever made in unit tests.
#[async_trait]
pub trait OrchestrationClassifier: Send + Sync {
    /// Inspect `goal` and return the recommended [`DecompositionMode`].
    async fn classify(&self, goal: &str) -> DecompositionMode;
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
    async fn classify(&self, goal: &str) -> DecompositionMode {
        use aaos_core::AgentId;
        use aaos_llm::{CompletionRequest, ContentBlock, Message};

        let system = concat!(
            "Does this goal have independent parallelizable subtasks that would benefit ",
            "from a multi-agent plan? Respond with `decompose` or `direct`.\n\n",
            "- decompose: structured goals with clear data flow — fetch/scrape/compute/write, ",
            "comparisons, transformations, \"write X to Y\" tasks. Decomposes into a ",
            "parallel subtask DAG.\n",
            "- direct: open-ended investigation, exploration, code-reading, bug-hunting, ",
            "\"find/analyse/understand X\" tasks. One multi-turn agent manages its own ",
            "context.\n\n",
            "Respond with exactly one word: `decompose` or `direct`. No other text.",
        );

        let req = CompletionRequest {
            agent_id: AgentId::new(),
            model: Self::model(),
            system: system.into(),
            messages: vec![Message::User {
                content: format!("Goal: {goal}"),
            }],
            tools: vec![],
            max_tokens: 8192,
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
                warn!(error = %e, "orchestration classifier LLM call failed — defaulting to direct");
                return DecompositionMode::Direct;
            }
        };

        parse_classifier_response(&raw)
    }
}

/// Parse a raw classifier response into a [`DecompositionMode`].
///
/// Applies liberal matching — case-insensitive, leading/trailing whitespace
/// stripped, substring search so a model that replies "I think direct is
/// best" still routes correctly.  Falls back to [`DecompositionMode::Direct`]
/// on anything that doesn't contain a recognisable keyword.
fn parse_classifier_response(raw: &str) -> DecompositionMode {
    let lower = raw.to_lowercase();
    let trimmed = lower.trim();

    if trimmed == "decompose" {
        return DecompositionMode::Decompose;
    }
    if trimmed == "direct" {
        return DecompositionMode::Direct;
    }

    // Substring fallback: model added a sentence but the keyword is still there.
    // Check decompose first since "direct" is a substring of "indirectly" etc.
    if trimmed.contains("decompose") {
        return DecompositionMode::Decompose;
    }

    if trimmed.contains("direct") {
        return DecompositionMode::Direct;
    }

    // No keyword found — log and default to direct (cheaper failure mode).
    warn!(
        raw = raw,
        "orchestration classifier returned unrecognised response — defaulting to direct"
    );
    DecompositionMode::Direct
}

// ---------------------------------------------------------------------------
// No-op classifier — used when no LLM client is configured.
// ---------------------------------------------------------------------------

/// Always returns [`DecompositionMode::Direct`] without making any LLM call.
///
/// Installed when the daemon starts without an API key.
pub struct NoopOrchestrationClassifier;

#[async_trait]
impl OrchestrationClassifier for NoopOrchestrationClassifier {
    async fn classify(&self, _goal: &str) -> DecompositionMode {
        DecompositionMode::Direct
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
    fn parses_exact_decompose() {
        assert_eq!(
            parse_classifier_response("decompose"),
            DecompositionMode::Decompose
        );
    }

    #[test]
    fn parses_exact_direct() {
        assert_eq!(
            parse_classifier_response("direct"),
            DecompositionMode::Direct
        );
    }

    #[test]
    fn parses_decompose_with_newline() {
        assert_eq!(
            parse_classifier_response("decompose\n"),
            DecompositionMode::Decompose
        );
    }

    #[test]
    fn parses_direct_with_spaces() {
        assert_eq!(
            parse_classifier_response("  Direct "),
            DecompositionMode::Direct
        );
    }

    #[test]
    fn parses_direct_substring() {
        assert_eq!(
            parse_classifier_response("I think direct is best"),
            DecompositionMode::Direct
        );
    }

    #[test]
    fn parses_decompose_substring() {
        assert_eq!(
            parse_classifier_response("This goal should decompose into parallel tasks"),
            DecompositionMode::Decompose
        );
    }

    #[test]
    fn parses_unrecognised_falls_back_to_direct() {
        assert_eq!(
            parse_classifier_response("whatever"),
            DecompositionMode::Direct
        );
    }

    #[test]
    fn parses_empty_falls_back_to_direct() {
        assert_eq!(parse_classifier_response(""), DecompositionMode::Direct);
    }

    // ---- async classifier tests via mock LLM ----

    #[tokio::test]
    async fn classifier_returns_decompose_on_decompose_response() {
        let clf = LlmOrchestrationClassifier::new(ScriptedLlm::returning("decompose\n"));
        assert_eq!(
            clf.classify("fetch HN top 5").await,
            DecompositionMode::Decompose
        );
    }

    #[tokio::test]
    async fn classifier_returns_direct_on_direct_response() {
        let clf = LlmOrchestrationClassifier::new(ScriptedLlm::returning("  Direct "));
        assert_eq!(
            clf.classify("read the codebase and find bugs").await,
            DecompositionMode::Direct
        );
    }

    #[tokio::test]
    async fn classifier_returns_direct_on_substring_match() {
        let clf = LlmOrchestrationClassifier::new(ScriptedLlm::returning("I think direct is best"));
        assert_eq!(
            clf.classify("explore the architecture").await,
            DecompositionMode::Direct
        );
    }

    #[tokio::test]
    async fn classifier_falls_back_to_direct_on_unrecognised_response() {
        let clf = LlmOrchestrationClassifier::new(ScriptedLlm::returning("whatever"));
        assert_eq!(clf.classify("some goal").await, DecompositionMode::Direct);
    }

    #[tokio::test]
    async fn classifier_falls_back_to_direct_on_llm_error() {
        let clf = LlmOrchestrationClassifier::new(ScriptedLlm::failing());
        assert_eq!(clf.classify("some goal").await, DecompositionMode::Direct);
    }

    #[tokio::test]
    async fn noop_classifier_always_returns_direct() {
        let clf = NoopOrchestrationClassifier;
        assert_eq!(
            clf.classify("anything at all").await,
            DecompositionMode::Direct
        );
    }
}
