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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_user_roundtrips_json() {
        let msg = Message::User { content: "hello".into() };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();
        match parsed {
            Message::User { content } => assert_eq!(content, "hello"),
            _ => panic!("expected User variant"),
        }
    }

    #[test]
    fn message_assistant_roundtrips_json() {
        let msg = Message::Assistant {
            content: vec![ContentBlock::Text { text: "hi".into() }],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();
        match parsed {
            Message::Assistant { content } => {
                assert_eq!(content.len(), 1);
                match &content[0] {
                    ContentBlock::Text { text } => assert_eq!(text, "hi"),
                    _ => panic!("expected Text block"),
                }
            }
            _ => panic!("expected Assistant variant"),
        }
    }

    #[test]
    fn message_tool_result_roundtrips_json() {
        let msg = Message::ToolResult {
            tool_use_id: "call_1".into(),
            content: serde_json::json!({"result": 42}),
            is_error: false,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();
        match parsed {
            Message::ToolResult { tool_use_id, is_error, .. } => {
                assert_eq!(tool_use_id, "call_1");
                assert!(!is_error);
            }
            _ => panic!("expected ToolResult variant"),
        }
    }
}
