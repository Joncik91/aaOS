# Phase C2: Episodic Memory Store

> **Sub-project of Phase C: Agent Memory System**
> Builds on Phase C1 (managed context windows) and Phase B (persistent agents).

## Goal

Per-agent persistent memory that survives across conversations and summarization cycles. Agents explicitly store facts, observations, and decisions via a `memory_store` tool, and retrieve them via a `memory_query` tool. Like a personal notebook the agent can write to and search through.

## Peer Review Notes (Qwen + Copilot)

Reviewed by Qwen CLI and GitHub Copilot CLI. Key feedback incorporated:
- Single LanceDB table with `agent_id` column, not one table per agent (both reviewers)
- `query` trait method now includes `category` filter to match tool interface (Copilot)
- Cap overflow behavior defined: evict oldest records (both reviewers)
- Embedding call happens in tool handler before persisting (Qwen)
- `MemoryResult` strips embedding vectors from results (Copilot)
- Added optional `replaces: Uuid` for update semantics (both reviewers)
- `agent_id` injected by runtime, not part of tool input schema (Copilot)
- Embedding failure during store returns error, not a non-queryable record (Copilot)
- Added `max_content_bytes` config (Copilot)
- `list()` paginated with offset/limit (Copilot)
- Added `MemoryScope` field for C3 forward-compatibility (Copilot C3 review)
- Embedding dimension mismatch detection (Copilot)
- Removed self-contradictory Capability enum section (Copilot)

## What Changes

### New: `aaos-memory` crate

A new workspace member dedicated to the memory subsystem. Keeps memory logic out of `aaos-runtime` (which is already growing).

**Workspace addition:** `crates/aaos-memory`

### New: `MemoryStore` trait

```rust
pub trait MemoryStore: Send + Sync {
    /// Store a memory record for an agent. Embedding must already be populated.
    /// If episodic_max_records is exceeded, evicts the oldest record(s) first.
    async fn store(&self, agent_id: &AgentId, record: MemoryRecord) -> Result<Uuid>;

    /// Query memories by semantic similarity. Returns top-K results.
    /// Results are stripped of embedding vectors.
    async fn query(
        &self,
        agent_id: &AgentId,
        query_embedding: &[f32],
        limit: usize,
        category: Option<MemoryCategory>,
    ) -> Result<Vec<MemoryResult>>;

    /// Delete a specific memory.
    async fn delete(&self, agent_id: &AgentId, memory_id: &Uuid) -> Result<()>;

    /// List memories for an agent with pagination (for debugging/inspection).
    /// Results are stripped of embedding vectors.
    async fn list(
        &self,
        agent_id: &AgentId,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>>;

    /// Count memories for an agent.
    async fn count(&self, agent_id: &AgentId) -> Result<usize>;
}
```

**Placement:** `aaos-memory::store`

**Key design decisions:**
- `query` takes a pre-computed `query_embedding`, not a raw string — the tool handler calls `EmbeddingSource::embed()` on the query before calling the store. This keeps the store independent of the embedding source.
- `category` filter is applied at the DB level (WHERE clause), not post-filter, so `limit` returns the correct number of results.
- `agent_id` scoping is enforced at the store level — agents can only access their own memories.

### New: `MemoryRecord` and `MemoryResult`

```rust
pub struct MemoryRecord {
    pub id: Uuid,
    pub agent_id: AgentId,
    pub content: String,                      // the fact/observation/decision
    pub category: MemoryCategory,
    pub scope: MemoryScope,                   // Private (C2) or Shared (future C3)
    pub metadata: HashMap<String, Value>,     // arbitrary tags
    pub created_at: DateTime<Utc>,
    pub replaces: Option<Uuid>,              // if this record supersedes an older one
    pub embedding: Vec<f32>,                  // required, not optional
    pub embedding_model: String,              // e.g., "text-embedding-3-small"
}

pub enum MemoryCategory {
    Fact,        // "User's name is Jounes"
    Observation, // "The auth module has high cyclomatic complexity"
    Decision,    // "We chose LanceDB over Qdrant for embedded use"
    Preference,  // "User prefers terse responses"
}

/// Forward-compatible with C3. Private is the only supported scope in C2.
pub enum MemoryScope {
    Private,
    // Future C3: Shared { topics: Vec<String> }
}

/// Query result — embedding vectors stripped for efficiency.
pub struct MemoryResult {
    pub id: Uuid,
    pub content: String,
    pub category: MemoryCategory,
    pub metadata: HashMap<String, Value>,
    pub created_at: DateTime<Utc>,
    pub relevance_score: f32,  // cosine similarity, normalized to 0.0-1.0
}
```

**Embedding is required, not optional.** If the embedding source fails during `memory_store`, the tool returns an error to the agent. We don't store non-queryable records — that's a silent failure mode that confuses agents.

**`embedding_model` stored per record.** If the system embedding model changes, records with mismatched dimensions are skipped during query (with a logged warning), not silently corrupted. This enables gradual migration.

**`replaces: Option<Uuid>`** enables update semantics. When set, the referenced record is soft-deleted (or marked superseded) and the new record takes its place. Agents use this when they learn a fact has changed.

### New: `EmbeddingSource` trait

```rust
pub trait EmbeddingSource: Send + Sync {
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;
    fn dimensions(&self) -> usize;
    fn model_name(&self) -> &str;
}
```

**Implementations (v1):**
- `HttpEmbeddingSource` — Uses an external embeddings API (OpenAI `text-embedding-3-small`, Voyage, etc.) via HTTP. Configurable endpoint, API key, model name. Uses `reqwest` (already in workspace).
- `MockEmbeddingSource` — Returns deterministic vectors for testing. Configurable dimensions.

**Future:** `OnnxEmbeddingSource` for local models — deferred to keep C2 scope manageable.

The embedding source is configured per-system, not per-agent. All agents share the same embedding model for consistency.

### New: `LanceDbMemoryStore`

Primary `MemoryStore` implementation backed by LanceDB (embedded, Rust-native, supports metadata filtering).

**Storage:** Single LanceDB database at `{data_dir}/memory/`. One table `memories` shared across all agents.

**Schema:**
- `id: String` (UUID)
- `agent_id: String`
- `content: String`
- `category: String`
- `scope: String` (default "private")
- `metadata: String` (JSON — not filterable at DB layer in v1, noted for future)
- `created_at: i64` (timestamp)
- `replaces: String` (nullable UUID)
- `embedding_model: String`
- `vector: FixedSizeList<Float32>` (embedding)

**Query:** Vector similarity search with `WHERE agent_id = ? AND category = ?` filtering (category optional). Returns top-K by cosine similarity. Skips records where `embedding_model` doesn't match current model (dimension mismatch guard).

**Cap enforcement:** On `store()`, if `count(agent_id) >= episodic_max_records`, delete the oldest record(s) for that agent before inserting. LRU eviction — least surprising default.

### New: `InMemoryMemoryStore`

For testing. Stores `Vec<MemoryRecord>` per agent in a `DashMap`. Query uses brute-force cosine similarity over stored embeddings.

### New: `memory_store` tool

Registered in `ToolRegistry`. Agents use it to explicitly save memories.

```json
{
  "name": "memory_store",
  "description": "Store a fact, observation, decision, or preference for later retrieval. Use 'replaces' to update an existing memory.",
  "input_schema": {
    "type": "object",
    "properties": {
      "content": { "type": "string", "description": "The memory to store (max 4096 bytes)" },
      "category": { "type": "string", "enum": ["fact", "observation", "decision", "preference"] },
      "replaces": { "type": "string", "description": "Optional: UUID of a memory this replaces" }
    },
    "required": ["content", "category"]
  }
}
```

**Tool handler flow:**
1. Validate content length (`<= max_content_bytes`, default 4096).
2. Call `embedding_source.embed(content)` — if this fails, return error to agent.
3. Create `MemoryRecord` with `agent_id` injected from `InvocationContext` (NOT from tool input — agents cannot specify another agent's ID).
4. Call `memory_store.store(agent_id, record)` — handles cap eviction internally.
5. Return `{ "memory_id": "<uuid>", "status": "stored" }`.

**Capability:** `tool: memory_store` in the agent's manifest capabilities list. Goes through the existing `ToolInvoke` capability path — no new `Capability` enum variants needed.

### New: `memory_query` tool

```json
{
  "name": "memory_query",
  "description": "Search your stored memories by meaning. Returns the most relevant memories for the given query.",
  "input_schema": {
    "type": "object",
    "properties": {
      "query": { "type": "string", "description": "What to search for" },
      "limit": { "type": "integer", "default": 5, "description": "Max results (1-20)" },
      "category": { "type": "string", "enum": ["fact", "observation", "decision", "preference"], "description": "Optional: filter by category" }
    },
    "required": ["query"]
  }
}
```

**Tool handler flow:**
1. Call `embedding_source.embed(query)`.
2. Call `memory_store.query(agent_id, query_embedding, limit, category)`.
3. Return results as JSON array (content, category, relevance_score, memory_id, created_at).

**Capability:** `tool: memory_query` — separate from `memory_store` so agents can have read-only memory access.

### New: `memory_delete` tool

```json
{
  "name": "memory_delete",
  "description": "Delete a specific stored memory by ID.",
  "input_schema": {
    "type": "object",
    "properties": {
      "memory_id": { "type": "string", "description": "UUID of the memory to delete" }
    },
    "required": ["memory_id"]
  }
}
```

### Modified: `MemoryConfig`

```rust
pub struct MemoryConfig {
    // From C1:
    pub context_window: String,
    pub max_history_messages: Option<usize>,
    pub summarization_model: Option<String>,
    pub summarization_threshold: Option<f32>,
    pub archive_ttl_days: Option<u32>,
    // New in C2:
    pub episodic_enabled: bool,                // default: false
    pub episodic_max_records: Option<usize>,    // per-agent cap, default: 10_000
    pub episodic_max_content_bytes: Option<usize>, // default: 4096
}
```

### Audit Events

Two new `AuditEventKind` variants:

- `MemoryStored { memory_id: Uuid, category: String, content_hash: String }` — `content_hash` is SHA-256 of the content, computed at audit time (not stored in record).
- `MemoryQueried { query_hash: String, results_count: usize }` — `query_hash` is SHA-256 of the query string.

### Agent Deletion Cleanup

When an agent is stopped and removed from the registry, its memories are NOT automatically deleted (they may be valuable for debugging/forensics). Orphaned memories can be cleaned up via a future admin API or manual deletion. This is explicitly documented as expected behavior.

## What You'll See

1. Spawn a persistent agent with `memory_store` and `memory_query` capabilities.
2. Tell it: "Remember that the project deadline is March 15th."
3. The agent calls `memory_store` with `category: fact, content: "Project deadline is March 15th"`.
4. Many turns later (or in a new session after restart), ask: "When is the project deadline?"
5. The agent calls `memory_query` with `query: "project deadline"`, gets the stored fact, and responds correctly.
6. This works even after context window summarization has compressed the original conversation.

## Testing Strategy

- **Unit tests:** `MemoryRecord` creation, serialization, category parsing, `MemoryScope` default.
- **Unit tests:** `InMemoryMemoryStore` — store, query (with mock embeddings), delete, list (paginated), count, agent isolation (agent A can't see agent B's memories), cap eviction (LRU), `replaces` semantics.
- **Unit tests:** `EmbeddingSource` mock — deterministic vectors, dimension checking, model name.
- **Unit tests:** Embedding dimension mismatch — records with wrong dimensions skipped during query.
- **Integration test:** `memory_store` and `memory_query` tools registered, invoked through `ToolInvocation` with capability checks. Verify `agent_id` is injected from context, not tool input.
- **Integration test:** Full persistent agent loop — agent stores memory in turn 1, context gets summarized (C1), agent queries memory in turn N, gets correct result.
- **Live API test:** Real Haiku agent with memory tools. Store facts, have a long conversation that triggers summarization, verify memory recall works across the summarization boundary.

## Dependencies

- **New crate dependency:** `lancedb` (workspace-level). Exact version TBD based on latest stable Rust API.
- **New crate dependency:** `sha2` (for content/query hashing in audit events).
- **Existing:** `reqwest` (for embedding API calls), `uuid`, `chrono`, `serde`, `dashmap`.
- **Requires:** Phase C1 (managed context windows) for the summarization boundary test, but the memory store itself is independently functional.

## Out of Scope

- Automatic fact extraction during summarization (future: C2 v2)
- Automatic RAG retrieval (runtime injects memories without agent asking)
- Cross-agent memory sharing (C3) — `MemoryScope::Shared` variant reserved but not implemented
- Local embedding models (ONNX)
- Memory importance ranking or decay
- Deduplication by content similarity (agents manage this via `replaces`)
