use reqwest::Client;
use serde_json::{json, Value};

use aaos_core::TokenUsage;
use async_trait::async_trait;

use crate::client::LlmClient;
use crate::error::{LlmError, LlmResult};
use crate::types::{CompletionRequest, CompletionResponse, ContentBlock, LlmStopReason, Message};

const DEEPSEEK_SUPPORTED_MODELS: &[&str] = &[
    "deepseek-reasoner",
    "deepseek-chat",
];

/// Configuration for an OpenAI-compatible API client (e.g. DeepSeek).
#[derive(Debug, Clone)]
pub struct OpenAiCompatConfig {
    pub api_key: String,
    pub base_url: String,
    pub default_max_tokens: u32,
}

impl OpenAiCompatConfig {
    /// Load DeepSeek configuration from environment variables.
    /// API key from DEEPSEEK_API_KEY (required).
    /// Base URL defaults to https://api.deepseek.com.
    pub fn deepseek_from_env() -> LlmResult<Self> {
        let api_key = std::env::var("DEEPSEEK_API_KEY").map_err(|_| LlmError::AuthError)?;
        let base_url = std::env::var("DEEPSEEK_BASE_URL")
            .unwrap_or_else(|_| "https://api.deepseek.com".to_string());
        Ok(Self {
            api_key,
            base_url,
            // deepseek-reasoner supports up to 32K output tokens; use 8K as a safe default
            default_max_tokens: 8192,
        })
    }
}

/// OpenAI-compatible LLM client. Works with DeepSeek and any provider
/// that implements the OpenAI Chat Completions API format.
pub struct OpenAiCompatibleClient {
    config: OpenAiCompatConfig,
    http: Client,
}

impl OpenAiCompatibleClient {
    pub fn new(config: OpenAiCompatConfig) -> Self {
        Self {
            config,
            http: Client::new(),
        }
    }

    fn validate_model(&self, model: &str) -> LlmResult<()> {
        if !DEEPSEEK_SUPPORTED_MODELS.contains(&model) {
            return Err(LlmError::UnsupportedModel {
                model: model.to_string(),
            });
        }
        Ok(())
    }

    /// Translate aaOS messages to OpenAI chat format.
    ///
    /// Key differences from Anthropic format:
    /// - System prompt is injected as first message with role "system"
    /// - ToolResult becomes role "tool" with tool_call_id
    /// - Assistant tool calls use tool_calls array with function format
    fn build_messages(&self, request: &CompletionRequest) -> Vec<Value> {
        let mut messages: Vec<Value> = Vec::new();

        // System prompt as first message (OpenAI format)
        if !request.system.is_empty() {
            messages.push(json!({
                "role": "system",
                "content": request.system,
            }));
        }

        for msg in &request.messages {
            match msg {
                Message::User { content } => {
                    messages.push(json!({
                        "role": "user",
                        "content": content,
                    }));
                }
                Message::Assistant { content } => {
                    // Separate text blocks and tool_use blocks
                    let mut text_parts: Vec<String> = Vec::new();
                    let mut tool_calls: Vec<Value> = Vec::new();

                    for block in content {
                        match block {
                            ContentBlock::Text { text } => {
                                text_parts.push(text.clone());
                            }
                            ContentBlock::ToolUse { id, name, input } => {
                                tool_calls.push(json!({
                                    "id": id,
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": serde_json::to_string(input)
                                            .unwrap_or_else(|_| "{}".to_string()),
                                    },
                                }));
                            }
                        }
                    }

                    let text_content = if text_parts.is_empty() {
                        None
                    } else {
                        Some(text_parts.join("\n"))
                    };

                    let mut assistant_msg = json!({ "role": "assistant" });
                    if let Some(text) = text_content {
                        assistant_msg["content"] = json!(text);
                    } else {
                        // OpenAI requires content to be present (can be null for tool-only)
                        assistant_msg["content"] = json!(null);
                    }
                    if !tool_calls.is_empty() {
                        assistant_msg["tool_calls"] = json!(tool_calls);
                    }

                    messages.push(assistant_msg);
                }
                Message::ToolResult {
                    tool_use_id,
                    content,
                    is_error: _,
                } => {
                    // OpenAI tool result format: role "tool" with tool_call_id
                    let content_str = match content {
                        Value::String(s) => s.clone(),
                        other => serde_json::to_string(other)
                            .unwrap_or_else(|_| other.to_string()),
                    };
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_use_id,
                        "content": content_str,
                    }));
                }
                Message::Summary { .. } => {
                    // Summary messages are folded into the system prompt by ContextManager.
                    // They should never appear in the messages array sent to the API.
                    panic!("BUG: Message::Summary must not be sent to the LLM API directly; fold into system prompt via ContextManager")
                }
            }
        }

        messages
    }

    fn build_request_body(&self, request: &CompletionRequest) -> Value {
        let messages = self.build_messages(request);

        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    },
                })
            })
            .collect();

        // Cap max_tokens to model's limit — deepseek-chat allows max 8192,
        // deepseek-reasoner allows 32768
        let model_max = match request.model.as_str() {
            "deepseek-reasoner" => 32_768,
            "deepseek-chat" => 8_192,
            _ => 8_192,
        };
        let max_tokens = request.max_tokens.min(model_max);

        let mut body = json!({
            "model": request.model,
            "max_tokens": max_tokens,
            "messages": messages,
        });

        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }

        body
    }

    fn parse_response(&self, status: u16, body: &Value) -> LlmResult<CompletionResponse> {
        if status == 401 || status == 403 {
            return Err(LlmError::AuthError);
        }
        if status == 429 {
            return Err(LlmError::RateLimited {
                retry_after_ms: 60_000,
            });
        }
        if status >= 400 {
            let message = body
                .pointer("/error/message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
                .to_string();
            return Err(LlmError::ApiError { status, message });
        }

        // OpenAI format: choices[0].message
        let choice = body
            .pointer("/choices/0/message")
            .ok_or_else(|| LlmError::ParseError("missing choices[0].message".into()))?;

        let mut blocks: Vec<ContentBlock> = Vec::new();

        // Extract text content
        if let Some(content) = choice.get("content").and_then(|c| c.as_str()) {
            if !content.is_empty() {
                blocks.push(ContentBlock::Text {
                    text: content.to_string(),
                });
            }
        }

        // Extract tool calls
        if let Some(tool_calls) = choice.get("tool_calls").and_then(|tc| tc.as_array()) {
            for call in tool_calls {
                let id = call
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = call
                    .pointer("/function/name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let arguments_str = call
                    .pointer("/function/arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}");
                let input: Value =
                    serde_json::from_str(arguments_str).unwrap_or(Value::Object(Default::default()));

                blocks.push(ContentBlock::ToolUse { id, name, input });
            }
        }

        // Map finish_reason to LlmStopReason
        let finish_reason = body
            .pointer("/choices/0/finish_reason")
            .and_then(|r| r.as_str())
            .unwrap_or("stop");

        let stop_reason = match finish_reason {
            "stop" => LlmStopReason::EndTurn,
            "tool_calls" => LlmStopReason::ToolUse,
            "length" => LlmStopReason::MaxTokens,
            "content_filter" => LlmStopReason::StopSequence,
            other => {
                return Err(LlmError::ParseError(format!(
                    "unknown finish_reason: {:?}",
                    other
                )))
            }
        };

        // OpenAI usage: prompt_tokens / completion_tokens
        let usage = if let Some(u) = body.get("usage") {
            TokenUsage {
                input_tokens: u
                    .get("prompt_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                output_tokens: u
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            }
        } else {
            TokenUsage::default()
        };

        Ok(CompletionResponse {
            content: blocks,
            stop_reason,
            usage,
        })
    }
}

#[async_trait]
impl LlmClient for OpenAiCompatibleClient {
    fn max_context_tokens(&self, model: &str) -> u32 {
        match model {
            "deepseek-reasoner" => 128_000,
            "deepseek-chat" => 128_000,
            _ => 128_000, // conservative default
        }
    }

    async fn complete(&self, request: CompletionRequest) -> LlmResult<CompletionResponse> {
        self.validate_model(&request.model)?;

        let url = format!("{}/v1/chat/completions", self.config.base_url);
        let body = self.build_request_body(&request);

        tracing::debug!(agent_id = %request.agent_id, model = %request.model, "calling OpenAI-compat LLM API");

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = response.status().as_u16();
        let response_body: Value = response.json().await?;

        self.parse_response(status, &response_body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_core::{AgentId, ToolDefinition};

    fn test_config() -> OpenAiCompatConfig {
        OpenAiCompatConfig {
            api_key: "test-key".into(),
            base_url: "https://api.deepseek.com".into(),
            default_max_tokens: 8192,
        }
    }

    fn client() -> OpenAiCompatibleClient {
        OpenAiCompatibleClient::new(test_config())
    }

    #[test]
    fn validate_supported_models() {
        let c = client();
        assert!(c.validate_model("deepseek-reasoner").is_ok());
        assert!(c.validate_model("deepseek-chat").is_ok());
    }

    #[test]
    fn validate_unsupported_model() {
        let c = client();
        let err = c.validate_model("gpt-4").unwrap_err();
        assert!(matches!(err, LlmError::UnsupportedModel { .. }));
    }

    #[test]
    fn system_prompt_injected_as_first_message() {
        let c = client();
        let request = CompletionRequest {
            agent_id: AgentId::new(),
            model: "deepseek-chat".into(),
            system: "You are helpful.".into(),
            messages: vec![Message::User { content: "Hello".into() }],
            tools: vec![],
            max_tokens: 1024,
        };
        let body = c.build_request_body(&request);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "You are helpful.");
        assert_eq!(msgs[1]["role"], "user");
    }

    #[test]
    fn tool_result_becomes_role_tool() {
        let c = client();
        let request = CompletionRequest {
            agent_id: AgentId::new(),
            model: "deepseek-chat".into(),
            system: "".into(),
            messages: vec![
                Message::Assistant {
                    content: vec![ContentBlock::ToolUse {
                        id: "call_1".into(),
                        name: "echo".into(),
                        input: json!({"message": "hi"}),
                    }],
                },
                Message::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: json!("hi"),
                    is_error: false,
                },
            ],
            tools: vec![],
            max_tokens: 1024,
        };
        let body = c.build_request_body(&request);
        let msgs = body["messages"].as_array().unwrap();
        // First message: assistant with tool_calls
        assert_eq!(msgs[0]["role"], "assistant");
        assert!(msgs[0].get("tool_calls").is_some());
        // Second message: tool result
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["tool_call_id"], "call_1");
    }

    #[test]
    fn assistant_tool_use_becomes_function_call() {
        let c = client();
        let request = CompletionRequest {
            agent_id: AgentId::new(),
            model: "deepseek-chat".into(),
            system: "".into(),
            messages: vec![Message::Assistant {
                content: vec![
                    ContentBlock::Text { text: "Let me fetch that.".into() },
                    ContentBlock::ToolUse {
                        id: "call_2".into(),
                        name: "web_fetch".into(),
                        input: json!({"url": "https://example.com"}),
                    },
                ],
            }],
            tools: vec![],
            max_tokens: 1024,
        };
        let body = c.build_request_body(&request);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["content"], "Let me fetch that.");
        let tool_calls = msgs[0]["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls[0]["id"], "call_2");
        assert_eq!(tool_calls[0]["type"], "function");
        assert_eq!(tool_calls[0]["function"]["name"], "web_fetch");
    }

    #[test]
    fn tools_formatted_as_functions() {
        let c = client();
        let request = CompletionRequest {
            agent_id: AgentId::new(),
            model: "deepseek-chat".into(),
            system: "".into(),
            messages: vec![Message::User { content: "hi".into() }],
            tools: vec![ToolDefinition {
                name: "echo".into(),
                description: "Echoes input".into(),
                input_schema: json!({"type": "object", "properties": {"message": {"type": "string"}}}),
            }],
            max_tokens: 1024,
        };
        let body = c.build_request_body(&request);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "echo");
    }

    #[test]
    fn parse_text_response() {
        let c = client();
        let body = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Hello!",
                },
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5},
        });
        let resp = c.parse_response(200, &body).unwrap();
        assert_eq!(resp.content.len(), 1);
        assert!(matches!(&resp.content[0], ContentBlock::Text { text } if text == "Hello!"));
        assert_eq!(resp.stop_reason, LlmStopReason::EndTurn);
        assert_eq!(resp.usage.input_tokens, 10);
        assert_eq!(resp.usage.output_tokens, 5);
    }

    #[test]
    fn parse_tool_call_response() {
        let c = client();
        let body = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {
                            "name": "web_fetch",
                            "arguments": "{\"url\":\"https://example.com\"}",
                        },
                    }],
                },
                "finish_reason": "tool_calls",
            }],
            "usage": {"prompt_tokens": 20, "completion_tokens": 15},
        });
        let resp = c.parse_response(200, &body).unwrap();
        assert_eq!(resp.stop_reason, LlmStopReason::ToolUse);
        assert_eq!(resp.content.len(), 1);
        assert!(matches!(&resp.content[0], ContentBlock::ToolUse { name, .. } if name == "web_fetch"));
    }

    #[test]
    fn parse_auth_error() {
        let c = client();
        let body = json!({"error": {"message": "invalid api key"}});
        let err = c.parse_response(401, &body).unwrap_err();
        assert!(matches!(err, LlmError::AuthError));
    }

    #[test]
    fn parse_rate_limit_error() {
        let c = client();
        let body = json!({"error": {"message": "rate limited"}});
        let err = c.parse_response(429, &body).unwrap_err();
        assert!(matches!(err, LlmError::RateLimited { .. }));
    }

    #[test]
    fn parse_api_error() {
        let c = client();
        let body = json!({"error": {"message": "server error"}});
        let err = c.parse_response(500, &body).unwrap_err();
        assert!(matches!(err, LlmError::ApiError { status: 500, .. }));
    }

    #[test]
    fn max_context_tokens_deepseek() {
        let c = client();
        assert_eq!(c.max_context_tokens("deepseek-reasoner"), 128_000);
        assert_eq!(c.max_context_tokens("deepseek-chat"), 128_000);
    }
}
