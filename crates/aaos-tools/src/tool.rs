use async_trait::async_trait;
use serde_json::Value;

use aaos_core::{Result, ToolDefinition};

use crate::context::InvocationContext;

/// Trait that all tools must implement.
///
/// A tool is any capability that agents can invoke — file operations,
/// web searches, API calls, spawning other agents, etc. Every tool
/// has a typed schema and is invoked through the tool registry.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The tool's definition (name, description, input schema).
    fn definition(&self) -> ToolDefinition;

    /// Invoke the tool with the given input and invocation context.
    async fn invoke(&self, input: Value, ctx: &InvocationContext) -> Result<Value>;
}

/// A simple echo tool for testing — returns its input as output.
pub struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "echo".to_string(),
            description: "Returns the input as-is. Useful for testing.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" }
                },
                "required": ["message"]
            }),
        }
    }

    async fn invoke(&self, input: Value, _ctx: &InvocationContext) -> Result<Value> {
        Ok(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_core::AgentId;

    fn dummy_ctx() -> InvocationContext {
        InvocationContext {
            agent_id: AgentId::new(),
            tokens: vec![],
            capability_registry: std::sync::Arc::new(aaos_core::CapabilityRegistry::new()),
        }
    }

    #[tokio::test]
    async fn echo_tool_returns_input() {
        let tool = EchoTool;
        let input = serde_json::json!({"message": "hello world"});
        let output = tool.invoke(input.clone(), &dummy_ctx()).await.unwrap();
        assert_eq!(input, output);
    }

    #[test]
    fn echo_tool_definition() {
        let tool = EchoTool;
        let def = tool.definition();
        assert_eq!(def.name, "echo");
        assert!(def.input_schema.get("required").is_some());
    }
}
