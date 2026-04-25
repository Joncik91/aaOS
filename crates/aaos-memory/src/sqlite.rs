//! SQLite-backed persistent memory store.
//!
//! Uses rusqlite for storage and Rust-side cosine similarity for search.
//! Embeddings stored as BLOBs (f32 → little-endian bytes). At the scales
//! aaOS operates (hundreds to low thousands of records per agent), brute-force
//! cosine similarity is fast enough — no vector index needed.

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::store::{MemoryError, MemoryStore, MemoryStoreResult};
use crate::types::{MemoryCategory, MemoryRecord, MemoryResult, MemoryScope};
use aaos_core::AgentId;

/// SQLite-backed memory store. Thread-safe via Mutex around the connection.
pub struct SqliteMemoryStore {
    conn: Mutex<rusqlite::Connection>,
}

impl SqliteMemoryStore {
    /// Open or create a SQLite database at the given path.
    pub fn open(path: &Path) -> MemoryStoreResult<Self> {
        let conn = rusqlite::Connection::open(path)
            .map_err(|e| MemoryError::Storage(format!("failed to open SQLite: {e}")))?;

        // Enable WAL mode for concurrent reads
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .map_err(|e| MemoryError::Storage(format!("failed to set pragmas: {e}")))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memories (
                id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                content TEXT NOT NULL,
                category TEXT NOT NULL,
                scope TEXT NOT NULL DEFAULT 'private',
                metadata TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL,
                replaces TEXT,
                embedding BLOB NOT NULL,
                embedding_model TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_memories_agent ON memories(agent_id);
            CREATE INDEX IF NOT EXISTS idx_memories_agent_cat ON memories(agent_id, category);",
        )
        .map_err(|e| MemoryError::Storage(format!("failed to create table: {e}")))?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Open an in-memory SQLite database (for tests).
    #[cfg(test)]
    pub fn in_memory() -> MemoryStoreResult<Self> {
        let conn = rusqlite::Connection::open_in_memory()
            .map_err(|e| MemoryError::Storage(format!("failed to open in-memory SQLite: {e}")))?;
        conn.execute_batch(
            "CREATE TABLE memories (
                id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                content TEXT NOT NULL,
                category TEXT NOT NULL,
                scope TEXT NOT NULL DEFAULT 'private',
                metadata TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL,
                replaces TEXT,
                embedding BLOB NOT NULL,
                embedding_model TEXT NOT NULL
            );
            CREATE INDEX idx_memories_agent ON memories(agent_id);
            CREATE INDEX idx_memories_agent_cat ON memories(agent_id, category);",
        )
        .map_err(|e| MemoryError::Storage(format!("failed to create table: {e}")))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn blob_to_embedding(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 {
        0.0
    } else {
        // Normalize to [0, 1] to match InMemoryMemoryStore behavior
        (dot / denom + 1.0) / 2.0
    }
}

fn category_to_str(cat: &MemoryCategory) -> &'static str {
    match cat {
        MemoryCategory::Fact => "fact",
        MemoryCategory::Observation => "observation",
        MemoryCategory::Decision => "decision",
        MemoryCategory::Preference => "preference",
    }
}

fn str_to_category(s: &str) -> MemoryCategory {
    match s {
        "fact" => MemoryCategory::Fact,
        "observation" => MemoryCategory::Observation,
        "decision" => MemoryCategory::Decision,
        "preference" => MemoryCategory::Preference,
        _ => MemoryCategory::Fact,
    }
}

fn scope_to_str(scope: &MemoryScope) -> &'static str {
    match scope {
        MemoryScope::Private => "private",
    }
}

#[async_trait]
impl MemoryStore for SqliteMemoryStore {
    async fn store(&self, record: MemoryRecord) -> MemoryStoreResult<Uuid> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|e| MemoryError::Storage(e.to_string()))?;

        let blob = embedding_to_blob(&record.embedding);
        let metadata_json = serde_json::to_string(&record.metadata)
            .map_err(|e| MemoryError::Storage(format!("failed to serialize metadata: {e}")))?;

        // Wrap DELETE + INSERT in an explicit transaction so that if the
        // INSERT fails the old record is not lost.  Without this, the two
        // statements ran as separate auto-commits: a failed INSERT left the
        // store permanently empty for the replaced record's key.
        let tx = conn
            .transaction()
            .map_err(|e| MemoryError::Storage(format!("failed to begin transaction: {e}")))?;

        // Atomic replace: delete old record if this one replaces it
        if let Some(replaces_id) = &record.replaces {
            tx.execute(
                "DELETE FROM memories WHERE id = ?1 AND agent_id = ?2",
                rusqlite::params![replaces_id.to_string(), record.agent_id.to_string()],
            )
            .map_err(|e| MemoryError::Storage(format!("failed to delete replaced: {e}")))?;
        }

        tx.execute(
            "INSERT INTO memories (id, agent_id, content, category, scope, metadata, created_at, replaces, embedding, embedding_model)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                record.id.to_string(),
                record.agent_id.to_string(),
                record.content,
                category_to_str(&record.category),
                scope_to_str(&record.scope),
                metadata_json,
                record.created_at.to_rfc3339(),
                record.replaces.map(|u| u.to_string()),
                blob,
                record.embedding_model,
            ],
        )
        .map_err(|e| MemoryError::Storage(format!("failed to insert: {e}")))?;

        tx.commit()
            .map_err(|e| MemoryError::Storage(format!("failed to commit transaction: {e}")))?;

        Ok(record.id)
    }

    async fn query(
        &self,
        agent_id: &AgentId,
        query_embedding: &[f32],
        limit: usize,
        category: Option<MemoryCategory>,
    ) -> MemoryStoreResult<Vec<MemoryResult>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| MemoryError::Storage(e.to_string()))?;

        let (sql, params): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = match &category {
            Some(cat) => (
                "SELECT id, content, category, metadata, created_at, embedding FROM memories WHERE agent_id = ?1 AND category = ?2",
                vec![Box::new(agent_id.to_string()) as Box<dyn rusqlite::types::ToSql>, Box::new(category_to_str(cat).to_string())],
            ),
            None => (
                "SELECT id, content, category, metadata, created_at, embedding FROM memories WHERE agent_id = ?1",
                vec![Box::new(agent_id.to_string()) as Box<dyn rusqlite::types::ToSql>],
            ),
        };

        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| MemoryError::Storage(format!("failed to prepare query: {e}")))?;

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let rows: Vec<(String, String, String, String, String, Vec<u8>)> = stmt
            .query_map(param_refs.as_slice(), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                ))
            })
            .map_err(|e| MemoryError::Storage(format!("query failed: {e}")))?
            .filter_map(|r| r.ok())
            .collect();

        // Compute cosine similarity in Rust and sort
        let mut scored: Vec<(MemoryResult, f32)> = rows
            .into_iter()
            .filter_map(|(id_str, content, cat_str, meta_str, created_str, blob)| {
                let embedding = blob_to_embedding(&blob);
                let sim = cosine_similarity(query_embedding, &embedding);
                let metadata = serde_json::from_str(&meta_str).unwrap_or_default();
                let created_at = DateTime::parse_from_rfc3339(&created_str)
                    .ok()?
                    .with_timezone(&Utc);
                Some((
                    MemoryResult {
                        id: Uuid::parse_str(&id_str).ok()?,
                        content,
                        category: str_to_category(&cat_str),
                        metadata,
                        created_at,
                        relevance_score: sim,
                    },
                    sim,
                ))
            })
            .collect();

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);

        Ok(scored.into_iter().map(|(r, _)| r).collect())
    }

    async fn delete(&self, agent_id: &AgentId, memory_id: &Uuid) -> MemoryStoreResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| MemoryError::Storage(e.to_string()))?;
        let deleted = conn
            .execute(
                "DELETE FROM memories WHERE id = ?1 AND agent_id = ?2",
                rusqlite::params![memory_id.to_string(), agent_id.to_string()],
            )
            .map_err(|e| MemoryError::Storage(format!("delete failed: {e}")))?;
        if deleted == 0 {
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
        let conn = self
            .conn
            .lock()
            .map_err(|e| MemoryError::Storage(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, content, category, metadata, created_at FROM memories
                 WHERE agent_id = ?1 ORDER BY created_at ASC LIMIT ?2 OFFSET ?3",
            )
            .map_err(|e| MemoryError::Storage(format!("failed to prepare list: {e}")))?;

        let results = stmt
            .query_map(
                rusqlite::params![agent_id.to_string(), limit as i64, offset as i64],
                |row| {
                    let id_str: String = row.get(0)?;
                    let content: String = row.get(1)?;
                    let cat_str: String = row.get(2)?;
                    let meta_str: String = row.get(3)?;
                    let created_str: String = row.get(4)?;
                    Ok((id_str, content, cat_str, meta_str, created_str))
                },
            )
            .map_err(|e| MemoryError::Storage(format!("list query failed: {e}")))?
            .filter_map(|r| {
                let (id_str, content, cat_str, meta_str, created_str) = r.ok()?;
                Some(MemoryResult {
                    id: Uuid::parse_str(&id_str).ok()?,
                    content,
                    category: str_to_category(&cat_str),
                    metadata: serde_json::from_str(&meta_str).unwrap_or_default(),
                    created_at: DateTime::parse_from_rfc3339(&created_str)
                        .ok()?
                        .with_timezone(&Utc),
                    relevance_score: 0.0,
                })
            })
            .collect();

        Ok(results)
    }

    async fn count(&self, agent_id: &AgentId) -> MemoryStoreResult<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| MemoryError::Storage(e.to_string()))?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE agent_id = ?1",
                rusqlite::params![agent_id.to_string()],
                |row| row.get(0),
            )
            .map_err(|e| MemoryError::Storage(format!("count failed: {e}")))?;
        Ok(count as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn test_record(agent_id: AgentId, content: &str, embedding: Vec<f32>) -> MemoryRecord {
        MemoryRecord {
            id: Uuid::new_v4(),
            agent_id,
            content: content.into(),
            category: MemoryCategory::Fact,
            scope: MemoryScope::Private,
            metadata: HashMap::new(),
            created_at: Utc::now(),
            replaces: None,
            embedding,
            embedding_model: "test".into(),
        }
    }

    #[tokio::test]
    async fn store_and_count() {
        let store = SqliteMemoryStore::in_memory().unwrap();
        let agent = AgentId::new();
        let r = test_record(agent, "hello", vec![1.0, 0.0, 0.0]);
        store.store(r).await.unwrap();
        assert_eq!(store.count(&agent).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn query_by_similarity() {
        let store = SqliteMemoryStore::in_memory().unwrap();
        let agent = AgentId::new();

        store
            .store(test_record(agent, "cats are great", vec![1.0, 0.0, 0.0]))
            .await
            .unwrap();
        store
            .store(test_record(agent, "dogs are nice", vec![0.0, 1.0, 0.0]))
            .await
            .unwrap();
        store
            .store(test_record(agent, "fish swim", vec![0.0, 0.0, 1.0]))
            .await
            .unwrap();

        let results = store
            .query(&agent, &[0.9, 0.1, 0.0], 2, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].content, "cats are great");
    }

    #[tokio::test]
    async fn agent_isolation() {
        let store = SqliteMemoryStore::in_memory().unwrap();
        let a1 = AgentId::new();
        let a2 = AgentId::new();

        store
            .store(test_record(a1, "agent 1 secret", vec![1.0, 0.0]))
            .await
            .unwrap();
        store
            .store(test_record(a2, "agent 2 data", vec![1.0, 0.0]))
            .await
            .unwrap();

        let r1 = store.query(&a1, &[1.0, 0.0], 10, None).await.unwrap();
        assert_eq!(r1.len(), 1);
        assert_eq!(r1[0].content, "agent 1 secret");
    }

    #[tokio::test]
    async fn delete_memory() {
        let store = SqliteMemoryStore::in_memory().unwrap();
        let agent = AgentId::new();
        let r = test_record(agent, "temporary", vec![1.0]);
        let id = store.store(r).await.unwrap();
        assert_eq!(store.count(&agent).await.unwrap(), 1);
        store.delete(&agent, &id).await.unwrap();
        assert_eq!(store.count(&agent).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn replaces_is_atomic() {
        let store = SqliteMemoryStore::in_memory().unwrap();
        let agent = AgentId::new();

        let old = test_record(agent, "old fact", vec![1.0, 0.0]);
        let old_id = store.store(old).await.unwrap();

        let mut new = test_record(agent, "updated fact", vec![1.0, 0.0]);
        new.replaces = Some(old_id);
        store.store(new).await.unwrap();

        assert_eq!(store.count(&agent).await.unwrap(), 1);
        let results = store.query(&agent, &[1.0, 0.0], 10, None).await.unwrap();
        assert_eq!(results[0].content, "updated fact");
    }

    #[tokio::test]
    async fn list_ordered_by_created_at() {
        let store = SqliteMemoryStore::in_memory().unwrap();
        let agent = AgentId::new();

        store
            .store(test_record(agent, "first", vec![1.0]))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        store
            .store(test_record(agent, "second", vec![1.0]))
            .await
            .unwrap();

        let results = store.list(&agent, 0, 10).await.unwrap();
        assert_eq!(results[0].content, "first");
        assert_eq!(results[1].content, "second");
    }

    #[tokio::test]
    async fn query_by_category() {
        let store = SqliteMemoryStore::in_memory().unwrap();
        let agent = AgentId::new();

        let mut fact = test_record(agent, "a fact", vec![1.0, 0.0]);
        fact.category = MemoryCategory::Fact;
        store.store(fact).await.unwrap();

        let mut obs = test_record(agent, "an observation", vec![0.0, 1.0]);
        obs.category = MemoryCategory::Observation;
        store.store(obs).await.unwrap();

        let facts = store
            .query(&agent, &[0.5, 0.5], 10, Some(MemoryCategory::Fact))
            .await
            .unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].content, "a fact");
    }
}
