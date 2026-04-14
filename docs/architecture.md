# aaOS Architecture

## Overview

aaOS is an agent runtime organized as a six-layer stack, each layer providing services to the one above it.

**Current state:** The runtime runs as a daemon on Linux, isolated in Docker. The abstractions are designed to survive a future migration to a real capability-based microkernel (see [Roadmap](roadmap.md)): the `AgentServices` trait is the future syscall interface, and the `Tool` trait is the future driver model. Code written against these interfaces today will work unchanged on a real kernel.

## Layer Details

### 1. Hardware Abstraction Layer

Agents request compute capabilities (GPU time, network access), not device handles. Resources are allocated via capability tokens, enabling fair scheduling and budget enforcement.

**Status:** Future work. Currently relies on host OS for hardware access.

### 2. Agent Kernel (`aaos-runtime`)

The core of the system. Manages:

- **Agent Processes** — State machine: Starting → Running → Paused → Stopping → Stopped. Each process holds capability tokens, command/message channels, and an optional `JoinHandle` for persistent loop tasks.
- **Agent Registry** — Thread-safe process table (DashMap-based). Handles spawn, stop (sync and async), capability issuance, and persistent loop startup.
- **Persistent Agent Loop** — Agents with `lifecycle: persistent` run a background tokio task (`persistent_agent_loop`) that receives messages from a channel, executes them with conversation history via the LLM executor, persists the transcript, and responds via the router's pending-response map. Survives executor errors. Supports Pause/Resume/Stop commands.
- **Session Store** — `SessionStore` trait with `JsonlSessionStore` (JSONL files, one per agent) and `InMemorySessionStore` (for tests). History loaded once at loop startup, appended after each turn, compacted every 10 turns. Configurable via `max_history_messages`.
- **Scheduler** — Round-robin with priority support (implemented, not yet activated for agent-level scheduling)
- **Supervisor** — Restart policies (always, on-failure, never) with exponential backoff
- **Budget Enforcement** — `BudgetTracker` with atomic CAS operations tracks per-agent token usage. `BudgetConfig` in agent manifest (optional `max_tokens` + `reset_period_seconds`). Enforced in `report_usage()` — agents exceeding budget get `BudgetExceeded` errors. No budget = no enforcement.

### 3. Agent Memory Layer (`aaos-memory`)

Three memory tiers:

- **Context Window** — Managed by `ContextManager` in the runtime, not the agent. When the conversation grows too long (estimated via chars/4 heuristic against a configurable `TokenBudget`), the runtime summarizes older messages via an LLM call and archives the originals to `ArchiveSegment` files. Summary messages are folded into the system prompt prefix, preserving API turn alternation. Tool call/result pairs are kept atomic during summarization selection. Fallback to hard truncation on LLM failure. Configurable summarization threshold (default 0.7) and model.
- **Conversation Persistence** — JSONL session store keyed by agent ID. Persistent agents load history at startup and append after each turn. `run_with_history_and_prompt()` on the executor accepts an overridden system prompt (for summary prefix injection). Archive segments stored as `{agent_id}.archive.{uuid}.json` files with TTL-based pruning.
- **Episodic Store** — Per-agent vector-indexed persistent memory via `MemoryStore` trait. Agents explicitly store facts, observations, decisions, and preferences via `memory_store` tool, and retrieve them by meaning via `memory_query` tool (cosine similarity over embeddings). Two backends: `InMemoryMemoryStore` (default, volatile) and `SqliteMemoryStore` (persistent across container restarts, set via `AAOS_MEMORY_DB`). Both use brute-force cosine similarity in Rust, agent isolation, atomic replaces, LRU cap eviction. Embeddings via `EmbeddingSource` trait — `OllamaEmbeddingSource` (nomic-embed-text, 768 dims) for production, `MockEmbeddingSource` for tests.
- **Shared Knowledge** — Cross-agent semantic storage (deferred — requires proven multi-agent patterns)

### 4. Tool & Service Layer (`aaos-tools`)

Universal tool registry where every capability is:
- Registered with a JSON schema
- Discoverable by agents
- Invoked through capability-checked channels
- Logged to the audit trail

Built-in tools: `echo`, `web_fetch`, `file_read`, `file_list`, `file_write`, `spawn_agent`, `memory_store`, `memory_query`, `memory_delete`, `skill_read`. External tools integrate via the Tool trait.

**`file_list`** — List directory contents (name, kind, size) or return metadata for a single file. Introduced after run 4 analysis showed children were guessing paths and calling `file_read` on directories to explore them. Uses the same `FileRead` capability glob as `file_read`, same lexical path normalization — capability model unchanged.

**AgentSkills Support** — Implements the [AgentSkills](https://agentskills.io) open standard by Anthropic. Skills are folders with `SKILL.md` files containing YAML frontmatter + markdown instructions. `SkillRegistry` discovers skills at startup from `/etc/aaos/skills/` and `AAOS_SKILLS_DIR`. Skill catalog (names + descriptions) injected into agent system prompts (progressive disclosure tier 1). `skill_read` tool serves full instructions and reference files on demand (tiers 2+3). Path traversal protection on reference file reads. 21 production-grade skills bundled from addyosmani/agent-skills.

**Constraint Enforcement** — `CapabilityToken.permits()` checks `max_invocations` against `invocation_count`. `record_use()` increments the counter after successful operations. Tokens with exhausted invocation limits are denied. Previously constraints were declared but never enforced — found and fixed by the system's own self-reflection.

**Inference Scheduling** — `ScheduledLlmClient` decorator wraps any `LlmClient` with a `tokio::sync::Semaphore` for concurrency control (default max 3 concurrent API calls). Optional rate smoothing via minimum delay between calls. Configurable via `AAOS_MAX_CONCURRENT_INFERENCE` and `AAOS_MIN_INFERENCE_DELAY_MS`. Prevents API stampedes when multiple agents fire simultaneously.

**Multi-Provider LLM** — `AnthropicClient` (Anthropic Messages API) and `OpenAiCompatibleClient` (any OpenAI-compatible API — DeepSeek, OpenRouter, etc.). The daemon checks `DEEPSEEK_API_KEY` first, falls back to `ANTHROPIC_API_KEY`. Model-specific `max_tokens` capping (deepseek-chat: 8192, deepseek-reasoner: 32768). Bootstrap Agent uses deepseek-reasoner (thinking mode), children use deepseek-chat.

### 5. IPC Layer (`aaos-ipc`)

MCP-native inter-agent communication:

- **McpMessage** — JSON-RPC 2.0 envelope with aaOS metadata (sender, recipient, trace_id, capability token)
- **McpResponse** — Success/error response with responder metadata
- **MessageRouter** — Routes messages with capability validation. Supports both fire-and-forget (`route()`) and request-response (`register_pending()` / `respond()`) via a `DashMap<Uuid, oneshot::Sender<McpResponse>>` pending-response map.
- **SchemaValidator** — Validates payloads against registered schemas
- **`send_and_wait()`** — Method on `AgentServices` for request-response IPC. Creates a oneshot channel, registers it on the router, routes the message, and awaits the response with a configurable timeout. Capability-checked.

### 6. Bootstrap & Orchestration Layer

The system can run autonomously in a Docker container with `agentd` as PID 1:

- **Bootstrap Agent** — A persistent DeepSeek Reasoner agent that receives goals, decomposes them into agent roles, writes child manifests, spawns children (DeepSeek Chat) with narrowed capabilities, coordinates work, and produces output. Few-shot manifest examples in the system prompt guide reliable YAML generation.
- **Persistent Goal Queue** — Bootstrap runs as a persistent agent accepting goals via the Unix socket API. Container stays alive between tasks.
- **Workspace Isolation** — Each goal gets `/data/workspace/{name}/`. Children write intermediate files there. Output goes to `/output/`.
- **Stable Bootstrap Identity (opt-in)** — Normally every agent gets a fresh unforgeable UUID at spawn. Bootstrap is the exception: its `AgentId` is resolved from `AAOS_BOOTSTRAP_ID` or `/var/lib/aaos/bootstrap_id` so episodic memory accumulates across container restarts. Only the Bootstrap path uses `AgentRegistry::spawn_with_id()`; regular `agent.spawn` RPC is unchanged. Reset via `AAOS_RESET_MEMORY=1`. The `AgentId::from_uuid()` constructor is kernel-only and flagged as such — a concession to persistence that slightly bends the "IDs are fresh kernel-generated process IDs" model. Long-term a separate *system memory identity* distinct from `AgentId` may be cleaner.
- **Cross-run learning (opt-in, minimal)** — When `AAOS_PERSISTENT_MEMORY=1`, the `run-aaos.sh` launcher bind-mounts the host's `./memory/` into `/var/lib/aaos/memory`, so the SQLite episodic store and the stable Bootstrap ID survive restarts. The Bootstrap manifest instructs the agent to `memory_query` before decomposing a goal and `memory_store` a compact run summary after completion. Children do not persist — they return findings to Bootstrap, which decides what to keep. Deliberately minimal: no new crate, no pattern schema, no reflection service — just existing primitives wired up. The plan is to observe 10-20 runs, then design a structured `PatternStore` only if recurring patterns justify one. Per-run behavior and lessons are logged in [`docs/reflection-log.md`](reflection-log.md).
- **Safety Guardrails** — Agent count limit (100), spawn depth limit (5), parent⊆child capability enforcement, automatic retry of failed children.
- **StdoutAuditLog** — Audit events streamed as JSON-lines to stdout for `docker logs -f` observability.

### 7. Human Supervision Layer

Read-only observation of the autonomous system. Deliberately last — the system must be functional without it.

**Status:** `StdoutAuditLog` provides JSON-lines observability. Verbose executor logging streams full agent thoughts, tool calls with arguments, and tool results. Live dashboard script (`tools/dashboard.py`) for terminal-based monitoring. `run-aaos.sh` launcher auto-opens dashboard in a separate terminal. Web dashboard is future work.

## Capability Security Model

```
Agent Manifest declares capabilities
        ↓
Kernel issues CapabilityTokens at spawn
        ↓
Every operation validates token
        ↓
Denied operations logged to audit trail
```

Key properties:
- **No ambient authority** — Agents start with zero permissions
- **Unforgeable** — Tokens are UUID-identified, kernel-issued
- **Narrowable only** — Tokens can be constrained further, never escalated
- **Revocable** — Tokens can be revoked at runtime; `permits()` checks revocation on every call. `revoke_capability()` and `revoke_all_capabilities()` on the registry. `CapabilityRevoked` audit event.
- **Audited** — Every grant, denial, and revocation is logged

## Audit Trail

Every action in aaOS produces an `AuditEvent`:

- Agent spawned/stopped
- Capability granted/denied
- Tool invoked (with input hash)
- Message sent/delivered
- Human approval requested/granted/denied
- Agent execution started/completed
- Agent loop started/stopped (persistent agents)
- Agent message received (persistent agents, with trace_id)
- Context summarized/summarization failed (context window management)
- Memory stored/queried (episodic memory, with content/query hashes)

Events include trace IDs for request-level correlation and parent event IDs for causal tracing. 21 event kinds total.
