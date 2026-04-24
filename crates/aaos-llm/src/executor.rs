use std::sync::Arc;

use aaos_core::{AgentId, AgentManifest, AgentServices, PromptSource, TokenUsage};
use serde_json::Value;

use crate::client::LlmClient;
use crate::types::{CompletionRequest, ContentBlock, LlmStopReason, Message};

/// Configuration for the execution loop.
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// Maximum LLM API calls per execution. Default: 50.
    pub max_iterations: u32,
    /// Maximum total tokens (input + output) across all iterations. Default: 1_000_000.
    pub max_total_tokens: u64,
    /// Maximum output tokens per LLM call. Default: 16384.
    pub max_output_tokens: u32,
    /// Number of times to nudge the LLM back into action when it emits an
    /// `EndTurn` response with no tool calls. Each nudge injects a short
    /// User message ("you haven't committed output — call file_write now
    /// or explain what's blocking you") and loops. After `commit_nudges`
    /// consecutive thought-only turns the loop terminates with `Complete`.
    /// Default: 0 (no nudging — preserves legacy behaviour). Roles that
    /// require a side-effect at termination (e.g. bug-hunt writing a
    /// findings file) set this to 1-3.
    pub commit_nudges: u32,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            max_iterations: 50,
            max_total_tokens: 1_000_000,
            max_output_tokens: 16_384,
            commit_nudges: 0,
        }
    }
}

/// Result of an agent execution.
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub response: String,
    pub usage: TokenUsage,
    pub iterations: u32,
    pub stop_reason: ExecutionStopReason,
}

/// Why the execution loop stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionStopReason {
    Complete,
    MaxIterations,
    MaxTokens,
    Truncated,
    Error(String),
}

impl std::fmt::Display for ExecutionStopReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Complete => write!(f, "complete"),
            Self::MaxIterations => write!(f, "max_iterations"),
            Self::MaxTokens => write!(f, "max_tokens"),
            Self::Truncated => write!(f, "truncated"),
            Self::Error(msg) => write!(f, "error: {msg}"),
        }
    }
}

/// Result of an agent execution that includes transcript delta for persistence.
#[derive(Debug, Clone)]
pub struct ExecutionResultWithHistory {
    pub response: String,
    pub usage: TokenUsage,
    pub iterations: u32,
    pub stop_reason: ExecutionStopReason,
    pub transcript_delta: Vec<Message>,
}

/// Drives an agent through the LLM inference loop.
pub struct AgentExecutor {
    llm: Arc<dyn LlmClient>,
    services: Arc<dyn AgentServices>,
    config: ExecutorConfig,
}

impl AgentExecutor {
    pub fn new(
        llm: Arc<dyn LlmClient>,
        services: Arc<dyn AgentServices>,
        config: ExecutorConfig,
    ) -> Self {
        Self {
            llm,
            services,
            config,
        }
    }

    /// Run an agent: call LLM, execute tool calls, feed results back, repeat.
    pub async fn run(
        &self,
        agent_id: AgentId,
        manifest: &AgentManifest,
        initial_message: &str,
    ) -> ExecutionResult {
        let system = match &manifest.system_prompt {
            PromptSource::Inline(s) => s.clone(),
            PromptSource::File(path) => match tokio::fs::read_to_string(path).await {
                Ok(content) => content,
                Err(e) => {
                    return ExecutionResult {
                        response: String::new(),
                        usage: TokenUsage::default(),
                        iterations: 0,
                        stop_reason: ExecutionStopReason::Error(format!(
                            "failed to read system prompt: {e}"
                        )),
                    };
                }
            },
        };

        // Get tools filtered by agent's capabilities
        let tools = match self.services.list_tools(agent_id).await {
            Ok(t) => t,
            Err(e) => {
                return ExecutionResult {
                    response: String::new(),
                    usage: TokenUsage::default(),
                    iterations: 0,
                    stop_reason: ExecutionStopReason::Error(format!("failed to list tools: {e}")),
                };
            }
        };

        let mut messages = vec![Message::User {
            content: initial_message.to_string(),
        }];
        let mut cumulative_usage = TokenUsage::default();
        let mut iterations: u32 = 0;
        let mut last_text = String::new();
        let mut nudges_used: u32 = 0;

        tracing::info!(
            agent_id = %agent_id,
            max_iterations = self.config.max_iterations,
            commit_nudges = self.config.commit_nudges,
            "executor.run: starting loop with config"
        );

        loop {
            // Call LLM
            let request = CompletionRequest {
                agent_id,
                model: manifest.model.clone(),
                system: system.clone(),
                messages: messages.clone(),
                tools: tools.clone(),
                max_tokens: self.config.max_output_tokens,
            };

            let response = match self.llm.complete(request).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        agent_id = %agent_id,
                        iter = iterations,
                        error = %e,
                        "executor.run: LLM call failed — returning Error stop_reason"
                    );
                    return ExecutionResult {
                        response: last_text,
                        usage: cumulative_usage,
                        iterations,
                        stop_reason: ExecutionStopReason::Error(e.to_string()),
                    };
                }
            };

            iterations += 1;

            tracing::info!(
                agent_id = %agent_id,
                iter = iterations,
                stop_reason = ?response.stop_reason,
                content_blocks = response.content.len(),
                "executor.run: LLM responded"
            );

            // Verbose logging: full LLM response
            for block in &response.content {
                match block {
                    ContentBlock::Text { text } => {
                        tracing::info!(agent_id = %agent_id, iter = iterations, "\n--- AGENT THINKS ---\n{}\n--- END ---", text);
                    }
                    ContentBlock::ToolUse { id: _, name, input } => {
                        tracing::info!(agent_id = %agent_id, iter = iterations, tool = %name, "\n--- TOOL CALL: {} ---\n{}\n--- END ---", name, input);
                    }
                }
            }

            // Report and accumulate usage
            let _ = self
                .services
                .report_usage(agent_id, response.usage.clone())
                .await;
            cumulative_usage.input_tokens += response.usage.input_tokens;
            cumulative_usage.output_tokens += response.usage.output_tokens;

            // Check token budget
            if cumulative_usage.total() > self.config.max_total_tokens {
                // Extract any text from this response before stopping
                for block in &response.content {
                    if let ContentBlock::Text { text } = block {
                        last_text = text.clone();
                    }
                }
                return ExecutionResult {
                    response: last_text,
                    usage: cumulative_usage,
                    iterations,
                    stop_reason: ExecutionStopReason::MaxTokens,
                };
            }

            // Handle stop reason
            match response.stop_reason {
                LlmStopReason::EndTurn | LlmStopReason::StopSequence => {
                    // Extract text from response
                    for block in &response.content {
                        if let ContentBlock::Text { text } = block {
                            last_text = text.clone();
                        }
                    }

                    // Commit-nudge: if the config allows nudges and we
                    // haven't exhausted them, append the last assistant
                    // turn to history, inject a nudge user-message, and
                    // loop. Forces the LLM to actually call its output
                    // tool (file_write etc.) instead of terminating mid-
                    // investigation with thought-only text.
                    if nudges_used < self.config.commit_nudges {
                        nudges_used += 1;
                        messages.push(Message::Assistant {
                            content: response.content.clone(),
                        });
                        messages.push(Message::User {
                            content: "You ended your turn without calling a tool. \
                                 Your execution contract requires you to call \
                                 file_write (or your designated output tool) with \
                                 your final report. Either call it now with whatever \
                                 findings you have, or explain in one sentence what \
                                 is blocking you. Do not emit another thought-only \
                                 response."
                                .into(),
                        });
                        tracing::warn!(
                            agent_id = %agent_id,
                            nudge = nudges_used,
                            "commit-nudge: LLM emitted EndTurn without tool call; \
                             re-prompting"
                        );
                        continue;
                    }

                    return ExecutionResult {
                        response: last_text,
                        usage: cumulative_usage,
                        iterations,
                        stop_reason: ExecutionStopReason::Complete,
                    };
                }
                LlmStopReason::MaxTokens => {
                    for block in &response.content {
                        if let ContentBlock::Text { text } = block {
                            last_text = text.clone();
                        }
                    }
                    return ExecutionResult {
                        response: last_text,
                        usage: cumulative_usage,
                        iterations,
                        stop_reason: ExecutionStopReason::Truncated,
                    };
                }
                LlmStopReason::ToolUse => {
                    // Collect tool_use blocks
                    let tool_uses: Vec<_> = response
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::ToolUse { id, name, input } => {
                                Some((id.clone(), name.clone(), input.clone()))
                            }
                            _ => None,
                        })
                        .collect();

                    // Also collect any text
                    for block in &response.content {
                        if let ContentBlock::Text { text } = block {
                            last_text = text.clone();
                        }
                    }

                    // Append assistant message
                    messages.push(Message::Assistant {
                        content: response.content.clone(),
                    });

                    // Execute each tool call sequentially
                    for (tool_use_id, tool_name, tool_input) in tool_uses {
                        match self
                            .services
                            .invoke_tool(agent_id, &tool_name, tool_input)
                            .await
                        {
                            Ok(result) => {
                                messages.push(Message::ToolResult {
                                    tool_use_id,
                                    content: result,
                                    is_error: false,
                                });
                            }
                            Err(e) => {
                                messages.push(Message::ToolResult {
                                    tool_use_id,
                                    content: Value::String(e.to_string()),
                                    is_error: true,
                                });
                            }
                        }
                    }

                    tracing::info!(
                        agent_id = %agent_id,
                        iter = iterations,
                        "executor.run: ToolUse arm complete — looping"
                    );

                    // Check iteration limit
                    if iterations >= self.config.max_iterations {
                        return ExecutionResult {
                            response: last_text,
                            usage: cumulative_usage,
                            iterations,
                            stop_reason: ExecutionStopReason::MaxIterations,
                        };
                    }
                }
            }
        }
    }

    /// Run an agent with prior conversation history.
    ///
    /// Prepends `prior_messages` to the message vec before adding the new user message.
    /// Returns `ExecutionResultWithHistory` with a `transcript_delta` containing only
    /// the NEW messages produced during this execution (not the prior ones).
    pub async fn run_with_history(
        &self,
        agent_id: AgentId,
        manifest: &AgentManifest,
        initial_message: &str,
        prior_messages: &[Message],
    ) -> ExecutionResultWithHistory {
        let system = match &manifest.system_prompt {
            PromptSource::Inline(s) => s.clone(),
            PromptSource::File(path) => match tokio::fs::read_to_string(path).await {
                Ok(content) => content,
                Err(e) => {
                    return ExecutionResultWithHistory {
                        response: String::new(),
                        usage: TokenUsage::default(),
                        iterations: 0,
                        stop_reason: ExecutionStopReason::Error(format!(
                            "failed to read system prompt: {e}"
                        )),
                        transcript_delta: vec![],
                    };
                }
            },
        };

        self.run_with_history_and_prompt(
            agent_id,
            manifest,
            initial_message,
            prior_messages,
            &system,
        )
        .await
    }

    /// Run an agent with prior conversation history and an overridden system prompt.
    ///
    /// Like `run_with_history`, but uses the provided `system_prompt` instead of
    /// extracting it from the manifest. This is used when the ContextManager has
    /// folded conversation summaries into the system prompt.
    pub async fn run_with_history_and_prompt(
        &self,
        agent_id: AgentId,
        manifest: &AgentManifest,
        initial_message: &str,
        prior_messages: &[Message],
        system_prompt: &str,
    ) -> ExecutionResultWithHistory {
        let system = system_prompt.to_string();

        // Get tools filtered by agent's capabilities
        let tools = match self.services.list_tools(agent_id).await {
            Ok(t) => t,
            Err(e) => {
                return ExecutionResultWithHistory {
                    response: String::new(),
                    usage: TokenUsage::default(),
                    iterations: 0,
                    stop_reason: ExecutionStopReason::Error(format!("failed to list tools: {e}")),
                    transcript_delta: vec![],
                };
            }
        };

        // Start with prior messages, then add the new user message
        let mut messages: Vec<Message> = prior_messages.to_vec();
        let new_user_message = Message::User {
            content: initial_message.to_string(),
        };
        messages.push(new_user_message.clone());

        // Track only new messages added during this execution
        let mut transcript_delta: Vec<Message> = vec![new_user_message];

        let mut cumulative_usage = TokenUsage::default();
        let mut iterations: u32 = 0;
        let mut last_text = String::new();

        loop {
            // Call LLM
            let request = CompletionRequest {
                agent_id,
                model: manifest.model.clone(),
                system: system.clone(),
                messages: messages.clone(),
                tools: tools.clone(),
                max_tokens: self.config.max_output_tokens,
            };

            let response = match self.llm.complete(request).await {
                Ok(r) => r,
                Err(e) => {
                    return ExecutionResultWithHistory {
                        response: last_text,
                        usage: cumulative_usage,
                        iterations,
                        stop_reason: ExecutionStopReason::Error(e.to_string()),
                        transcript_delta,
                    };
                }
            };

            iterations += 1;

            // Verbose logging: full LLM response
            for block in &response.content {
                match block {
                    ContentBlock::Text { text } => {
                        tracing::info!(agent_id = %agent_id, iter = iterations, "\n--- AGENT THINKS ---\n{}\n--- END ---", text);
                    }
                    ContentBlock::ToolUse { id: _, name, input } => {
                        tracing::info!(agent_id = %agent_id, iter = iterations, tool = %name, "\n--- TOOL CALL: {} ---\n{}\n--- END ---", name, input);
                    }
                }
            }

            // Report and accumulate usage
            let _ = self
                .services
                .report_usage(agent_id, response.usage.clone())
                .await;
            cumulative_usage.input_tokens += response.usage.input_tokens;
            cumulative_usage.output_tokens += response.usage.output_tokens;

            // Check token budget
            if cumulative_usage.total() > self.config.max_total_tokens {
                for block in &response.content {
                    if let ContentBlock::Text { text } = block {
                        last_text = text.clone();
                    }
                }
                return ExecutionResultWithHistory {
                    response: last_text,
                    usage: cumulative_usage,
                    iterations,
                    stop_reason: ExecutionStopReason::MaxTokens,
                    transcript_delta,
                };
            }

            // Handle stop reason
            match response.stop_reason {
                LlmStopReason::EndTurn | LlmStopReason::StopSequence => {
                    // Extract text from response and record assistant message in delta
                    let assistant_msg = Message::Assistant {
                        content: response.content.clone(),
                    };
                    for block in &response.content {
                        if let ContentBlock::Text { text } = block {
                            last_text = text.clone();
                        }
                    }
                    transcript_delta.push(assistant_msg);
                    return ExecutionResultWithHistory {
                        response: last_text,
                        usage: cumulative_usage,
                        iterations,
                        stop_reason: ExecutionStopReason::Complete,
                        transcript_delta,
                    };
                }
                LlmStopReason::MaxTokens => {
                    let assistant_msg = Message::Assistant {
                        content: response.content.clone(),
                    };
                    for block in &response.content {
                        if let ContentBlock::Text { text } = block {
                            last_text = text.clone();
                        }
                    }
                    transcript_delta.push(assistant_msg);
                    return ExecutionResultWithHistory {
                        response: last_text,
                        usage: cumulative_usage,
                        iterations,
                        stop_reason: ExecutionStopReason::Truncated,
                        transcript_delta,
                    };
                }
                LlmStopReason::ToolUse => {
                    // Collect tool_use blocks
                    let tool_uses: Vec<_> = response
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::ToolUse { id, name, input } => {
                                Some((id.clone(), name.clone(), input.clone()))
                            }
                            _ => None,
                        })
                        .collect();

                    // Also collect any text
                    for block in &response.content {
                        if let ContentBlock::Text { text } = block {
                            last_text = text.clone();
                        }
                    }

                    // Append assistant message (to both messages and delta)
                    let assistant_msg = Message::Assistant {
                        content: response.content.clone(),
                    };
                    messages.push(assistant_msg.clone());
                    transcript_delta.push(assistant_msg);

                    // Execute each tool call sequentially
                    for (tool_use_id, tool_name, tool_input) in tool_uses {
                        let tool_result_msg = match self
                            .services
                            .invoke_tool(agent_id, &tool_name, tool_input)
                            .await
                        {
                            Ok(result) => {
                                tracing::info!(agent_id = %agent_id, tool = %tool_name, "\n--- TOOL RESULT: {} ---\n{}\n--- END ---", tool_name, result);
                                Message::ToolResult {
                                    tool_use_id,
                                    content: result,
                                    is_error: false,
                                }
                            }
                            Err(e) => {
                                tracing::info!(agent_id = %agent_id, tool = %tool_name, "\n--- TOOL ERROR: {} ---\n{}\n--- END ---", tool_name, e);
                                Message::ToolResult {
                                    tool_use_id,
                                    content: Value::String(e.to_string()),
                                    is_error: true,
                                }
                            }
                        };
                        messages.push(tool_result_msg.clone());
                        transcript_delta.push(tool_result_msg);
                    }

                    // Check iteration limit
                    if iterations >= self.config.max_iterations {
                        return ExecutionResultWithHistory {
                            response: last_text,
                            usage: cumulative_usage,
                            iterations,
                            stop_reason: ExecutionStopReason::MaxIterations,
                            transcript_delta,
                        };
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{LlmError, LlmResult};
    use crate::types::CompletionResponse;
    use async_trait::async_trait;
    use std::sync::Mutex;

    // Mock LLM client that returns scripted responses
    struct MockLlmClient {
        responses: Mutex<Vec<LlmResult<CompletionResponse>>>,
    }

    impl MockLlmClient {
        fn new(responses: Vec<LlmResult<CompletionResponse>>) -> Self {
            Self {
                responses: Mutex::new(responses),
            }
        }
    }

    #[async_trait]
    impl LlmClient for MockLlmClient {
        fn max_context_tokens(&self, _model: &str) -> u32 {
            200_000
        }

        async fn complete(&self, _request: CompletionRequest) -> LlmResult<CompletionResponse> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Err(LlmError::Other("no more scripted responses".into()))
            } else {
                responses.remove(0)
            }
        }
    }

    // Mock AgentServices that tracks calls
    struct MockAgentServices {
        tool_results: Mutex<Vec<aaos_core::Result<Value>>>,
        tools: Vec<aaos_core::ToolDefinition>,
        usage_reports: Mutex<Vec<TokenUsage>>,
    }

    impl MockAgentServices {
        fn new(
            tool_results: Vec<aaos_core::Result<Value>>,
            tools: Vec<aaos_core::ToolDefinition>,
        ) -> Self {
            Self {
                tool_results: Mutex::new(tool_results),
                tools,
                usage_reports: Mutex::new(vec![]),
            }
        }
    }

    #[async_trait]
    impl AgentServices for MockAgentServices {
        async fn invoke_tool(
            &self,
            _agent_id: AgentId,
            _tool: &str,
            _input: Value,
        ) -> aaos_core::Result<Value> {
            let mut results = self.tool_results.lock().unwrap();
            if results.is_empty() {
                Err(aaos_core::CoreError::ToolNotFound("no more results".into()))
            } else {
                results.remove(0)
            }
        }

        async fn send_message(
            &self,
            _agent_id: AgentId,
            _message: Value,
        ) -> aaos_core::Result<Value> {
            Ok(serde_json::json!({"status": "delivered"}))
        }

        async fn request_approval(
            &self,
            _agent_id: AgentId,
            _description: String,
            _timeout: std::time::Duration,
        ) -> aaos_core::Result<aaos_core::ApprovalResult> {
            Ok(aaos_core::ApprovalResult::Approved)
        }

        async fn report_usage(
            &self,
            _agent_id: AgentId,
            usage: TokenUsage,
        ) -> aaos_core::Result<()> {
            self.usage_reports.lock().unwrap().push(usage);
            Ok(())
        }

        async fn list_tools(
            &self,
            _agent_id: AgentId,
        ) -> aaos_core::Result<Vec<aaos_core::ToolDefinition>> {
            Ok(self.tools.clone())
        }

        async fn send_and_wait(
            &self,
            _agent_id: AgentId,
            _recipient: AgentId,
            _method: String,
            _params: serde_json::Value,
            _timeout: std::time::Duration,
        ) -> aaos_core::Result<serde_json::Value> {
            Err(aaos_core::CoreError::Ipc("not implemented in mock".into()))
        }
    }

    fn test_manifest() -> AgentManifest {
        AgentManifest::from_yaml(
            r#"
name: test-agent
model: claude-haiku-4-5-20251001
system_prompt: "You are a test assistant."
capabilities:
  - "tool: echo"
"#,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn simple_text_response() {
        let llm = Arc::new(MockLlmClient::new(vec![Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text: "Hello!".into(),
            }],
            stop_reason: LlmStopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
            },
        })]));
        let services = Arc::new(MockAgentServices::new(vec![], vec![]));
        let executor = AgentExecutor::new(llm, services, ExecutorConfig::default());

        let result = executor.run(AgentId::new(), &test_manifest(), "Hi").await;
        assert_eq!(result.response, "Hello!");
        assert_eq!(result.iterations, 1);
        assert_eq!(result.stop_reason, ExecutionStopReason::Complete);
        assert_eq!(result.usage.input_tokens, 10);
        assert_eq!(result.usage.output_tokens, 5);
    }

    #[tokio::test]
    async fn tool_use_then_text() {
        let llm = Arc::new(MockLlmClient::new(vec![
            // First response: tool call
            Ok(CompletionResponse {
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "echo".into(),
                    input: serde_json::json!({"message": "test"}),
                }],
                stop_reason: LlmStopReason::ToolUse,
                usage: TokenUsage {
                    input_tokens: 20,
                    output_tokens: 10,
                },
            }),
            // Second response: text
            Ok(CompletionResponse {
                content: vec![ContentBlock::Text {
                    text: "Done!".into(),
                }],
                stop_reason: LlmStopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 30,
                    output_tokens: 5,
                },
            }),
        ]));
        let services = Arc::new(MockAgentServices::new(
            vec![Ok(serde_json::json!({"message": "test"}))],
            vec![],
        ));
        let executor = AgentExecutor::new(llm, services, ExecutorConfig::default());

        let result = executor
            .run(AgentId::new(), &test_manifest(), "Echo something")
            .await;
        assert_eq!(result.response, "Done!");
        assert_eq!(result.iterations, 2);
        assert_eq!(result.stop_reason, ExecutionStopReason::Complete);
        assert_eq!(result.usage.input_tokens, 50);
        assert_eq!(result.usage.output_tokens, 15);
    }

    #[tokio::test]
    async fn tool_error_fed_back_to_llm() {
        let llm = Arc::new(MockLlmClient::new(vec![
            // First: tool call
            Ok(CompletionResponse {
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "broken".into(),
                    input: serde_json::json!({}),
                }],
                stop_reason: LlmStopReason::ToolUse,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
            }),
            // Second: LLM sees the error and responds with text
            Ok(CompletionResponse {
                content: vec![ContentBlock::Text {
                    text: "Tool failed, here's my answer anyway.".into(),
                }],
                stop_reason: LlmStopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 15,
                    output_tokens: 10,
                },
            }),
        ]));
        let services = Arc::new(MockAgentServices::new(
            vec![Err(aaos_core::CoreError::ToolNotFound("broken".into()))],
            vec![],
        ));
        let executor = AgentExecutor::new(llm, services, ExecutorConfig::default());

        let result = executor
            .run(AgentId::new(), &test_manifest(), "Do something")
            .await;
        assert_eq!(result.stop_reason, ExecutionStopReason::Complete);
        assert_eq!(result.iterations, 2);
        assert!(result.response.contains("Tool failed"));
    }

    #[tokio::test]
    async fn max_iterations_limit() {
        // Create an LLM that always returns tool calls
        let mut responses = Vec::new();
        for i in 0..5 {
            responses.push(Ok(CompletionResponse {
                content: vec![ContentBlock::ToolUse {
                    id: format!("call_{i}"),
                    name: "echo".into(),
                    input: serde_json::json!({}),
                }],
                stop_reason: LlmStopReason::ToolUse,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
            }));
        }
        let llm = Arc::new(MockLlmClient::new(responses));

        let mut tool_results = Vec::new();
        for _ in 0..5 {
            tool_results.push(Ok(serde_json::json!({"ok": true})));
        }
        let services = Arc::new(MockAgentServices::new(tool_results, vec![]));

        let config = ExecutorConfig {
            max_iterations: 3,
            max_total_tokens: 1_000_000,
            max_output_tokens: 16_384,
            commit_nudges: 0,
        };
        let executor = AgentExecutor::new(llm, services, config);

        let result = executor
            .run(AgentId::new(), &test_manifest(), "Loop forever")
            .await;
        assert_eq!(result.stop_reason, ExecutionStopReason::MaxIterations);
        assert_eq!(result.iterations, 3);
    }

    #[tokio::test]
    async fn max_tokens_budget() {
        let llm = Arc::new(MockLlmClient::new(vec![Ok(CompletionResponse {
            content: vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "echo".into(),
                input: serde_json::json!({}),
            }],
            stop_reason: LlmStopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 500,
                output_tokens: 600,
            },
        })]));
        let services = Arc::new(MockAgentServices::new(
            vec![Ok(serde_json::json!({}))],
            vec![],
        ));
        let config = ExecutorConfig {
            max_iterations: 50,
            max_total_tokens: 100, // Very low budget
            max_output_tokens: 16_384,
            commit_nudges: 0,
        };
        let executor = AgentExecutor::new(llm, services, config);

        let result = executor
            .run(AgentId::new(), &test_manifest(), "Expensive")
            .await;
        assert_eq!(result.stop_reason, ExecutionStopReason::MaxTokens);
    }

    #[tokio::test]
    async fn llm_api_error_terminates() {
        let llm = Arc::new(MockLlmClient::new(vec![Err(LlmError::AuthError)]));
        let services = Arc::new(MockAgentServices::new(vec![], vec![]));
        let executor = AgentExecutor::new(llm, services, ExecutorConfig::default());

        let result = executor
            .run(AgentId::new(), &test_manifest(), "Hello")
            .await;
        assert!(matches!(result.stop_reason, ExecutionStopReason::Error(_)));
        assert_eq!(result.iterations, 0);
    }

    #[tokio::test]
    async fn truncated_on_llm_max_tokens() {
        let llm = Arc::new(MockLlmClient::new(vec![Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text: "Partial resp...".into(),
            }],
            stop_reason: LlmStopReason::MaxTokens,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 4096,
            },
        })]));
        let services = Arc::new(MockAgentServices::new(vec![], vec![]));
        let executor = AgentExecutor::new(llm, services, ExecutorConfig::default());

        let result = executor
            .run(AgentId::new(), &test_manifest(), "Write a lot")
            .await;
        assert_eq!(result.stop_reason, ExecutionStopReason::Truncated);
        assert_eq!(result.response, "Partial resp...");
    }

    #[tokio::test]
    async fn run_with_history_passes_prior_messages() {
        let llm = Arc::new(MockLlmClient::new(vec![Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text: "I remember!".into(),
            }],
            stop_reason: LlmStopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 20,
                output_tokens: 5,
            },
        })]));
        let services = Arc::new(MockAgentServices::new(vec![], vec![]));
        let executor = AgentExecutor::new(llm, services, ExecutorConfig::default());

        let prior = vec![
            Message::User {
                content: "My name is Alice.".into(),
            },
            Message::Assistant {
                content: vec![ContentBlock::Text {
                    text: "Hello Alice!".into(),
                }],
            },
        ];

        let result = executor
            .run_with_history(AgentId::new(), &test_manifest(), "What's my name?", &prior)
            .await;

        assert_eq!(result.response, "I remember!");
        assert_eq!(result.stop_reason, ExecutionStopReason::Complete);
        assert_eq!(result.transcript_delta.len(), 2);
        match &result.transcript_delta[0] {
            Message::User { content } => assert_eq!(content, "What's my name?"),
            _ => panic!("expected User message"),
        }
    }

    #[tokio::test]
    async fn run_with_history_tool_use_transcript() {
        let llm = Arc::new(MockLlmClient::new(vec![
            Ok(CompletionResponse {
                content: vec![ContentBlock::ToolUse {
                    id: "c1".into(),
                    name: "echo".into(),
                    input: serde_json::json!({"msg": "test"}),
                }],
                stop_reason: LlmStopReason::ToolUse,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
            }),
            Ok(CompletionResponse {
                content: vec![ContentBlock::Text {
                    text: "Done".into(),
                }],
                stop_reason: LlmStopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 15,
                    output_tokens: 3,
                },
            }),
        ]));
        let services = Arc::new(MockAgentServices::new(
            vec![Ok(serde_json::json!({"ok": true}))],
            vec![],
        ));
        let executor = AgentExecutor::new(llm, services, ExecutorConfig::default());

        let result = executor
            .run_with_history(AgentId::new(), &test_manifest(), "Do it", &[])
            .await;

        assert_eq!(result.response, "Done");
        assert_eq!(result.transcript_delta.len(), 4);
    }

    #[tokio::test]
    async fn run_with_empty_history_same_as_run() {
        let llm = Arc::new(MockLlmClient::new(vec![Ok(CompletionResponse {
            content: vec![ContentBlock::Text { text: "Hi!".into() }],
            stop_reason: LlmStopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
            },
        })]));
        let services = Arc::new(MockAgentServices::new(vec![], vec![]));
        let executor = AgentExecutor::new(llm, services, ExecutorConfig::default());

        let result = executor
            .run_with_history(AgentId::new(), &test_manifest(), "Hello", &[])
            .await;

        assert_eq!(result.response, "Hi!");
        assert_eq!(result.iterations, 1);
        assert_eq!(result.transcript_delta.len(), 2);
    }

    #[tokio::test]
    async fn usage_reported_each_iteration() {
        let llm = Arc::new(MockLlmClient::new(vec![
            Ok(CompletionResponse {
                content: vec![ContentBlock::ToolUse {
                    id: "c1".into(),
                    name: "echo".into(),
                    input: serde_json::json!({}),
                }],
                stop_reason: LlmStopReason::ToolUse,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
            }),
            Ok(CompletionResponse {
                content: vec![ContentBlock::Text {
                    text: "done".into(),
                }],
                stop_reason: LlmStopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 20,
                    output_tokens: 3,
                },
            }),
        ]));
        let services = Arc::new(MockAgentServices::new(
            vec![Ok(serde_json::json!({}))],
            vec![],
        ));
        let executor = AgentExecutor::new(llm, services.clone(), ExecutorConfig::default());

        executor.run(AgentId::new(), &test_manifest(), "hi").await;
        let reports = services.usage_reports.lock().unwrap();
        assert_eq!(reports.len(), 2);
    }
}
