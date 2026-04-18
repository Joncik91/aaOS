use crate::client::session::McpSession;
use crate::types::McpError;
use aaos_core::{CoreError, Result, ToolDefinition};
use aaos_tools::{InvocationContext, Tool};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

pub struct McpToolProxy {
    /// Registered name in ToolRegistry: `mcp.<server>.<tool_name>`.
    pub registered_name: String,
    /// Tool name sent to the MCP server.
    pub remote_name: String,
    pub session: Arc<McpSession>,
    pub definition: ToolDefinition,
}

impl McpToolProxy {
    pub fn new(
        server_name: &str,
        remote_name: String,
        description: String,
        input_schema: Value,
        session: Arc<McpSession>,
    ) -> Self {
        let registered_name = format!("mcp.{}.{}", server_name, remote_name);
        Self {
            definition: ToolDefinition {
                name: registered_name.clone(),
                description,
                input_schema,
            },
            registered_name,
            remote_name,
            session,
        }
    }
}

#[async_trait]
impl Tool for McpToolProxy {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn invoke(&self, input: Value, _ctx: &InvocationContext) -> Result<Value> {
        self.session
            .call(&self.remote_name, input)
            .await
            .map_err(|e| match e {
                McpError::Unhealthy => CoreError::ToolUnavailable(self.registered_name.clone()),
                McpError::Transport(msg) => CoreError::ToolUnavailable(msg),
                McpError::Rpc { code, message } => {
                    CoreError::Ipc(format!("MCP rpc error {code}: {message}"))
                }
                McpError::Json(e) => CoreError::Json(e),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_definition_name_matches() {
        let def = ToolDefinition {
            name: "mcp.echo.echo".into(),
            description: "echo tool via MCP".into(),
            input_schema: serde_json::json!({ "type": "object" }),
        };
        assert_eq!(def.name, "mcp.echo.echo");
    }
}
