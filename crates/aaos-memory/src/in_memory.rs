use async_trait::async_trait;
use dashmap::DashMap;
use uuid::Uuid;

use crate::store::{MemoryError, MemoryStore, MemoryStoreResult};
use crate::types::{MemoryCategory, MemoryRecord, MemoryResult};
use aaos_core::AgentId;

/// In-memory implementation of `MemoryStore` backed by `DashMap`.
///
/// Provides cosine-similarity search, per-agent isolation, LRU cap eviction,
/// and `replaces` (update-in-place) semantics.
pub struct InMemoryMemoryStore {
    records: DashMap<AgentId, Vec<MemoryRecord>>,
    max_records: usize,
    expected_dims: usize,
    expected_model: String,
}

impl InMemoryMemoryStore {
    pub fn new(max_records: usize, expected_dims: usize, expected_model: &str) -> Self {
        Self {
            records: DashMap::new(),
            max_records,
            expected_dims,
            expected_model: expected_model.to_owned(),
        }
    }
}

/// Cosine similarity between two vectors, normalized to [0.0, 1.0].
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let mag_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let mag_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if mag_a == 0.0 || mag_b == 0.0 {
        return 0.0;
    }
    let sim = dot / (mag_a * mag_b);
    // Normalize from [-1, 1] to [0, 1]
    (sim + 1.0) / 2.0
}

fn record_to_result(record: &MemoryRecord, relevance_score: f32) -> MemoryResult {
    MemoryResult {
        id: record.id,
        content: record.content.clone(),
        category: record.category.clone(),
        metadata: record.metadata.clone(),
        created_at: record.created_at,
        relevance_score,
    }
}

#[async_trait]
impl MemoryStore for InMemoryMemoryStore {
    async fn store(&self, record: MemoryRecord) -> MemoryStoreResult<Uuid> {
        let agent_id = record.agent_id;
        let record_id = record.id;

        let mut entries = self.records.entry(agent_id).or_default();

        // Handle replaces semantics: remove the old record if specified
        if let Some(old_id) = record.replaces {
            entries.retain(|r| r.id != old_id);
        }

        // Cap eviction: remove oldest by created_at if at capacity
        while entries.len() >= self.max_records {
            if let Some(oldest_idx) = entries
                .iter()
                .enumerate()
                .min_by_key(|(_, r)| r.created_at)
                .map(|(i, _)| i)
            {
                entries.remove(oldest_idx);
            } else {
                break;
            }
        }

        entries.push(record);
        Ok(record_id)
    }

    async fn query(
        &self,
        agent_id: &AgentId,
        query_embedding: &[f32],
        limit: usize,
        category: Option<MemoryCategory>,
    ) -> MemoryStoreResult<Vec<MemoryResult>> {
        let entries = match self.records.get(agent_id) {
            Some(e) => e,
            None => return Ok(Vec::new()),
        };

        let mut scored: Vec<MemoryResult> = entries
            .iter()
            .filter(|r| {
                // Dimension mismatch check
                if r.embedding.len() != self.expected_dims {
                    tracing::warn!(
                        record_id = %r.id,
                        expected = self.expected_dims,
                        actual = r.embedding.len(),
                        "skipping record: embedding dimension mismatch"
                    );
                    return false;
                }
                // Model mismatch check
                if r.embedding_model != self.expected_model {
                    tracing::warn!(
                        record_id = %r.id,
                        expected = %self.expected_model,
                        actual = %r.embedding_model,
                        "skipping record: embedding model mismatch"
                    );
                    return false;
                }
                // Category filter
                if let Some(ref cat) = category {
                    if &r.category != cat {
                        return false;
                    }
                }
                true
            })
            .map(|r| {
                let score = cosine_similarity(query_embedding, &r.embedding);
                record_to_result(r, score)
            })
            .collect();

        // Sort descending by relevance
        scored.sort_by(|a, b| b.relevance_score.partial_cmp(&a.relevance_score).unwrap());
        scored.truncate(limit);
        Ok(scored)
    }

    async fn delete(&self, agent_id: &AgentId, memory_id: &Uuid) -> MemoryStoreResult<()> {
        let mut entries = self
            .records
            .get_mut(agent_id)
            .ok_or(MemoryError::NotFound(*memory_id))?;

        let before = entries.len();
        entries.retain(|r| r.id != *memory_id);
        if entries.len() == before {
            return Err(MemoryError::NotFound(*memory_id));
        }
        Ok(())
    }

    async fn list(
        &self,
        agent_id: &AgentId,
        offset: usize,
        limit: usize,
    ) -> MemoryStoreResult<Vec<MemoryResult>> {
        let entries = match self.records.get(agent_id) {
            Some(e) => e,
            None => return Ok(Vec::new()),
        };

        let results: Vec<MemoryResult> = entries
            .iter()
            .skip(offset)
            .take(limit)
            .map(|r| record_to_result(r, 0.0))
            .collect();

        Ok(results)
    }

    async fn count(&self, agent_id: &AgentId) -> MemoryStoreResult<usize> {
        Ok(self.records.get(agent_id).map_or(0, |e| e.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::HashMap;

    fn make_record(
        agent_id: AgentId,
        content: &str,
        category: MemoryCategory,
        embedding: Vec<f32>,
    ) -> MemoryRecord {
        MemoryRecord {
            id: Uuid::new_v4(),
            agent_id,
            content: content.to_owned(),
            category,
            scope: crate::types::MemoryScope::Private,
            metadata: HashMap::new(),
            created_at: Utc::now(),
            replaces: None,
            embedding,
            embedding_model: "test-model".into(),
        }
    }

    fn make_store() -> InMemoryMemoryStore {
        InMemoryMemoryStore::new(100, 3, "test-model")
    }

    #[tokio::test]
    async fn store_and_query_basic() {
        let store = make_store();
        let agent = AgentId::new();
        let emb = vec![1.0, 0.0, 0.0];
        let rec = make_record(agent, "hello world", MemoryCategory::Fact, emb.clone());
        store.store(rec).await.unwrap();

        let results = store.query(&agent, &emb, 10, None).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "hello world");
        // Perfect self-similarity should be 1.0
        assert!((results[0].relevance_score - 1.0).abs() < 1e-5);
    }

    #[tokio::test]
    async fn agent_isolation() {
        let store = make_store();
        let agent_a = AgentId::new();
        let agent_b = AgentId::new();
        let emb = vec![1.0, 0.0, 0.0];

        let rec = make_record(agent_a, "secret", MemoryCategory::Fact, emb.clone());
        store.store(rec).await.unwrap();

        // Agent B should see nothing
        let results = store.query(&agent_b, &emb, 10, None).await.unwrap();
        assert!(results.is_empty());

        // Agent A should see the record
        let results = store.query(&agent_a, &emb, 10, None).await.unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn cap_eviction() {
        let store = InMemoryMemoryStore::new(2, 3, "test-model");
        let agent = AgentId::new();
        let emb = vec![1.0, 0.0, 0.0];

        // Store 3 records with increasing timestamps
        for i in 0..3 {
            let mut rec = make_record(
                agent,
                &format!("record-{i}"),
                MemoryCategory::Fact,
                emb.clone(),
            );
            // Ensure distinct timestamps
            rec.created_at = Utc::now() + chrono::Duration::milliseconds(i as i64 * 10);
            store.store(rec).await.unwrap();
        }

        // Should only have 2 records; the oldest (record-0) evicted
        assert_eq!(store.count(&agent).await.unwrap(), 2);
        let results = store.list(&agent, 0, 10).await.unwrap();
        let contents: Vec<&str> = results.iter().map(|r| r.content.as_str()).collect();
        assert!(!contents.contains(&"record-0"));
        assert!(contents.contains(&"record-1"));
        assert!(contents.contains(&"record-2"));
    }

    #[tokio::test]
    async fn replaces_semantics() {
        let store = make_store();
        let agent = AgentId::new();
        let emb = vec![1.0, 0.0, 0.0];

        let original = make_record(agent, "version 1", MemoryCategory::Fact, emb.clone());
        let original_id = original.id;
        store.store(original).await.unwrap();

        let mut replacement = make_record(agent, "version 2", MemoryCategory::Fact, emb.clone());
        replacement.replaces = Some(original_id);
        store.store(replacement).await.unwrap();

        // Only version 2 should remain
        assert_eq!(store.count(&agent).await.unwrap(), 1);
        let results = store.list(&agent, 0, 10).await.unwrap();
        assert_eq!(results[0].content, "version 2");
    }

    #[tokio::test]
    async fn delete_existing() {
        let store = make_store();
        let agent = AgentId::new();
        let emb = vec![1.0, 0.0, 0.0];

        let rec = make_record(agent, "to delete", MemoryCategory::Fact, emb);
        let id = rec.id;
        store.store(rec).await.unwrap();

        store.delete(&agent, &id).await.unwrap();
        assert_eq!(store.count(&agent).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn delete_nonexistent() {
        let store = make_store();
        let agent = AgentId::new();
        let fake_id = Uuid::new_v4();

        let err = store.delete(&agent, &fake_id).await.unwrap_err();
        assert!(matches!(err, MemoryError::NotFound(_)));
    }

    #[tokio::test]
    async fn list_pagination() {
        let store = make_store();
        let agent = AgentId::new();
        let emb = vec![1.0, 0.0, 0.0];

        for i in 0..5 {
            let mut rec = make_record(
                agent,
                &format!("item-{i}"),
                MemoryCategory::Fact,
                emb.clone(),
            );
            rec.created_at = Utc::now() + chrono::Duration::milliseconds(i as i64 * 10);
            store.store(rec).await.unwrap();
        }

        let page = store.list(&agent, 1, 2).await.unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].content, "item-1");
        assert_eq!(page[1].content, "item-2");
    }

    #[tokio::test]
    async fn count_per_agent() {
        let store = make_store();
        let agent_a = AgentId::new();
        let agent_b = AgentId::new();
        let emb = vec![1.0, 0.0, 0.0];

        for _ in 0..3 {
            store
                .store(make_record(agent_a, "a", MemoryCategory::Fact, emb.clone()))
                .await
                .unwrap();
        }
        store
            .store(make_record(agent_b, "b", MemoryCategory::Fact, emb.clone()))
            .await
            .unwrap();

        assert_eq!(store.count(&agent_a).await.unwrap(), 3);
        assert_eq!(store.count(&agent_b).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn dimension_mismatch_skipped() {
        let store = InMemoryMemoryStore::new(100, 3, "test-model");
        let agent = AgentId::new();

        // Store a record with wrong dimensions (4 instead of 3)
        let wrong = make_record(
            agent,
            "wrong dims",
            MemoryCategory::Fact,
            vec![1.0, 0.0, 0.0, 0.0],
        );
        store.store(wrong).await.unwrap();

        // Store a valid record
        let good = make_record(agent, "good", MemoryCategory::Fact, vec![1.0, 0.0, 0.0]);
        store.store(good).await.unwrap();

        let results = store
            .query(&agent, &[1.0, 0.0, 0.0], 10, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "good");
    }

    #[tokio::test]
    async fn query_category_filter() {
        let store = make_store();
        let agent = AgentId::new();
        let emb = vec![1.0, 0.0, 0.0];

        store
            .store(make_record(
                agent,
                "a fact",
                MemoryCategory::Fact,
                emb.clone(),
            ))
            .await
            .unwrap();
        store
            .store(make_record(
                agent,
                "an observation",
                MemoryCategory::Observation,
                emb.clone(),
            ))
            .await
            .unwrap();

        let results = store
            .query(&agent, &emb, 10, Some(MemoryCategory::Fact))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "a fact");
    }

    #[tokio::test]
    async fn results_have_no_embedding() {
        let store = make_store();
        let agent = AgentId::new();
        let emb = vec![1.0, 0.0, 0.0];

        store
            .store(make_record(
                agent,
                "test",
                MemoryCategory::Fact,
                emb.clone(),
            ))
            .await
            .unwrap();

        let results = store.query(&agent, &emb, 10, None).await.unwrap();
        // MemoryResult struct has no embedding field — this is a compile-time guarantee.
        // We verify by serializing and checking the JSON has no embedding key.
        let json = serde_json::to_string(&results[0]).unwrap();
        assert!(!json.contains("embedding"));
    }
}
