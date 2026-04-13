use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use aaos_core::AgentId;

/// A stored memory record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: Uuid,
    pub agent_id: AgentId,
    pub content: String,
    pub category: MemoryCategory,
    pub scope: MemoryScope,
    pub metadata: HashMap<String, Value>,
    pub created_at: DateTime<Utc>,
    pub replaces: Option<Uuid>,
    pub embedding: Vec<f32>,
    pub embedding_model: String,
}

/// Memory categories for semantic organization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCategory {
    Fact,
    Observation,
    Decision,
    Preference,
}

/// Memory scope — Private (C2) or Shared (future C3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    #[default]
    Private,
    // Future C3: Shared { topics: Vec<String> }
}

/// Query result with relevance score. Embedding vectors stripped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryResult {
    pub id: Uuid,
    pub content: String,
    pub category: MemoryCategory,
    pub metadata: HashMap<String, Value>,
    pub created_at: DateTime<Utc>,
    pub relevance_score: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_category_serde_roundtrip() {
        let cat = MemoryCategory::Fact;
        let json = serde_json::to_string(&cat).unwrap();
        assert_eq!(json, "\"fact\"");
        let parsed: MemoryCategory = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, MemoryCategory::Fact);
    }

    #[test]
    fn memory_scope_default_is_private() {
        let scope = MemoryScope::default();
        assert_eq!(scope, MemoryScope::Private);
    }

    #[test]
    fn memory_record_serde_roundtrip() {
        let record = MemoryRecord {
            id: Uuid::new_v4(),
            agent_id: AgentId::new(),
            content: "The deadline is March 15th".into(),
            category: MemoryCategory::Fact,
            scope: MemoryScope::Private,
            metadata: HashMap::new(),
            created_at: Utc::now(),
            replaces: None,
            embedding: vec![0.1, 0.2, 0.3],
            embedding_model: "test-model".into(),
        };
        let json = serde_json::to_string(&record).unwrap();
        let parsed: MemoryRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, record.id);
        assert_eq!(parsed.content, record.content);
        assert_eq!(parsed.category, MemoryCategory::Fact);
    }

    #[test]
    fn memory_result_has_no_embedding() {
        let result = MemoryResult {
            id: Uuid::new_v4(),
            content: "test".into(),
            category: MemoryCategory::Observation,
            metadata: HashMap::new(),
            created_at: Utc::now(),
            relevance_score: 0.95,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(!json.contains("embedding"));
    }
}
