# Roadmap

The prototype demonstrates that agent-first OS abstractions work: capability-based security, structured IPC, tool execution with two-level enforcement, agent orchestration with capability narrowing, and human-in-the-loop approval. Everything below builds on this foundation.

## Phase B: Persistent Agents & Request-Response IPC *(complete)*

Persistent agents run continuously in a tokio background task, processing messages sequentially from a channel. Request-response IPC uses a `DashMap<Uuid, oneshot::Sender>` pending-response map on the router. Conversation history persists in JSONL files via a `SessionStore` trait, loaded once at startup and appended after each turn.

**What was built:** `persistent_agent_loop()`, `start_persistent_loop()` on registry, `send_and_wait()` on `AgentServices`, `SessionStore` trait + `JsonlSessionStore`, `run_with_history()` on `AgentExecutor` with transcript delta, `max_history_messages` config, Pause/Resume/Stop commands, 3 new audit events, `MailboxFull`/`Timeout` error variants. 141 tests (30 new), verified end-to-end with real Haiku 4.5 API.

**What this enables:** Agents that remember context across interactions. Multi-agent workflows where peers communicate directly via `send_and_wait`. The foundation for the NarrativeEngine orchestration layer.

## Phase C: Agent Memory System *(C1+C2 complete, C3 deferred)*

**C1: Managed context windows.** *(complete)* The runtime manages what's in the agent's context window. When the conversation grows too long, `ContextManager` summarizes older messages via an LLM call and archives the originals to disk. The agent sees a coherent conversation; the runtime handles the compression transparently. `TokenBudget` estimates context size using a chars/4 heuristic, triggering summarization at a configurable threshold (default 70%). Summary messages are folded into the system prompt prefix, preserving User/Assistant turn alternation. Tool call/result pairs are kept atomic during summarization. Fallback to hard truncation on LLM failure.

**What was built (C1):** `TokenBudget` type with `from_config()`, `ContextManager` with `prepare_context()`, `Message::Summary` variant, `ArchiveSegment` + archive methods on `SessionStore` trait, `LlmClient::max_context_tokens()`, `run_with_history_and_prompt()` on `AgentExecutor`, 2 new audit events. 25 new tests (166 total). Verified end-to-end with real Haiku 4.5 — summarization preserves facts across compression boundaries.

**C2: Episodic store.** *(complete)* Per-agent persistent memory via explicit `memory_store`, `memory_query`, and `memory_delete` tools. Agents store facts, observations, decisions, and preferences. Later, they query by meaning via cosine similarity over embeddings. In-memory store with brute-force search (SQLite+sqlite-vec planned for persistence). Embeddings via Ollama's nomic-embed-text model (768 dims, OpenAI-compatible `/v1/embeddings` endpoint).

**What was built (C2):** New `aaos-memory` crate (7th workspace member) with `MemoryStore` trait, `InMemoryMemoryStore` (cosine similarity, agent isolation, LRU cap eviction, replaces/update semantics, dimension mismatch handling), `EmbeddingSource` trait with `MockEmbeddingSource` and `OllamaEmbeddingSource`. Three new tools in `aaos-tools`. `MemoryConfig` with episodic fields. 2 new audit events. 39 new tests (205 total). Verified end-to-end with real Haiku + Ollama nomic-embed-text.

**C3: Shared knowledge graph.** *(deferred)* Cross-agent knowledge sharing. Design direction documented but not buildable — requires C1+C2 production usage, cross-agent capability model, proven multi-agent need. See `docs/phase-c3-design.md` (local only).

**What this enables:** Agents that learn from experience. A persistent agent that remembers facts across summarization boundaries. Agents that explicitly store and retrieve knowledge by meaning. The foundation for shared intelligence (C3) when multi-agent patterns prove the need.

## Phase D: Supervision Dashboard

A web-based UI for humans to observe, steer, and intervene — the "desktop environment" for the agent OS.

**Activity monitor.** Real-time view of all running agents, their states, current tools in use, and token consumption. Like `htop` for agents.

**Audit trail viewer.** Every action is already logged. The dashboard makes it navigable: search by agent, filter by event type, trace from any action back to root cause through parent events and trace IDs.

**Approval queue UI.** The `approval.list` / `approval.respond` API already exists. The dashboard wraps it: see pending requests with full context (agent name, tool, input, file path), approve or deny with one click.

**Policy editor.** System-wide rules that apply across agents: token budget limits, auto-deny patterns (never approve writes to certain paths), rate limiting, model restrictions. Policies are enforced by the kernel, configured through the dashboard.

**Architecture:** A thin client over the existing Unix socket API. The daemon already serves all the data — the dashboard just presents it. No new backend logic required, only a frontend.

## Phase E: Inference Scheduling & Local Models

Treat LLM inference as a schedulable resource, like CPU time.

**Local model support.** Integrate Ollama or vLLM as `LlmClient` implementations alongside `AnthropicClient`. The manifest's `model` field determines which provider handles the request. Different agents can use different models: cheap local models for routine tasks, powerful API models for complex reasoning.

**Inference scheduling.** Multiple agents competing for inference time need a scheduler. The existing `RoundRobinScheduler` (implemented but dormant) becomes the inference queue. Priority-based scheduling: a critical agent gets inference before a background scanner. Budget enforcement: per-agent token limits, per-session cost caps.

**KV cache management.** For local models, the KV cache is the equivalent of virtual memory. A persistent agent's cache should survive between turns. The runtime manages cache allocation, eviction, and sharing — agents with overlapping context (same codebase, same docs) can share cache entries.

**What this enables:** Cost-effective agent fleets. A team of 20 agents where 15 run on a local 7B model and 5 use Claude for the hard decisions. GPU/NPU allocation as a kernel concern, not an application concern.

## Phase F: Real Kernel Migration

Move from userspace abstractions on Linux to a real capability-based microkernel.

**Target kernels.** Redox OS (Rust-native, capability-based, active development) or seL4 (formally verified). The agent syscall API is already defined by the `AgentServices` trait — the migration replaces the implementation, not the interface.

**What changes.** Capability tokens become kernel objects, not userspace UUIDs. Agent isolation uses hardware-enforced process boundaries, not Docker containers. The audit trail is a kernel subsystem, not an application-level log. IPC uses kernel message passing, not Unix sockets.

**What stays the same.** The `AgentServices` trait. The `Tool` trait. The manifest format. The API methods. Everything above the kernel — the entire agent programming model — is unchanged. Applications (agent manifests, tools, orchestration logic) work identically. This is the Android pattern: the app model is the product, the kernel is an implementation detail.

**Prerequisites.** Phases B through E must be battle-tested before this begins. The abstractions need to prove themselves under real workloads before being baked into a kernel where changes are expensive.
