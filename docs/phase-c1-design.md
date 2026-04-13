# Phase C1: Managed Context Windows

> **Sub-project of Phase C: Agent Memory System**
> Builds on Phase B (persistent agents, session store, conversation persistence).

## Goal

The runtime transparently manages what's in an agent's context window. When the conversation grows too long, the runtime summarizes older messages using the LLM and archives the originals. The agent sees a coherent conversation; the runtime handles compression transparently. Like virtual memory for attention.

## Peer Review Notes (Qwen + Copilot)

Reviewed by Qwen CLI and GitHub Copilot CLI. Key feedback incorporated:
- Replaced `TokenBudget` accumulator with per-call context size estimation (both reviewers flagged accumulator drift)
- Summary folded into system prompt prefix, not a User message (avoids turn alternation breakage)
- Archive-first-then-mutate ordering for atomicity (Copilot)
- Tool call/result pairs treated as atomic units during summarization selection (both)
- `turn_range` replaced with message indices (no turn counter exists)
- Summarization threshold made configurable (Qwen)
- TTL pruning moved to explicit method, not side effect of load (Copilot)
- Phase B compaction migration path noted (Copilot)

## What Changes

### New: `TokenBudget` type

Parses `MemoryConfig.context_window` (e.g., "128k") into a usable number (`131_072u32`). Reconciles with the model's actual context limit via `min(manifest.context_window, model.max_context)`.

**Placement:** `aaos-core::manifest` (alongside `MemoryConfig`).

**Interface:**
```rust
pub struct TokenBudget {
    pub max_tokens: u32,
}

impl TokenBudget {
    /// Parse "128k" → 131_072, "200k" → 204_800, etc.
    /// Caps at min(parsed, model_max).
    pub fn from_config(config_str: &str, model_max: u32) -> Result<Self>;

    /// Estimate tokens in a message list.
    /// Uses chars/4 heuristic (conservative). Not perfect, but avoids
    /// needing a tokenizer dependency.
    pub fn estimate_tokens(messages: &[Message]) -> u32;

    /// Should we summarize before sending these messages?
    /// True when estimated tokens > threshold * max_tokens.
    pub fn should_summarize(&self, messages: &[Message], threshold: f32) -> bool;
}
```

**Key design decision:** No accumulated `used_tokens` counter. Instead, estimate context size from the actual message list before each call. This avoids drift after summarization and is simpler to reason about. The estimate uses `chars / 4` as a conservative heuristic. The LLM response's `usage.input_tokens` can be logged for observability but doesn't drive the trigger.

**Precedence with `max_history_messages`:** `max_history_messages` is the hard cap (message count). `TokenBudget` is the soft cap (estimated token count). The soft cap fires first (at configurable threshold, default 0.7). If the soft cap's summarization fails, `max_history_messages` is the safety valve that truncates. Both limits are checked; they don't conflict.

### New: `Message::Summary` variant

Add to the existing `Message` enum in `aaos-llm::types`:

```rust
pub enum Message {
    User { content: String },
    Assistant { content: Vec<ContentBlock> },
    ToolResult { tool_use_id: String, content: Value, is_error: bool },
    Summary { content: String, messages_summarized: u32, source_range: (usize, usize) },
}
```

**LLM serialization:** The `Summary` variant is **not** sent as a User message (that would break turn alternation). Instead, when `ContextManager` prepares context, summaries are folded into the **system prompt prefix**: the agent's system prompt becomes `"{summary_text}\n\n{original_system_prompt}"`. This preserves the User/Assistant alternation required by the Anthropic API.

**Persistence serialization:** Normal serde round-trip with `#[serde(tag = "role")]` — `Summary` persists as `{"role": "summary", ...}` in JSONL. The dual path (system prompt for LLM, tagged JSON for storage) is handled in `ContextManager::prepare_context()`.

`source_range: (usize, usize)` is message indices into the original history (not turn numbers — no turn counter exists). Used for archive filename generation and audit events.

### New: `ContextManager`

Sits between the persistent loop and the executor. Before each LLM call, decides whether to summarize.

**Placement:** `aaos-runtime::context` (new module).

**Interface:**
```rust
pub struct ContextManager {
    llm_client: Arc<dyn LlmClient>,
    budget: TokenBudget,
    summarization_model: String,     // can differ from agent's model (cheaper)
    summarization_threshold: f32,    // configurable, default 0.7
}

impl ContextManager {
    pub fn new(
        llm_client: Arc<dyn LlmClient>,
        budget: TokenBudget,
        model: String,
        threshold: f32,
    ) -> Self;

    /// Prepare context for the next LLM call.
    /// Does NOT mutate history — returns the prepared messages and
    /// an optional SummarizationResult. The CALLER is responsible for:
    /// 1. Archiving the segment (if summarized)
    /// 2. Updating the in-memory history (replacing archived messages with summary)
    /// This ordering ensures archive-first atomicity.
    pub async fn prepare_context(
        &self,
        history: &[Message],
        system_prompt: &str,
    ) -> Result<PreparedContext>;
}

pub struct PreparedContext {
    /// Messages to send to the LLM (summaries folded into system_prompt).
    pub messages: Vec<Message>,
    /// Modified system prompt (with summary prefix if applicable).
    pub system_prompt: String,
    /// If summarization occurred, contains the archived segment.
    pub summarization: Option<SummarizationResult>,
}

pub struct SummarizationResult {
    pub archived_messages: Vec<Message>,  // raw messages that were replaced
    pub summary: Message,                  // the Summary message (for storage)
    pub source_range: (usize, usize),     // message indices
    pub tokens_saved_estimate: u32,
}
```

**Summarization logic:**
1. Estimate tokens in `history` via `TokenBudget::estimate_tokens()`.
2. If estimated tokens > `threshold * max_tokens`, select oldest N messages for summarization.
3. **Message selection respects atomic pairs:** ToolUse (Assistant) + ToolResult messages are never split. If a ToolUse is in the selection range, its corresponding ToolResult must be included (and vice versa).
4. **Existing Summary messages at the front are included in the selection** — they get re-summarized into a more compact form (recursive summarization).
5. **Guard the summarization request itself:** If the selected messages exceed the summarization model's context limit, truncate the selection to fit. This prevents the summarization call from failing due to its own input being too large.
6. Call the LLM with: `system: "Compress this conversation into a dense factual summary. Preserve all names, numbers, decisions, stated preferences, and tool results. Be concise." messages: [selected messages formatted as text]`
7. **Do not mutate history.** Return `SummarizationResult` for the caller to apply.

**Fallback:** If the summarization LLM call fails (timeout, API error, etc.), fall back to hard truncation: drop the oldest messages up to `max_history_messages`. Log `ContextSummarizationFailed` audit event. The agent keeps running.

### Expanded: `SessionStore` trait

Add two methods:

```rust
pub trait SessionStore: Send + Sync {
    fn load(&self, agent_id: &AgentId) -> Result<Vec<Message>>;
    fn append(&self, agent_id: &AgentId, messages: &[Message]) -> Result<()>;
    fn clear(&self, agent_id: &AgentId) -> Result<()>;

    // New in C1:
    fn archive_segment(&self, agent_id: &AgentId, segment: &ArchiveSegment) -> Result<()>;
    fn load_archives(&self, agent_id: &AgentId) -> Result<Vec<ArchiveSegment>>;
    fn prune_archives(&self, agent_id: &AgentId, max_age: Duration) -> Result<usize>;
}

pub struct ArchiveSegment {
    pub source_range: (usize, usize),  // message indices from original history
    pub messages: Vec<Message>,
    pub archived_at: DateTime<Utc>,
}
```

**JsonlSessionStore changes:**
- `archive_segment()` writes to `{data_dir}/{agent_id}.archive.{uuid}.jsonl` (UUID avoids filename collisions on retry/race).
- `load_archives()` reads all archive files for the agent, sorted by `archived_at`. Does NOT prune — read-only.
- `prune_archives()` deletes archive files older than `max_age`. Called explicitly on agent startup, not as a side effect.

**InMemorySessionStore changes:** Stores archives in a second `DashMap<String, Vec<ArchiveSegment>>`.

**Phase B migration:** Existing JSONL files from Phase B (with the 10-turn compaction format) load normally — they're just message lists. The compaction logic is removed but existing files remain compatible.

### Expanded: `LlmClient` trait

Add one method:

```rust
pub trait LlmClient: Send + Sync {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse>;
    // New in C1:
    fn max_context_tokens(&self, model: &str) -> u32;
}
```

`AnthropicClient` implementation returns known limits from a configurable lookup (not compiled-in). Default: `claude-haiku-4-5-20251001` → 200_000, unknown → 128_000. The lookup can be overridden via a config file or environment variable to avoid staleness.

### Modified: `persistent_agent_loop()`

Current flow:
```
receive message → run_with_history(history) → append to session store → compact every 10 turns
```

New flow:
```
receive message →
    context_manager.prepare_context(history, system_prompt) →
    if summarized:
        1. archive_segment(archived_messages)  ← archive FIRST
        2. replace archived messages with Summary in history  ← then mutate
    executor.run_with_history(prepared_messages, prepared_system_prompt) →
    append transcript_delta to session store
```

The every-10-turns JSONL compaction is removed — `ContextManager` replaces it.

### Modified: `MemoryConfig`

```rust
pub struct MemoryConfig {
    #[serde(default = "default_context_window")]
    pub context_window: String,                // "128k", "200k", etc.
    #[serde(default)]
    pub max_history_messages: Option<usize>,    // hard cap, safety valve
    #[serde(default)]
    pub summarization_model: Option<String>,    // cheaper model for summaries
    #[serde(default = "default_summarization_threshold")]
    pub summarization_threshold: Option<f32>,   // default: 0.7
    #[serde(default = "default_archive_ttl_days")]
    pub archive_ttl_days: Option<u32>,          // default: 30
    #[serde(default)]
    pub episodic_store: Option<String>,         // reserved for C2
}
```

## What You'll See

1. Spawn a persistent agent.
2. Send it 50+ messages with facts scattered throughout ("my name is X", "my dog is Y", "project deadline is Z").
3. Watch the audit log show summarization events.
4. At message 60, ask "what's my name and my dog's name?" — agent knows.
5. Check disk: archive files contain the original raw messages. Current session has summary + recent messages.

## Audit Events

Two new `AuditEventKind` variants:

- `ContextSummarized { messages_summarized: u32, source_range: (usize, usize), tokens_saved_estimate: u32 }`
- `ContextSummarizationFailed { reason: String, fallback: String }`

## Testing Strategy

- **Unit tests:** `TokenBudget` parsing ("128k" → 131_072), estimation (chars/4), threshold detection, min(config, model) reconciliation.
- **Unit tests:** `ContextManager` with a mock LLM client — verify summarization triggers at the right threshold, fallback on LLM error, atomic tool call/result pair handling, summary folded into system prompt.
- **Unit tests:** `SessionStore` archive methods — write, read, prune (TTL), UUID-based filenames avoid collision.
- **Unit tests:** `Message::Summary` serde round-trip (JSONL persistence).
- **Integration test:** Persistent agent with a mock LLM that returns large responses to fill the token budget quickly. Verify summarization fires, archive is created, agent continues with coherent context.
- **Live API test:** Same as the "What You'll See" scenario above. Real Haiku, 50+ turns, verify memory preservation across summarization boundaries.

## Dependencies

- No new crate dependencies. Uses existing `LlmClient`, `SessionStore`, `Message` types.
- Requires Phase B (persistent agents, session store) — already complete.

## Out of Scope

- Episodic store (C2)
- Automatic fact extraction during summarization (C2 v2)
- Shared knowledge graph (C3)
- Automatic RAG retrieval
- Local tokenizer (using chars/4 heuristic instead)
