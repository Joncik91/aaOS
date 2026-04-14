use async_trait::async_trait;
use uuid::Uuid;

use aaos_core::AgentId;
use crate::types::{MemoryCategory, MemoryRecord, MemoryResult};

/// Error type for memory operations.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("memory not found: {0}")]
    NotFound(Uuid),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("embedding error: {0}")]
    Embedding(String),

    #[error("content too large: {size} bytes exceeds max {max} bytes")]
    ContentTooLarge { size: usize, max: usize },

    #[error("dimension mismatch: expected {expected}, got {actual}")]
    DimensionMismatch { expected: usize, actual: usize },
}

pub type MemoryStoreResult<T> = std::result::Result<T, MemoryError>;

/// Trait for episodic memory storage.
#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// Store a memory record. Embedding must already be populated.
    /// If episodic_max_records is exceeded, evicts the oldest record(s).
    async fn store(&self, record: MemoryRecord) -> MemoryStoreResult<Uuid>;

    /// Query memories by semantic similarity. Returns top-K results.
    /// Results are stripped of embedding vectors.
    async fn query(
        &self,
        agent_id: &AgentId,
        query_embedding: &[f32],
        limit: usize,
        category: Option<MemoryCategory>,
    ) -> MemoryStoreResult<Vec<MemoryResult>>;

    /// Delete a specific memory.
    async fn delete(&self, agent_id: &AgentId, memory_id: &Uuid) -> MemoryStoreResult<()>;

    /// List memories for an agent with pagination.
    /// Results are stripped of embedding vectors.
    async fn list(
        &self,
        agent_id: &AgentId,
        offset: usize,
        limit: usize,
    ) -> MemoryStoreResult<Vec<MemoryResult>>;

    /// Count memories for an agent.
    async fn count(&self, agent_id: &AgentId) -> MemoryStoreResult<usize>;
}
