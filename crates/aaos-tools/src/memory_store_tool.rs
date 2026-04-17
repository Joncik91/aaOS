use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use aaos_core::{AuditEvent, AuditEventKind, AuditLog, CoreError, Result, ToolDefinition};
use aaos_memory::{EmbeddingSource, MemoryCategory, MemoryRecord, MemoryScope, MemoryStore};

use crate::context::InvocationContext;
use crate::tool::Tool;

pub struct MemoryStoreTool {
    memory_store: Arc<dyn MemoryStore>,
    embedding_source: Arc<dyn EmbeddingSource>,
    audit_log: Arc<dyn AuditLog>,
    max_content_bytes: usize,
}

impl MemoryStoreTool {
    pub fn new(
        memory_store: Arc<dyn MemoryStore>,
        embedding_source: Arc<dyn EmbeddingSource>,
        audit_log: Arc<dyn AuditLog>,
        max_content_bytes: usize,
    ) -> Self {
        Self {
            memory_store,
            embedding_source,
            audit_log,
            max_content_bytes,
        }
    }
}

#[async_trait]
impl Tool for MemoryStoreTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "memory_store".to_string(),
            description: "Store a fact, observation, decision, or preference for later retrieval."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "The memory to store" },
                    "category": {
                        "type": "string",
                        "enum": ["fact", "observation", "decision", "preference"],
                        "description": "Memory category"
                    },
                    "replaces": {
                        "type": "string",
                        "description": "Optional: UUID of a memory this replaces"
                    }
                },
                "required": ["content", "category"]
            }),
        }
    }

    async fn invoke(&self, input: Value, ctx: &InvocationContext) -> Result<Value> {
        let content = input["content"]
            .as_str()
            .ok_or_else(|| CoreError::SchemaValidation("missing 'content'".into()))?;
        let category_str = input["category"]
            .as_str()
            .ok_or_else(|| CoreError::SchemaValidation("missing 'category'".into()))?;

        // Validate content size
        if content.len() > self.max_content_bytes {
            return Err(CoreError::SchemaValidation(format!(
                "content too large: {} bytes exceeds max {} bytes",
                content.len(),
                self.max_content_bytes
            )));
        }

        let category: MemoryCategory =
            serde_json::from_value(json!(category_str)).map_err(|_| {
                CoreError::SchemaValidation(format!("invalid category: {category_str}"))
            })?;

        let replaces = input
            .get("replaces")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok());

        // Generate embedding
        let embedding = self
            .embedding_source
            .embed(content)
            .await
            .map_err(|e| CoreError::Ipc(format!("embedding error: {e}")))?;

        let record = MemoryRecord {
            id: Uuid::new_v4(),
            agent_id: ctx.agent_id,
            content: content.to_string(),
            category: category.clone(),
            scope: MemoryScope::Private,
            metadata: HashMap::new(),
            created_at: Utc::now(),
            replaces,
            embedding,
            embedding_model: self.embedding_source.model_name().to_string(),
        };

        let memory_id = self
            .memory_store
            .store(record)
            .await
            .map_err(|e| CoreError::Ipc(format!("memory store error: {e}")))?;

        // Audit with content hash (not raw content)
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        let content_hash = format!("{:x}", hasher.finalize());

        self.audit_log.record(AuditEvent::new(
            ctx.agent_id,
            AuditEventKind::MemoryStored {
                memory_id,
                category: category_str.to_string(),
                content_hash,
            },
        ));

        Ok(json!({
            "memory_id": memory_id.to_string(),
            "status": "stored"
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_core::{AgentId, InMemoryAuditLog};
    use aaos_memory::{InMemoryMemoryStore, MockEmbeddingSource};

    fn setup() -> (MemoryStoreTool, InvocationContext, Arc<InMemoryAuditLog>) {
        let store = Arc::new(InMemoryMemoryStore::new(100, 64, "mock-embed"));
        let embedding = Arc::new(MockEmbeddingSource::new(64));
        let audit = Arc::new(InMemoryAuditLog::new());
        let tool = MemoryStoreTool::new(store, embedding, audit.clone(), 4096);
        let ctx = InvocationContext {
            agent_id: AgentId::new(),
            tokens: vec![],
            capability_registry: Arc::new(aaos_core::CapabilityRegistry::new()),
        };
        (tool, ctx, audit)
    }

    #[tokio::test]
    async fn store_fact_returns_memory_id() {
        let (tool, ctx, audit) = setup();
        let result = tool
            .invoke(
                json!({"content": "The sky is blue", "category": "fact"}),
                &ctx,
            )
            .await
            .unwrap();

        assert_eq!(result["status"], "stored");
        assert!(result["memory_id"].as_str().is_some());
        // Verify UUID parses
        let id_str = result["memory_id"].as_str().unwrap();
        Uuid::parse_str(id_str).unwrap();
        // Audit event recorded
        assert_eq!(audit.len(), 1);
    }

    #[tokio::test]
    async fn store_content_too_large() {
        let (tool, ctx, _) = setup();
        let big_content = "x".repeat(5000); // max is 4096
        let result = tool
            .invoke(json!({"content": big_content, "category": "fact"}), &ctx)
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("too large"));
    }

    #[tokio::test]
    async fn store_invalid_category() {
        let (tool, ctx, _) = setup();
        let result = tool
            .invoke(json!({"content": "test", "category": "invalid_cat"}), &ctx)
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("invalid category"));
    }

    #[tokio::test]
    async fn store_missing_content() {
        let (tool, ctx, _) = setup();
        let result = tool.invoke(json!({"category": "fact"}), &ctx).await;

        assert!(result.is_err());
    }
}
