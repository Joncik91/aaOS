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

Built-in tools: `echo`, `web_fetch`, `file_read`, `file_read_many`, `file_list`, `file_write`, `spawn_agent`, `spawn_agents`, `memory_store`, `memory_query`, `memory_delete`, `skill_read`. External tools integrate via the Tool trait.

**`file_read_many`** — Batch read of 2-16 files in parallel. Each path is capability-checked individually; per-file failures (capability denied, not found, too large) appear in the result array alongside successes so one bad path doesn't abort the batch. Introduced in the Phase 1 speed work after Run 7b's code-reader spent ~4m of ~5m37s on sequential `file_read` loops. Cuts scan-phase latency 3-5x compared to per-file loops. Explicit opt-in (tool-level) rather than executor-level parallelism — same-turn tool calls can be semantically dependent, so generic parallelism is a footgun.

**`spawn_agents`** — Batch version of `spawn_agent`. Spawns up to `AAOS_SPAWN_AGENTS_BATCH_CAP` (default 3) independent children concurrently and returns their results indexed to input order. **Best-effort semantics**: preflight is a fast-fail snapshot against the agent limit (not atomic — snapshot may be stale by fan-out); past preflight each child is independent — one child's failure does not abort siblings. Per-child cleanup reuses `SpawnAgentTool`'s scopeguard via delegation, so panics and errors all funnel through the registry's centralized `remove_agent`. A task-level panic (programming bug) surfaces as a batch error but the JoinSet is drained so non-panicking children's cleanup still runs. Use when subtasks are independent (e.g., scanning different crates); use sequential `spawn_agent` with `prior_findings` when a child's output feeds the next.

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
- **Stable Bootstrap Identity (opt-in)** — Normally every agent gets a fresh runtime-generated UUID at spawn. Bootstrap is the exception: its `AgentId` is resolved from `AAOS_BOOTSTRAP_ID` or `/var/lib/aaos/bootstrap_id` so episodic memory accumulates across container restarts. Only the Bootstrap path uses `AgentRegistry::spawn_with_id()`; regular `agent.spawn` RPC is unchanged. Reset via `AAOS_RESET_MEMORY=1`. The `AgentId::from_uuid()` constructor is kernel-only and flagged as such — a concession to persistence that slightly bends the "IDs are fresh kernel-generated process IDs" model. Long-term a separate *system memory identity* distinct from `AgentId` may be cleaner.
- **Cross-run learning (opt-in, minimal)** — When `AAOS_PERSISTENT_MEMORY=1`, the `run-aaos.sh` launcher bind-mounts the host's `./memory/` into `/var/lib/aaos/memory`, so the SQLite episodic store and the stable Bootstrap ID survive restarts. The Bootstrap manifest instructs the agent to `memory_query` before decomposing a goal and `memory_store` a compact run summary after completion. Children do not persist — they return findings to Bootstrap, which decides what to keep. Deliberately minimal: no new crate, no pattern schema, no reflection service — just existing primitives wired up. The plan is to observe 10-20 runs, then design a structured `PatternStore` only if recurring patterns justify one. Per-run behavior and lessons are logged in [`docs/reflection/`](reflection/README.md).
- **Safety Guardrails** — Agent count limit (100), spawn depth limit (5), parent⊆child capability enforcement, automatic retry of failed children.
- **Stable-identity gate on private memory** — `SpawnAgentTool` refuses any child manifest that declares `tool: memory_store`, and `AgentRegistry::spawn_with_tokens` defensively rejects the capability. `AgentProcess.persistent_identity` (runtime-owned, set only by `spawn_with_id`) marks agents with stable identity; only those may hold private memory. Introduced after run 6 observed Bootstrap granting children `memory_store` despite manifest prose forbidding it — "prompts persuade, only the kernel enforces."
- **Structured child-to-child handoff** — `spawn_agent` tool accepts an optional `prior_findings: string` field (≤ 32 KB). The `aaos-runtime::handoff` module wraps it with kernel-authored BEGIN/END delimiters, a timestamp, the parent agent name, and a prompt-injection warning. The parent LLM cannot remove the wrapping. Introduced after run 6 observed a `proposal-writer` confabulating when no structured channel existed for the prior `code-analyzer`'s output. Caveat: this is parent-provided continuity, not cryptographic provenance — a future handoff-handle design would verify findings against the audit log.
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
- **No ambient authority at the agent level** — Agents start with zero capabilities. The runtime process (`agentd`) itself still runs with ambient OS authority; Phase F plans Landlock-backed enforcement as a kernel-level backstop.
- **Handle-opaque, runtime-issued tokens** — Agents and tool implementations hold `CapabilityHandle` values (a `u64` wrapper). The underlying `CapabilityToken` and its mutable state live inside a runtime-owned `CapabilityRegistry` (`crates/aaos-core/src/capability_registry.rs`). Tool code never sees a `CapabilityToken`; it calls `registry.permits(handle, agent_id, cap)` for checks or `registry.authorize_and_record(...)` for invocation paths that should consume `max_invocations`. A forged handle either resolves to nothing (unknown index) or to a token owned by a different agent (cross-agent leak protection built into `resolve`).
- **Not cryptographically unforgeable.** Attackers with Rust-level code execution inside `agentd` can still read or mutate the registry's DashMap directly. Closing that gap requires HMAC-signed tokens or table-of-seals isolation — deferred hardening items, tracked in `docs/ideas.md`. The handle-opacity shipped today closes the most common in-process forgery path (third-party tool code constructing tokens) without the complexity of cross-process crypto.
- **Narrowable only** — Tokens can be constrained further, never escalated. Narrowing happens via `CapabilityRegistry::narrow(parent_handle, parent_agent, child_agent, additional)`, which atomically validates the parent's ownership, clones the token with the narrower constraints applied, and issues a fresh handle owned by the child.
- **Revocable** — Revocation flips `revoked_at` on the registry-held token; subsequent `authorize_and_record` calls return `CapabilityDenied::Revoked`. `AgentRegistry::revoke_capability()` and `revoke_all_capabilities()` delegate to the registry. `CapabilityRevoked` audit event schema unchanged.
- **Audited** — Every grant, denial, and revocation is logged. Durability depends on the configured audit backend.
- **Scope of enforcement** — Bundled tools (in `aaos-tools`) check via the registry at the call boundary. Third-party tool plugins must also route through the registry — the runtime hands them the handle, not the token, so direct inspection isn't possible without a registry reference. The registry's mutation API (`insert`, `narrow`, `revoke`, `remove_agent`) is marked `pub` with `RUNTIME-INTERNAL` rustdoc warnings for cross-crate accessibility from `aaos-runtime`; discipline is naming-convention-enforced rather than visibility-enforced because `pub(crate)` can't cross crate boundaries.
- **Kernel-level enforcement (namespaced backend only)** — When an agent runs under `NamespacedBackend`, the worker subprocess applies Landlock + seccomp (after `PR_SET_NO_NEW_PRIVS`) before entering the agent loop, and all tool invocations route through a peer-creds-authenticated Unix socket to the broker in `agentd`. The worker holds no `CapabilityHandle` values at all. This closes the in-process memory-attack threat class entirely for those agents. In-process backend agents continue to rely on handle opacity + registry discipline. Scaffolding is landed; the kernel launch path is pending manual verification on a Linux 5.13+ host (see the Agent Backends section below).

## Agent Backends

`AgentServices` is the agent-facing ABI; `AgentBackend` is the lower-level
"how do I actually run an agent's execution context" contract. Two backends
exist today, with a clean path to more:

- **`InProcessBackend`** (`crates/aaos-runtime/src/backend_in_process.rs`) —
  Today's default. Spawns a tokio task running `persistent_agent_loop` in
  the same process as `agentd`. Low overhead, trusts the process boundary.
- **`NamespacedBackend`** (`crates/aaos-backend-linux/src/lib.rs`) — Opt-in
  via `namespaced-agents` feature and `AAOS_DEFAULT_BACKEND=namespaced` env
  var. Scaffolding landed (handshake protocol, peer-creds session binding,
  Landlock + seccomp compilers, worker binary, fail-closed Landlock probe).
  The `clone() + uid_map + pivot_root + exec` launch path is pinned by a
  unit test but not yet functional; completion requires manual verification
  on a Linux 5.13+ host with root or user-namespace privileges.

The opaque `AgentLaunchHandle::state: Arc<dyn Any>` pattern means future
backends (Phase G MicroVM via Firecracker/Kata, a possible seL4 backend)
require zero changes to `aaos-core` — only a new crate implementing the
trait.

### Capability enforcement on the namespaced backend

Worker subprocess applies confinement AFTER `execve`, in this order:
1. `prctl(PR_SET_NO_NEW_PRIVS, 1)` — required for unprivileged Landlock
   and seccomp to take effect.
2. Build Landlock ruleset from policy description received over broker
   socket, then `landlock_restrict_self()`.
3. Build seccomp-BPF allowlist (runtime + broker IPC only; denies
   execve, ptrace, direct network, mount operations, privilege changes),
   then `seccomp(SECCOMP_SET_MODE_FILTER)`.
4. Send `sandboxed-ready` ack.

The parent's `launch()` returns `Ok(handle)` only after receiving
`sandboxed-ready` — confirming the subprocess is actually confined before
any agent-visible work begins.

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
- Context summarized/summarization failed (context window management). `ContextSummarizationFailed` carries a typed `SummarizationFailureKind` (`llm_call_failed`, `empty_response`, `boundary_selection`, `reply_parse_error`) alongside the free-form reason, so operators can route on category without string parsing.
- Memory stored/queried (episodic memory, with content/query hashes)
- Session-store error (persistent-agent on-disk history write failed; emitted with `operation` = `clear`|`append` and a throttle of one event per minute per agent to avoid log spam from a persistently-broken store)

Events include trace IDs for request-level correlation and parent event IDs for causal tracing. 22 event kinds total.
