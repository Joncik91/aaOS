use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Definition of a tool's interface — its name, description, and input schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}
