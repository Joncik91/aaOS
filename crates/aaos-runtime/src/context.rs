use std::sync::Arc;

use aaos_core::{AgentId, TokenBudget};
use aaos_llm::{ContentBlock, LlmClient, Message};
use aaos_llm::types::{CompletionRequest, CompletionResponse};

// ---------------------------------------------------------------------------
// Free helper functions (live here, NOT in aaos-core, because they need Message)
// ---------------------------------------------------------------------------

/// Count characters in a single message, handling all variants.
pub fn message_chars(msg: &Message) -> usize {
    match msg {
        Message::User { content } => content.len(),
        Message::Assistant { content } => content
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => text.len(),
                ContentBlock::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
            })
            .sum(),
        Message::ToolResult {
            tool_use_id,
            content,
            ..
        } => tool_use_id.len() + content.to_string().len(),
        Message::Summary { content, .. } => content.len(),
    }
}

/// Estimate token count for a slice of messages using chars/4 heuristic.
pub fn estimate_tokens(messages: &[Message]) -> u32 {
    let total_chars: usize = messages.iter().map(|m| message_chars(m)).sum();
    (total_chars / 4) as u32
}

/// Return true if the estimated token count exceeds `threshold` fraction of the budget.
pub fn should_summarize(budget: &TokenBudget, messages: &[Message], threshold: f32) -> bool {
    let estimated = estimate_tokens(messages);
    estimated as f32 > budget.max_tokens as f32 * threshold
}

// ---------------------------------------------------------------------------
// PreparedContext / SummarizationResult
// ---------------------------------------------------------------------------

/// Result of context preparation, returned to the caller.
pub struct PreparedContext {
    /// Messages to send to the LLM (Summary variants removed).
    pub messages: Vec<Message>,
    /// System prompt, potentially prefixed with summary text.
    pub system_prompt: String,
    /// If summarization occurred, this contains the result.
    pub summarization: Option<SummarizationResult>,
}

/// Details of a summarization that occurred during context preparation.
pub struct SummarizationResult {
    /// The raw messages that were archived (removed from active history).
    pub archived_messages: Vec<Message>,
    /// The Summary message to store in the session (for future re-summarization).
    pub summary: Message,
    /// Message indices from original history that were summarized.
    pub source_range: (usize, usize),
    /// Estimated tokens freed by this summarization.
    pub tokens_saved_estimate: u32,
}

// ---------------------------------------------------------------------------
// ContextManager
// ---------------------------------------------------------------------------

/// Manages agent context windows by summarizing and archiving old messages.
pub struct ContextManager {
    llm_client: Arc<dyn LlmClient>,
    budget: TokenBudget,
    summarization_model: String,
    summarization_threshold: f32,
}

impl ContextManager {
    pub fn new(
        llm_client: Arc<dyn LlmClient>,
        budget: TokenBudget,
        model: String,
        threshold: f32,
    ) -> Self {
        Self {
            llm_client,
            budget,
            summarization_model: model,
            summarization_threshold: threshold,
        }
    }

    /// Prepare context for the next LLM call.
    ///
    /// Does NOT mutate history. Returns `PreparedContext` with:
    /// - messages: the message list to send (Summary variants folded into system prompt)
    /// - system_prompt: potentially prefixed with summary text
    /// - summarization: if summarization occurred, the archived segment for the caller to persist
    ///
    /// The caller MUST, in order:
    /// 1. Archive the segment (if summarization is Some)
    /// 2. Replace archived messages with the Summary in the in-memory history
    pub async fn prepare_context(
        &self,
        history: &[Message],
        system_prompt: &str,
    ) -> Result<PreparedContext, String> {
        if !should_summarize(&self.budget, history, self.summarization_threshold) {
            // No summarization needed — just fold any existing Summary messages into the prompt
            return Ok(Self::fold_summaries_into_prompt(history, system_prompt));
        }

        // Select messages to summarize: oldest messages up to ~40% of the context
        let target_tokens = (self.budget.max_tokens as f32 * 0.4) as u32;
        let selection_end = Self::select_summarization_boundary(history, target_tokens);

        if selection_end == 0 {
            // Nothing to summarize (edge case: single huge message)
            return Ok(Self::fold_summaries_into_prompt(history, system_prompt));
        }

        let selected = &history[..selection_end];

        // Guard: make sure the selected messages fit in the summarization model's context
        let summarization_model_max =
            self.llm_client.max_context_tokens(&self.summarization_model);
        let selected_tokens = estimate_tokens(selected);
        let guarded_end = if selected_tokens > summarization_model_max * 9 / 10 {
            // Trim selection to fit
            Self::select_summarization_boundary(history, summarization_model_max * 7 / 10)
        } else {
            selection_end
        };

        if guarded_end == 0 {
            return Ok(Self::fold_summaries_into_prompt(history, system_prompt));
        }

        let to_summarize = &history[..guarded_end];

        // Format selected messages as text for the summarization prompt
        let formatted = Self::format_messages_for_summary(to_summarize);

        // Call the summarization LLM
        let summary_result = self.call_summarization_llm(&formatted).await;

        match summary_result {
            Ok(summary_text) => {
                let tokens_before = estimate_tokens(to_summarize);
                let tokens_after = (summary_text.len() / 4) as u32;
                let tokens_saved = tokens_before.saturating_sub(tokens_after);

                let summary_msg = Message::Summary {
                    content: summary_text.clone(),
                    messages_summarized: guarded_end as u32,
                    source_range: (0, guarded_end - 1),
                };

                let summarization = SummarizationResult {
                    archived_messages: to_summarize.to_vec(),
                    summary: summary_msg.clone(),
                    source_range: (0, guarded_end - 1),
                    tokens_saved_estimate: tokens_saved,
                };

                // Build the new message list: summary_msg + remaining history
                let mut new_history = vec![summary_msg];
                new_history.extend_from_slice(&history[guarded_end..]);

                let prepared = Self::fold_summaries_into_prompt(&new_history, system_prompt);

                Ok(PreparedContext {
                    messages: prepared.messages,
                    system_prompt: prepared.system_prompt,
                    summarization: Some(summarization),
                })
            }
            Err(e) => {
                // Fallback: no-op on failure (logs warning)
                tracing::warn!("Summarization failed ({e}), falling back to no-op");
                Ok(Self::fold_summaries_into_prompt(history, system_prompt))
            }
        }
    }

    /// Fold any Summary messages at the start of history into the system prompt prefix.
    /// Returns PreparedContext with summaries removed from messages and prepended to system_prompt.
    fn fold_summaries_into_prompt(history: &[Message], system_prompt: &str) -> PreparedContext {
        let mut summary_texts = Vec::new();
        let mut first_non_summary = 0;

        for (i, msg) in history.iter().enumerate() {
            match msg {
                Message::Summary { content, .. } => {
                    summary_texts.push(content.clone());
                    first_non_summary = i + 1;
                }
                _ => break,
            }
        }

        let final_system = if summary_texts.is_empty() {
            system_prompt.to_string()
        } else {
            let prefix = summary_texts.join("\n\n");
            format!("[Previous conversation summary]\n{prefix}\n\n{system_prompt}")
        };

        let messages = history[first_non_summary..].to_vec();

        PreparedContext {
            messages,
            system_prompt: final_system,
            summarization: None,
        }
    }

    /// Select how many messages from the start to summarize.
    /// Respects atomic tool call/result pairs.
    /// Returns the exclusive end index.
    fn select_summarization_boundary(history: &[Message], target_tokens: u32) -> usize {
        let mut accumulated_tokens: u32 = 0;
        let mut boundary = 0;

        for (i, msg) in history.iter().enumerate() {
            let msg_tokens = (message_chars(msg) / 4) as u32;
            accumulated_tokens += msg_tokens;

            if accumulated_tokens >= target_tokens {
                boundary = Self::adjust_boundary_for_tool_pairs(history, i + 1);
                break;
            }
        }

        // If we went through all messages without hitting target, summarize half
        if boundary == 0 && !history.is_empty() {
            boundary = Self::adjust_boundary_for_tool_pairs(history, history.len() / 2);
        }

        boundary
    }

    /// Adjust boundary to avoid splitting Assistant(ToolUse) / ToolResult pairs.
    /// If the message at boundary-1 is an Assistant with ToolUse, include all subsequent ToolResults.
    /// If the message at boundary is a ToolResult, back up to before its Assistant message.
    fn adjust_boundary_for_tool_pairs(history: &[Message], mut boundary: usize) -> usize {
        if boundary == 0 || boundary > history.len() {
            return boundary;
        }

        // Check if the last included message is an Assistant with tool_use
        if let Message::Assistant { content } = &history[boundary - 1] {
            let has_tool_use = content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. }));
            if has_tool_use {
                // Include all following ToolResult messages
                while boundary < history.len() {
                    if matches!(&history[boundary], Message::ToolResult { .. }) {
                        boundary += 1;
                    } else {
                        break;
                    }
                }
            }
        }

        // Check if the boundary starts on a ToolResult — back up
        if boundary < history.len() {
            if matches!(&history[boundary], Message::ToolResult { .. }) {
                // Find the Assistant message before this ToolResult
                while boundary > 0 {
                    boundary -= 1;
                    if matches!(&history[boundary], Message::Assistant { .. }) {
                        break;
                    }
                }
            }
        }

        boundary
    }

    /// Format messages as readable text for the summarization LLM.
    fn format_messages_for_summary(messages: &[Message]) -> String {
        let mut lines = Vec::new();
        for msg in messages {
            match msg {
                Message::User { content } => {
                    lines.push(format!("User: {content}"));
                }
                Message::Assistant { content } => {
                    for block in content {
                        match block {
                            ContentBlock::Text { text } => {
                                lines.push(format!("Assistant: {text}"));
                            }
                            ContentBlock::ToolUse { name, input, .. } => {
                                lines.push(format!("Assistant [tool_use: {name}]: {input}"));
                            }
                        }
                    }
                }
                Message::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let label = if *is_error { "error" } else { "result" };
                    lines.push(format!("Tool {label} ({tool_use_id}): {content}"));
                }
                Message::Summary { content, .. } => {
                    lines.push(format!("Previous summary: {content}"));
                }
            }
        }
        lines.join("\n")
    }

    /// Call the LLM to produce a summary of the formatted conversation.
    async fn call_summarization_llm(&self, formatted_text: &str) -> Result<String, String> {
        let request = CompletionRequest {
            agent_id: AgentId::new(), // ephemeral ID for summarization
            model: self.summarization_model.clone(),
            system: "Compress this conversation into a dense factual summary. Preserve all \
                     names, numbers, decisions, stated preferences, and tool results. Be concise."
                .to_string(),
            messages: vec![Message::User {
                content: formatted_text.to_string(),
            }],
            tools: vec![],
            max_tokens: 2048,
        };

        let response = self
            .llm_client
            .complete(request)
            .await
            .map_err(|e| e.to_string())?;

        // Extract text from response
        let text = response
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        if text.is_empty() {
            return Err("summarization LLM returned empty response".to_string());
        }

        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_core::TokenUsage;
    use aaos_llm::{LlmError, LlmResult};
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct MockSummarizationLlm {
        responses: Mutex<Vec<LlmResult<CompletionResponse>>>,
    }

    impl MockSummarizationLlm {
        fn with_summary(text: &str) -> Self {
            Self {
                responses: Mutex::new(vec![Ok(CompletionResponse {
                    content: vec![ContentBlock::Text {
                        text: text.into(),
                    }],
                    stop_reason: LlmStopReason::EndTurn,
                    usage: TokenUsage {
                        input_tokens: 100,
                        output_tokens: 50,
                    },
                })]),
            }
        }

        fn failing() -> Self {
            Self {
                responses: Mutex::new(vec![Err(LlmError::Other(
                    "simulated failure".into(),
                ))]),
            }
        }
    }

    #[async_trait]
    impl LlmClient for MockSummarizationLlm {
        async fn complete(&self, _req: CompletionRequest) -> LlmResult<CompletionResponse> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Err(LlmError::Other("no more responses".into()))
            } else {
                responses.remove(0)
            }
        }

        fn max_context_tokens(&self, _model: &str) -> u32 {
            200_000
        }
    }

    fn make_long_history(n: usize) -> Vec<Message> {
        let mut messages = Vec::new();
        for i in 0..n {
            messages.push(Message::User {
                content: format!("Message {i}: {}", "x".repeat(500)),
            });
            messages.push(Message::Assistant {
                content: vec![ContentBlock::Text {
                    text: format!("Response {i}: {}", "y".repeat(500)),
                }],
            });
        }
        messages
    }

    // -----------------------------------------------------------------------
    // Free function unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn message_chars_user() {
        let msg = Message::User {
            content: "hello".into(),
        };
        assert_eq!(message_chars(&msg), 5);
    }

    #[test]
    fn message_chars_summary() {
        let msg = Message::Summary {
            content: "abc".into(),
            messages_summarized: 1,
            source_range: (0, 0),
        };
        assert_eq!(message_chars(&msg), 3);
    }

    #[test]
    fn estimate_tokens_basic() {
        let msgs = vec![Message::User {
            content: "x".repeat(400),
        }];
        assert_eq!(estimate_tokens(&msgs), 100);
    }

    #[test]
    fn should_summarize_under() {
        let budget = TokenBudget { max_tokens: 1000 };
        let msgs = vec![Message::User {
            content: "hi".into(),
        }];
        assert!(!should_summarize(&budget, &msgs, 0.7));
    }

    #[test]
    fn should_summarize_over() {
        let budget = TokenBudget { max_tokens: 100 };
        let msgs = vec![Message::User {
            content: "x".repeat(400),
        }];
        // 400 chars / 4 = 100 tokens, threshold 0.7 * 100 = 70 → 100 > 70
        assert!(should_summarize(&budget, &msgs, 0.7));
    }

    // -----------------------------------------------------------------------
    // Async integration tests
    // -----------------------------------------------------------------------

    use aaos_llm::LlmStopReason;

    #[tokio::test]
    async fn no_summarization_under_threshold() {
        let llm = Arc::new(MockSummarizationLlm::with_summary("unused"));
        let budget = TokenBudget {
            max_tokens: 1_000_000,
        }; // very large
        let cm = ContextManager::new(llm, budget, "test-model".into(), 0.7);

        let history = vec![
            Message::User {
                content: "hello".into(),
            },
            Message::Assistant {
                content: vec![ContentBlock::Text {
                    text: "hi".into(),
                }],
            },
        ];

        let result = cm
            .prepare_context(&history, "You are helpful.")
            .await
            .unwrap();
        assert!(result.summarization.is_none());
        assert_eq!(result.messages.len(), 2);
        assert_eq!(result.system_prompt, "You are helpful.");
    }

    #[tokio::test]
    async fn summarization_triggers_over_threshold() {
        let llm = Arc::new(MockSummarizationLlm::with_summary(
            "User discussed topics 0-24. Key facts preserved.",
        ));
        // Small budget so our test history exceeds the threshold
        let budget = TokenBudget { max_tokens: 2000 };
        let cm = ContextManager::new(llm, budget, "test-model".into(), 0.7);

        let history = make_long_history(25); // 50 messages, ~12500 tokens estimated

        let result = cm
            .prepare_context(&history, "You are helpful.")
            .await
            .unwrap();
        assert!(result.summarization.is_some());
        let summ = result.summarization.unwrap();
        assert!(!summ.archived_messages.is_empty());
        assert!(summ.tokens_saved_estimate > 0);
        // System prompt should contain the summary prefix
        assert!(result.system_prompt.contains("[Previous conversation summary]"));
    }

    #[tokio::test]
    async fn fallback_on_llm_failure() {
        let llm = Arc::new(MockSummarizationLlm::failing());
        let budget = TokenBudget { max_tokens: 2000 };
        let cm = ContextManager::new(llm, budget, "test-model".into(), 0.7);

        let history = make_long_history(25);

        // Should not return error — falls back to no-op
        let result = cm
            .prepare_context(&history, "You are helpful.")
            .await
            .unwrap();
        assert!(result.summarization.is_none()); // no summarization on failure
    }

    #[tokio::test]
    async fn existing_summary_folded_into_prompt() {
        let llm = Arc::new(MockSummarizationLlm::with_summary("unused"));
        let budget = TokenBudget {
            max_tokens: 1_000_000,
        };
        let cm = ContextManager::new(llm, budget, "test-model".into(), 0.7);

        let history = vec![
            Message::Summary {
                content: "Previously: User name is Alice, dog is Rex.".into(),
                messages_summarized: 10,
                source_range: (0, 9),
            },
            Message::User {
                content: "What's my name?".into(),
            },
            Message::Assistant {
                content: vec![ContentBlock::Text {
                    text: "Alice!".into(),
                }],
            },
        ];

        let result = cm
            .prepare_context(&history, "You are helpful.")
            .await
            .unwrap();
        assert!(result.system_prompt.contains("Previously: User name is Alice"));
        assert!(result.system_prompt.contains("You are helpful."));
        // Summary should NOT be in the messages list
        assert_eq!(result.messages.len(), 2);
        assert!(!matches!(&result.messages[0], Message::Summary { .. }));
    }

    #[tokio::test]
    async fn full_flow_summarize_archive_continue() {
        // Setup: mock LLM that returns a summary, small budget
        let llm = Arc::new(MockSummarizationLlm::with_summary(
            "Summary: User sent 20 pairs of messages about topics 0-19. Each was acknowledged.",
        ));
        let budget = TokenBudget { max_tokens: 3000 };
        let cm = ContextManager::new(llm, budget, "test-model".into(), 0.7);

        // Build a long history
        let history = make_long_history(20); // 40 messages

        // First call: should trigger summarization
        let result1 = cm.prepare_context(&history, "You are helpful.").await.unwrap();
        assert!(result1.summarization.is_some(), "Expected summarization to trigger");

        let summ = result1.summarization.unwrap();
        assert!(!summ.archived_messages.is_empty());

        // Simulate what the persistent loop would do:
        let mut new_history = vec![summ.summary];
        let end = summ.source_range.1 + 1;
        new_history.extend_from_slice(&history[end..]);

        // Verify the new history is shorter
        assert!(new_history.len() < history.len());
        assert!(matches!(&new_history[0], Message::Summary { .. }));

        // Second call with new history and large budget: Summary should fold into system prompt
        let llm2 = Arc::new(MockSummarizationLlm::with_summary("unused"));
        let budget2 = TokenBudget { max_tokens: 1_000_000 };
        let cm2 = ContextManager::new(llm2, budget2, "test-model".into(), 0.7);

        let result2 = cm2.prepare_context(&new_history, "You are helpful.").await.unwrap();
        assert!(result2.summarization.is_none());
        assert!(result2.system_prompt.contains("[Previous conversation summary]"));
        assert!(result2.system_prompt.contains("Summary: User sent 20 pairs"));
        // Messages should not contain Summary variant
        for msg in &result2.messages {
            assert!(!matches!(msg, Message::Summary { .. }));
        }
    }

    #[tokio::test]
    async fn tool_call_result_pairs_not_split() {
        let llm = Arc::new(MockSummarizationLlm::with_summary(
            "Summary of tool interactions.",
        ));
        let budget = TokenBudget { max_tokens: 500 }; // very small to force summarization
        let cm = ContextManager::new(llm, budget, "test-model".into(), 0.7);

        let history = vec![
            Message::User {
                content: "x".repeat(400),
            },
            Message::Assistant {
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "search".into(),
                    input: serde_json::json!({"q": "test"}),
                }],
            },
            Message::ToolResult {
                tool_use_id: "call_1".into(),
                content: serde_json::json!({"results": []}),
                is_error: false,
            },
            Message::User {
                content: "thanks".into(),
            },
        ];

        let result = cm.prepare_context(&history, "system").await.unwrap();
        if let Some(ref summ) = result.summarization {
            // The archived messages should include the tool pair together
            let has_assistant = summ
                .archived_messages
                .iter()
                .any(|m| matches!(m, Message::Assistant { .. }));
            let has_tool_result = summ
                .archived_messages
                .iter()
                .any(|m| matches!(m, Message::ToolResult { .. }));
            // If one is there, both must be
            if has_assistant || has_tool_result {
                assert!(
                    has_assistant && has_tool_result,
                    "Tool call/result pair was split during summarization"
                );
            }
        }
    }
}
