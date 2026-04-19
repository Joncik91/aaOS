use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use aaos_core::{CoreError, Result};

use crate::tool::Tool;
use aaos_core::ToolDefinition;

/// System-wide registry of available tools.
///
/// Every capability in aaOS is a discoverable, callable tool with a
/// typed schema. Agents discover tools through the registry and invoke
/// them via the tool invocation layer (which enforces capabilities).
pub struct ToolRegistry {
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
        }
    }

    /// Register a tool. Replaces any existing tool with the same name.
    pub fn register(&self, tool: Arc<dyn Tool>) {
        let name = tool.definition().name.clone();
        tracing::info!(tool = %name, "tool registered");
        self.tools.write().unwrap().insert(name, tool);
    }

    /// Get a tool by name.
    pub fn get(&self, name: &str) -> Result<Arc<dyn Tool>> {
        self.tools
            .read()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| CoreError::ToolNotFound(name.to_string()))
    }

    /// List all registered tool definitions.
    pub fn list(&self) -> Vec<ToolDefinition> {
        self.tools
            .read()
            .unwrap()
            .values()
            .map(|t| t.definition())
            .collect()
    }

    /// Number of registered tools.
    pub fn count(&self) -> usize {
        self.tools.read().unwrap().len()
    }

    /// Register a tool under an explicit name, overriding the name from its
    /// `definition()`. Useful in tests to reuse stub tool implementations
    /// under multiple names (e.g. EchoTool registered as "file_write").
    #[cfg(test)]
    pub fn register_as(&self, tool: Arc<dyn Tool>, name: &str) {
        self.tools.write().unwrap().insert(name.to_string(), tool);
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::EchoTool;

    #[test]
    fn register_and_get() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool));

        let tool = registry.get("echo").unwrap();
        assert_eq!(tool.definition().name, "echo");
    }

    #[test]
    fn list_tools() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool));

        let tools = registry.list();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
    }

    #[test]
    fn get_nonexistent() {
        let registry = ToolRegistry::new();
        assert!(registry.get("nonexistent").is_err());
    }
}
