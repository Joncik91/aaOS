use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use uuid::Uuid;

use aaos_core::{AuditEvent, AuditEventKind, AuditLog, CoreError, Result, ToolDefinition};
use aaos_memory::MemoryStore;

use crate::context::InvocationContext;
use crate::tool::Tool;

pub struct MemoryDeleteTool {
    memory_store: Arc<dyn MemoryStore>,
    audit_log: Arc<dyn AuditLog>,
}

impl MemoryDeleteTool {
    pub fn new(memory_store: Arc<dyn MemoryStore>, audit_log: Arc<dyn AuditLog>) -> Self {
        Self {
            memory_store,
            audit_log,
        }
    }
}

#[async_trait]
impl Tool for MemoryDeleteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "memory_delete".to_string(),
            description: "Delete a specific stored memory by ID.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "memory_id": {
                        "type": "string",
                        "description": "UUID of the memory to delete"
                    }
                },
                "required": ["memory_id"]
            }),
        }
    }

    async fn invoke(&self, input: Value, ctx: &InvocationContext) -> Result<Value> {
        let memory_id_str = input["memory_id"]
            .as_str()
            .ok_or_else(|| CoreError::SchemaValidation("missing 'memory_id'".into()))?;
        let memory_id = Uuid::parse_str(memory_id_str)
            .map_err(|_| CoreError::SchemaValidation(format!("invalid UUID: {memory_id_str}")))?;

        self.memory_store
            .delete(&ctx.agent_id, &memory_id)
            .await
            .map_err(|e| CoreError::Ipc(format!("memory delete error: {e}")))?;

        // Audit the deletion
        self.audit_log.record(AuditEvent::new(
            ctx.agent_id,
            AuditEventKind::ToolInvoked {
                tool: "memory_delete".into(),
                input_hash: memory_id_str.to_string(),
                args_preview: None,
            },
        ));

        Ok(json!({
            "memory_id": memory_id_str,
            "status": "deleted"
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_core::{AgentId, InMemoryAuditLog};
    use aaos_memory::{InMemoryMemoryStore, MockEmbeddingSource};

    use crate::memory_store_tool::MemoryStoreTool;
    use crate::tool::Tool;

    fn setup() -> (
        MemoryDeleteTool,
        MemoryStoreTool,
        InvocationContext,
        Arc<InMemoryAuditLog>,
    ) {
        let store = Arc::new(InMemoryMemoryStore::new(100, 64, "mock-embed"));
        let embedding = Arc::new(MockEmbeddingSource::new(64));
        let audit = Arc::new(InMemoryAuditLog::new());
        let delete_tool = MemoryDeleteTool::new(store.clone(), audit.clone());
        let store_tool = MemoryStoreTool::new(store, embedding, audit.clone(), 4096);
        let ctx = InvocationContext {
            agent_id: AgentId::new(),
            tokens: vec![],
            capability_registry: Arc::new(aaos_core::CapabilityRegistry::new()),
        };
        (delete_tool, store_tool, ctx, audit)
    }

    #[tokio::test]
    async fn delete_existing_memory() {
        let (delete_tool, store_tool, ctx, _) = setup();

        // Store a memory
        let store_result = store_tool
            .invoke(
                json!({"content": "to be deleted", "category": "fact"}),
                &ctx,
            )
            .await
            .unwrap();
        let memory_id = store_result["memory_id"].as_str().unwrap();

        // Delete it
        let result = delete_tool
            .invoke(json!({"memory_id": memory_id}), &ctx)
            .await
            .unwrap();

        assert_eq!(result["status"], "deleted");
        assert_eq!(result["memory_id"], memory_id);
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_error() {
        let (delete_tool, _, ctx, _) = setup();

        let fake_id = Uuid::new_v4().to_string();
        let result = delete_tool
            .invoke(json!({"memory_id": fake_id}), &ctx)
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not found"));
    }

    #[tokio::test]
    async fn delete_invalid_uuid() {
        let (delete_tool, _, ctx, _) = setup();

        let result = delete_tool
            .invoke(json!({"memory_id": "not-a-uuid"}), &ctx)
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("invalid UUID"));
    }

    #[tokio::test]
    async fn delete_missing_memory_id() {
        let (delete_tool, _, ctx, _) = setup();
        let result = delete_tool.invoke(json!({}), &ctx).await;
        assert!(result.is_err());
    }
}
