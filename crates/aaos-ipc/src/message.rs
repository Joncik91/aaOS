use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use aaos_core::AgentId;

/// JSON-RPC 2.0 request, adapted for aaOS inter-agent communication.
///
/// Every message in aaOS is a typed, schema-validated structured message
/// following the MCP (Model Context Protocol) pattern. No raw text pipes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpMessage {
    /// JSON-RPC version (always "2.0")
    pub jsonrpc: String,
    /// Unique message ID
    pub id: Uuid,
    /// The method being called
    pub method: String,
    /// Method parameters
    #[serde(default)]
    pub params: serde_json::Value,
    /// aaOS metadata envelope
    pub metadata: MessageMetadata,
}

/// aaOS-specific metadata attached to every MCP message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageMetadata {
    pub sender: AgentId,
    pub recipient: AgentId,
    pub timestamp: DateTime<Utc>,
    /// Trace ID for request-level correlation
    pub trace_id: Uuid,
    /// The capability token ID authorizing this message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capability_token_id: Option<Uuid>,
}

/// Response to an MCP message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResponse {
    pub jsonrpc: String,
    pub id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<McpError>,
    pub metadata: ResponseMetadata,
}

/// Error in an MCP response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Metadata for a response message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseMetadata {
    pub responder: AgentId,
    pub timestamp: DateTime<Utc>,
    pub trace_id: Uuid,
}

impl McpMessage {
    /// Create a new MCP message.
    pub fn new(
        sender: AgentId,
        recipient: AgentId,
        method: impl Into<String>,
        params: serde_json::Value,
    ) -> Self {
        let trace_id = Uuid::new_v4();
        Self {
            jsonrpc: "2.0".to_string(),
            id: Uuid::new_v4(),
            method: method.into(),
            params,
            metadata: MessageMetadata {
                sender,
                recipient,
                timestamp: Utc::now(),
                trace_id,
                capability_token_id: None,
            },
        }
    }

    /// Attach a capability token ID to this message.
    pub fn with_capability_token(mut self, token_id: Uuid) -> Self {
        self.metadata.capability_token_id = Some(token_id);
        self
    }

    /// Create a success response to this message.
    pub fn respond_ok(&self, responder: AgentId, result: serde_json::Value) -> McpResponse {
        McpResponse {
            jsonrpc: "2.0".to_string(),
            id: self.id,
            result: Some(result),
            error: None,
            metadata: ResponseMetadata {
                responder,
                timestamp: Utc::now(),
                trace_id: self.metadata.trace_id,
            },
        }
    }

    /// Create an error response to this message.
    pub fn respond_err(
        &self,
        responder: AgentId,
        code: i32,
        message: impl Into<String>,
    ) -> McpResponse {
        McpResponse {
            jsonrpc: "2.0".to_string(),
            id: self.id,
            result: None,
            error: Some(McpError {
                code,
                message: message.into(),
                data: None,
            }),
            metadata: ResponseMetadata {
                responder,
                timestamp: Utc::now(),
                trace_id: self.metadata.trace_id,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_respond() {
        let sender = AgentId::new();
        let recipient = AgentId::new();
        let msg = McpMessage::new(
            sender,
            recipient,
            "tools/call",
            serde_json::json!({"tool": "search"}),
        );

        assert_eq!(msg.jsonrpc, "2.0");
        assert_eq!(msg.metadata.sender, sender);

        let resp = msg.respond_ok(recipient, serde_json::json!({"result": "found"}));
        assert_eq!(resp.id, msg.id);
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn error_response() {
        let msg = McpMessage::new(
            AgentId::new(),
            AgentId::new(),
            "test",
            serde_json::json!({}),
        );
        let resp = msg.respond_err(AgentId::new(), -32600, "Invalid request");
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32600);
    }

    #[test]
    fn message_roundtrips_json() {
        let msg = McpMessage::new(
            AgentId::new(),
            AgentId::new(),
            "tools/call",
            serde_json::json!({"name": "web_search", "arguments": {"query": "test"}}),
        );
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: McpMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg.id, parsed.id);
        assert_eq!(msg.method, parsed.method);
    }
}
