use reqwest::Client;
use serde_json::{json, Value};

use aaos_core::TokenUsage;
use async_trait::async_trait;

use crate::client::LlmClient;
use crate::error::{LlmError, LlmResult};
use crate::types::{CompletionRequest, CompletionResponse, ContentBlock, LlmStopReason, Message};

const SUPPORTED_MODELS: &[&str] = &[
    "claude-haiku-4-5-20251001",
    "claude-haiku-4-5-20251001",
    "claude-opus-4-20250514",
    "claude-opus-4-6-20250616",
    "claude-sonnet-4-6-20250725",
];

/// Configuration for the Anthropic API client.
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub api_key: String,
    pub base_url: String,
    pub default_max_tokens: u32,
}

impl AnthropicConfig {
    /// Load configuration from environment variables.
    /// API key from ANTHROPIC_API_KEY (required).
    /// Base URL from ANTHROPIC_BASE_URL (optional, defaults to https://api.anthropic.com).
    pub fn from_env() -> LlmResult<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| LlmError::AuthError)?;
        let base_url = std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
        Ok(Self {
            api_key,
            base_url,
            default_max_tokens: 4096,
        })
    }
}

/// Anthropic Messages API client.
pub struct AnthropicClient {
    config: AnthropicConfig,
    http: Client,
}

impl AnthropicClient {
    pub fn new(config: AnthropicConfig) -> Self {
        Self {
            config,
            http: Client::new(),
        }
    }

    fn validate_model(&self, model: &str) -> LlmResult<()> {
        if !SUPPORTED_MODELS.contains(&model) {
            return Err(LlmError::UnsupportedModel {
                model: model.to_string(),
            });
        }
        Ok(())
    }

    fn build_request_body(&self, request: &CompletionRequest) -> Value {
        let messages: Vec<Value> = request
            .messages
            .iter()
            .map(|msg| match msg {
                Message::User { content } => json!({
                    "role": "user",
                    "content": content,
                }),
                Message::Assistant { content } => {
                    let blocks: Vec<Value> = content
                        .iter()
                        .map(|block| match block {
                            ContentBlock::Text { text } => json!({
                                "type": "text",
                                "text": text,
                            }),
                            ContentBlock::ToolUse { id, name, input } => json!({
                                "type": "tool_use",
                                "id": id,
                                "name": name,
                                "input": input,
                            }),
                        })
                        .collect();
                    json!({ "role": "assistant", "content": blocks })
                }
                Message::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": content.to_string(),
                        "is_error": is_error,
                    }],
                }),
                Message::Summary { .. } => {
                    // Summary messages are folded into the system prompt by ContextManager.
                    // They should never appear in the messages array sent to the API.
                    panic!("BUG: Message::Summary must not be sent to the LLM API directly; fold into system prompt via ContextManager")
                }
            })
            .collect();

        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();

        let mut body = json!({
            "model": request.model,
            "max_tokens": request.max_tokens,
            "system": request.system,
            "messages": messages,
        });

        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }

        body
    }

    fn parse_response(&self, status: u16, body: &Value) -> LlmResult<CompletionResponse> {
        if status == 401 {
            return Err(LlmError::AuthError);
        }
        if status == 429 {
            let _retry_after = body
                .pointer("/error/message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown");
            return Err(LlmError::RateLimited {
                retry_after_ms: 60_000, // Default 60s if not parseable
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

        let content = body
            .get("content")
            .and_then(|c| c.as_array())
            .ok_or_else(|| LlmError::ParseError("missing 'content' array".into()))?;

        let blocks: Vec<ContentBlock> = content
            .iter()
            .filter_map(|block| {
                let block_type = block.get("type")?.as_str()?;
                match block_type {
                    "text" => Some(ContentBlock::Text {
                        text: block.get("text")?.as_str()?.to_string(),
                    }),
                    "tool_use" => Some(ContentBlock::ToolUse {
                        id: block.get("id")?.as_str()?.to_string(),
                        name: block.get("name")?.as_str()?.to_string(),
                        input: block.get("input")?.clone(),
                    }),
                    _ => None,
                }
            })
            .collect();

        let stop_reason = match body.get("stop_reason").and_then(|s| s.as_str()) {
            Some("end_turn") => LlmStopReason::EndTurn,
            Some("tool_use") => LlmStopReason::ToolUse,
            Some("max_tokens") => LlmStopReason::MaxTokens,
            Some("stop_sequence") => LlmStopReason::StopSequence,
            other => {
                return Err(LlmError::ParseError(format!(
                    "unknown stop_reason: {:?}",
                    other
                )))
            }
        };

        let usage = if let Some(u) = body.get("usage") {
            TokenUsage {
                input_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                output_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
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
impl LlmClient for AnthropicClient {
    fn max_context_tokens(&self, model: &str) -> u32 {
        match model {
            m if m.contains("haiku") => 200_000,
            m if m.contains("sonnet") => 200_000,
            m if m.contains("opus") => 200_000,
            _ => 128_000, // conservative default for unknown models
        }
    }

    async fn complete(&self, request: CompletionRequest) -> LlmResult<CompletionResponse> {
        self.validate_model(&request.model)?;

        let url = format!("{}/v1/messages", self.config.base_url);
        let body = self.build_request_body(&request);

        tracing::debug!(agent_id = %request.agent_id, model = %request.model, "calling LLM API");

        let response = self
            .http
            .post(&url)
            .header("x-api-key", &self.config.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
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

    fn test_config() -> AnthropicConfig {
        AnthropicConfig {
            api_key: "test-key".into(),
            base_url: "https://api.anthropic.com".into(),
            default_max_tokens: 4096,
        }
    }

    #[test]
    fn validate_supported_model() {
        let client = AnthropicClient::new(test_config());
        assert!(client.validate_model("claude-haiku-4-5-20251001").is_ok());
    }

    #[test]
    fn validate_unsupported_model() {
        let client = AnthropicClient::new(test_config());
        let err = client.validate_model("gpt-4").unwrap_err();
        assert!(matches!(err, LlmError::UnsupportedModel { .. }));
    }

    #[test]
    fn build_request_body_basic() {
        let client = AnthropicClient::new(test_config());
        let request = CompletionRequest {
            agent_id: AgentId::new(),
            model: "claude-haiku-4-5-20251001".into(),
            system: "You are helpful.".into(),
            messages: vec![Message::User {
                content: "Hello".into(),
            }],
            tools: vec![],
            max_tokens: 1024,
        };
        let body = client.build_request_body(&request);
        assert_eq!(body["model"], "claude-haiku-4-5-20251001");
        assert_eq!(body["system"], "You are helpful.");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "Hello");
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn build_request_body_with_tools() {
        let client = AnthropicClient::new(test_config());
        let request = CompletionRequest {
            agent_id: AgentId::new(),
            model: "claude-haiku-4-5-20251001".into(),
            system: "test".into(),
            messages: vec![Message::User {
                content: "hi".into(),
            }],
            tools: vec![ToolDefinition {
                name: "echo".into(),
                description: "Echoes input".into(),
                input_schema: json!({"type": "object"}),
            }],
            max_tokens: 1024,
        };
        let body = client.build_request_body(&request);
        assert_eq!(body["tools"][0]["name"], "echo");
    }

    #[test]
    fn parse_text_response() {
        let client = AnthropicClient::new(test_config());
        let body = json!({
            "content": [{"type": "text", "text": "Hello!"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let resp = client.parse_response(200, &body).unwrap();
        assert_eq!(resp.content.len(), 1);
        assert!(matches!(&resp.content[0], ContentBlock::Text { text } if text == "Hello!"));
        assert_eq!(resp.stop_reason, LlmStopReason::EndTurn);
        assert_eq!(resp.usage.input_tokens, 10);
        assert_eq!(resp.usage.output_tokens, 5);
    }

    #[test]
    fn parse_tool_use_response() {
        let client = AnthropicClient::new(test_config());
        let body = json!({
            "content": [
                {"type": "text", "text": "Let me echo that."},
                {"type": "tool_use", "id": "call_1", "name": "echo", "input": {"message": "hi"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 20, "output_tokens": 15}
        });
        let resp = client.parse_response(200, &body).unwrap();
        assert_eq!(resp.content.len(), 2);
        assert!(matches!(&resp.content[1], ContentBlock::ToolUse { name, .. } if name == "echo"));
        assert_eq!(resp.stop_reason, LlmStopReason::ToolUse);
    }

    #[test]
    fn parse_auth_error() {
        let client = AnthropicClient::new(test_config());
        let body = json!({"error": {"message": "invalid api key"}});
        let err = client.parse_response(401, &body).unwrap_err();
        assert!(matches!(err, LlmError::AuthError));
    }

    #[test]
    fn parse_rate_limit_error() {
        let client = AnthropicClient::new(test_config());
        let body = json!({"error": {"message": "rate limited"}});
        let err = client.parse_response(429, &body).unwrap_err();
        assert!(matches!(err, LlmError::RateLimited { .. }));
    }

    #[test]
    fn parse_api_error() {
        let client = AnthropicClient::new(test_config());
        let body = json!({"error": {"message": "overloaded"}});
        let err = client.parse_response(529, &body).unwrap_err();
        assert!(matches!(err, LlmError::ApiError { status: 529, .. }));
    }
}
