pub mod types;
pub mod store;
pub mod embedding;
pub mod in_memory;

pub use types::{MemoryRecord, MemoryCategory, MemoryScope, MemoryResult};
pub use store::{MemoryStore, MemoryError, MemoryResult2};
pub use embedding::{EmbeddingSource, MockEmbeddingSource, OllamaEmbeddingSource};
pub use in_memory::InMemoryMemoryStore;
