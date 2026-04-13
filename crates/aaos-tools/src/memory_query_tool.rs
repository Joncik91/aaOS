use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use aaos_core::{AuditEvent, AuditEventKind, AuditLog, CoreError, Result, ToolDefinition};
use aaos_memory::{EmbeddingSource, MemoryCategory, MemoryStore};

use crate::context::InvocationContext;
use crate::tool::Tool;

pub struct MemoryQueryTool {
    memory_store: Arc<dyn MemoryStore>,
    embedding_source: Arc<dyn EmbeddingSource>,
    audit_log: Arc<dyn AuditLog>,
}

impl MemoryQueryTool {
    pub fn new(
        memory_store: Arc<dyn MemoryStore>,
        embedding_source: Arc<dyn EmbeddingSource>,
        audit_log: Arc<dyn AuditLog>,
    ) -> Self {
        Self {
            memory_store,
            embedding_source,
            audit_log,
        }
    }
}

#[async_trait]
impl Tool for MemoryQueryTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "memory_query".to_string(),
            description:
                "Search your stored memories by meaning. Returns the most relevant memories."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What to search for" },
                    "limit": {
                        "type": "integer",
                        "description": "Max results (1-20, default 5)"
                    },
                    "category": {
                        "type": "string",
                        "enum": ["fact", "observation", "decision", "preference"],
                        "description": "Optional category filter"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn invoke(&self, input: Value, ctx: &InvocationContext) -> Result<Value> {
        let query = input["query"]
            .as_str()
            .ok_or_else(|| CoreError::SchemaValidation("missing 'query'".into()))?;

        let limit = input
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| (v as usize).clamp(1, 20))
            .unwrap_or(5);

        let category: Option<MemoryCategory> = input
            .get("category")
            .and_then(|v| v.as_str())
            .map(|s| serde_json::from_value(json!(s)))
            .transpose()
            .map_err(|_| CoreError::SchemaValidation("invalid category".into()))?;

        // Embed the query
        let query_embedding = self
            .embedding_source
            .embed(query)
            .await
            .map_err(|e| CoreError::Ipc(format!("embedding error: {e}")))?;

        // Search
        let results = self
            .memory_store
            .query(&ctx.agent_id, &query_embedding, limit, category)
            .await
            .map_err(|e| CoreError::Ipc(format!("memory query error: {e}")))?;

        // Audit with query hash (not raw query)
        let mut hasher = Sha256::new();
        hasher.update(query.as_bytes());
        let query_hash = format!("{:x}", hasher.finalize());

        self.audit_log.record(AuditEvent::new(
            ctx.agent_id,
            AuditEventKind::MemoryQueried {
                query_hash,
                results_count: results.len(),
            },
        ));

        let results_json: Vec<Value> = results
            .iter()
            .map(|r| {
                json!({
                    "memory_id": r.id.to_string(),
                    "content": r.content,
                    "category": r.category,
                    "relevance_score": r.relevance_score,
                    "created_at": r.created_at.to_rfc3339(),
                })
            })
            .collect();

        Ok(json!({
            "results": results_json,
            "count": results_json.len(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_core::{AgentId, InMemoryAuditLog};
    use aaos_memory::{InMemoryMemoryStore, MockEmbeddingSource};

    use crate::memory_store_tool::MemoryStoreTool;

    fn setup() -> (
        MemoryQueryTool,
        MemoryStoreTool,
        InvocationContext,
        Arc<InMemoryAuditLog>,
    ) {
        let store = Arc::new(InMemoryMemoryStore::new(100, 64, "mock-embed"));
        let embedding = Arc::new(MockEmbeddingSource::new(64));
        let audit = Arc::new(InMemoryAuditLog::new());
        let query_tool = MemoryQueryTool::new(store.clone(), embedding.clone(), audit.clone());
        let store_tool = MemoryStoreTool::new(store, embedding, audit.clone(), 4096);
        let ctx = InvocationContext {
            agent_id: AgentId::new(),
            tokens: vec![],
        };
        (query_tool, store_tool, ctx, audit)
    }

    #[tokio::test]
    async fn query_after_store_returns_results() {
        let (query_tool, store_tool, ctx, _) = setup();

        // Store a memory first
        store_tool
            .invoke(
                json!({"content": "Rust is a systems language", "category": "fact"}),
                &ctx,
            )
            .await
            .unwrap();

        // Query for it
        let result = query_tool
            .invoke(json!({"query": "Rust programming"}), &ctx)
            .await
            .unwrap();

        assert!(result["count"].as_u64().unwrap() >= 1);
        let results = result["results"].as_array().unwrap();
        assert!(!results.is_empty());
        assert!(results[0]["content"].as_str().is_some());
        assert!(results[0]["relevance_score"].as_f64().is_some());
    }

    #[tokio::test]
    async fn query_empty_store_returns_empty() {
        let (query_tool, _, ctx, _) = setup();

        let result = query_tool
            .invoke(json!({"query": "anything"}), &ctx)
            .await
            .unwrap();

        assert_eq!(result["count"], 0);
        assert!(result["results"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn query_category_filter() {
        let (query_tool, store_tool, ctx, _) = setup();

        store_tool
            .invoke(
                json!({"content": "I prefer dark mode", "category": "preference"}),
                &ctx,
            )
            .await
            .unwrap();
        store_tool
            .invoke(
                json!({"content": "The database has 5 tables", "category": "fact"}),
                &ctx,
            )
            .await
            .unwrap();

        // Query with category filter
        let result = query_tool
            .invoke(
                json!({"query": "database", "category": "fact"}),
                &ctx,
            )
            .await
            .unwrap();

        let results = result["results"].as_array().unwrap();
        // Only the fact should be returned
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["content"], "The database has 5 tables");
    }

    #[tokio::test]
    async fn query_missing_query_param() {
        let (query_tool, _, ctx, _) = setup();
        let result = query_tool.invoke(json!({}), &ctx).await;
        assert!(result.is_err());
    }
}
