use aaos_core::{AgentId, TokenUsage, ToolDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Request to the LLM completion API.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub agent_id: AgentId,
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: u32,
}

/// Response from the LLM completion API.
#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: LlmStopReason,
    pub usage: TokenUsage,
}

/// A block of content in an LLM response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
}

/// Stop reason from the LLM API response.
/// Named `LlmStopReason` to distinguish from `aaos_core::StopReason` (agent lifecycle).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmStopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
}

/// A message in the conversation history.
#[derive(Debug, Clone)]
pub enum Message {
    User {
        content: String,
    },
    Assistant {
        content: Vec<ContentBlock>,
    },
    ToolResult {
        tool_use_id: String,
        content: Value,
        is_error: bool,
    },
}
