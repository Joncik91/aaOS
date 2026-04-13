pub mod types;
pub mod store;
pub mod embedding;
pub mod in_memory;

pub use types::{MemoryRecord, MemoryCategory, MemoryScope, MemoryResult};
pub use store::{MemoryStore, MemoryError, MemoryResult2};
// EmbeddingSource and InMemoryMemoryStore exported after Tasks 4-5
