# aaOS Architecture

## Overview

aaOS is an agent runtime organized as a seven-layer stack, each layer providing services to the one above it: agent backend → runtime core → IPC → memory → tools → orchestration → human supervision.

**Current state:** Installable as a Debian `.deb` with a systemd unit + operator CLI, or runnable as a Docker container with `agentd` as PID 1. Both paths use the same daemon binary. The abstractions are designed to survive a future migration to a real capability-based microkernel (see [Roadmap](roadmap.md)): the `AgentServices` trait is the future syscall interface, and the `Tool` trait is the future driver model. Code written against these interfaces today will work unchanged on a real kernel. The pluggable `AgentBackend` trait lets a future MicroVM-per-agent (Firecracker/Kata/gVisor) backend land as a new crate without touching `aaos-core`.

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
- **Schedulers** — Two coexisting scheduler abstractions. The legacy `RoundRobinScheduler` with priority support is implemented but not yet activated for agent-level scheduling. The new `ReasoningScheduler` (build-history #10) awards LLM inference slots via a priority queue keyed on per-subtask wall-clock deadlines; every subtask's LLM client is wrapped in a `SchedulerView` so `complete()` calls route through it. See the Reasoning-Slot Scheduler section below.
- **Per-task TTL** — Optional `TaskTtl { max_hops, max_wall_clock }` on `Subtask`, populated from env defaults (`AAOS_DEFAULT_TASK_TTL_HOPS`, `AAOS_DEFAULT_TASK_TTL_WALL_CLOCK_S`) when the planner's output omits it. Enforced in `PlanExecutor::spawn_subtask`: hops-exhaustion refuses launch; a `tokio::select!` race cancels the runner future when the wall-clock deadline fires. Failure cascades to dependents via the existing partial-failure logic.
- **Supervisor** — Restart policies (always, on-failure, never) with exponential backoff
- **Budget Enforcement** — `BudgetTracker` with atomic CAS operations tracks per-agent token usage. `BudgetConfig` in agent manifest (optional `max_tokens` + `reset_period_seconds`). Enforced in `report_usage()` — agents exceeding budget get `BudgetExceeded` errors. No budget = no enforcement.

#### Reasoning-Slot Scheduler (build-history #10)

Every `agentd` server owns one `ReasoningScheduler` that gates LLM inference calls for **subtask agents** (children spawned by the PlanExecutor). Construction reads `AAOS_MAX_CONCURRENT_INFERENCE` (default 3) once per server. On startup, a single `dispatcher_loop` tokio task is spawned; it repeatedly pops the earliest-deadline `ReasoningRequest` off a `BinaryHeap<Reverse<...>>`, acquires a permit from an inner `Semaphore`, and hands the permit to the request's `oneshot::Sender<OwnedSemaphorePermit>`.

Each subtask's LLM client is wrapped in a `SchedulerView` before `AgentExecutor` touches it. `SchedulerView::complete` calls `scheduler.acquire_slot(subtask_id, priority, deadline)` first, then delegates to the inner client, then records the elapsed time into a `LatencyTracker`. One slot = one `complete()` call; no mid-inference preemption. Requests without a deadline get a 60-second synthetic one so no-TTL work competes fairly against short-deadline peers. If a caller drops the future before the dispatcher hands the permit over, the dispatcher discards that permit and loops — preventing dropped-waker stalls.

**Scope — what SchedulerView does and does NOT wrap.** The scheduler wraps subtask-agent LLM calls (everything reached through `Server::execute_agent_for_subtask`). It does NOT wrap the Planner's own DAG-producing LLM call or the Bootstrap agent's top-level LLM loop — those still go through the raw `llm_client` directly. Inference concurrency for non-subtask traffic is bounded by the legacy `ScheduledLlmClient` semaphore (constructed around the underlying LLM client in `agentd::main`). This means `AAOS_MAX_CONCURRENT_INFERENCE` is still the load-bearing backstop. The new scheduler is an inner gate for subtask-agent work, not a wholesale replacement.

The `LatencyTracker` trait has a minimal `SubtaskWallClockTracker` impl; sub-project 2 adds `PerModelLatencyTracker` as a second impl keyed by model name (256-sample bounded ring per model, p50/p95 queries) and a `CompositeLatencyTracker` that fans out `record()` to both.

#### Dynamic Model Routing (build-history #11)

Each `Role` declares a `model_ladder: Vec<String>` (ordered list; tier 0 == `role.model`, invariant enforced at catalog load) and an `escalate_on: Vec<EscalationSignal>` (defaults to all three). `Subtask.current_model_tier: u8` indexes into the ladder at spawn time — `role.render_manifest_with_model(ladder[tier], params)` produces the per-subtask manifest. The executor's replan path runs `decide_escalation` on each failed subtask against a structured `Vec<FailedSubtask>` (carried through `ExecutorError::Correctable.failures`); on a configured signal it bumps the tier up to `ladder.len() - 1`, emits `SubtaskModelEscalated`, and `carry_tiers_forward` merges the bump into the planner's new plan by subtask-id match.

Three escalation signals, in priority order (highest wins when multiple fired):
1. `ReplanRetry` — any `SubtaskCompleted{success: false}` for this subtask in the failed attempt.
2. `MaxTokens` — any `AgentExecutionCompleted{stop_reason: "MaxTokens"}` for this subtask's agent id.
3. `ToolRepeatGuard` — any `ToolRepeatGuardFired` for this subtask's agent id.

Signals are scanned from the audit broadcast via `AuditLog::events_snapshot()` (default-empty trait method; `InMemoryAuditLog` + `BroadcastAuditLog` override).

**Scope:** signal-based routing only; no cost math, no classifier. `PerModelLatencyTracker` collects per-model p50/p95 into 256-sample bounded rings but is not consumed by any routing decision in v1 — future cost-aware routing can read from it.

#### Worker-Side Tool Confinement (build-history #12)

When `AAOS_DEFAULT_BACKEND=namespaced`, agent tool invocations execute inside the worker under Landlock + seccomp — not in `agentd`'s address space. The daemon keeps policy (capability check + audit + repeat-guard); the broker stream carries each call across to the worker via `Request::InvokeTool { tool_name, input, request_id }` and returns the result through a `pending: HashMap<u64, oneshot::Sender>` demux on the existing post-handshake stream.

**What runs where:**
- **Worker-side** (confined, v1): `file_read`, `file_write`, `file_edit`, `file_list`, `file_read_many`, `grep`. Filesystem + pure-compute tools. Whitelisted explicitly in `aaos_backend_linux::worker_tools::WORKER_SIDE_TOOLS` — fail-closed on unknown names.
- **Daemon-side permanently**: `web_fetch` (outbound network), `cargo_run` + `git_commit` (subprocess execution). The worker's seccomp allowlist has no `socket`/`connect` and the kill-filter denies `execve` — both by design. Routing these tools through the worker would require a broker-mediated network proxy or a broker-mediated subprocess runner; in both cases the daemon would still be the component doing the actual network/subprocess work, so the round-trip would move no security line. Better to name the split clearly: filesystem + compute tools run under confinement; network + subprocess tools do not. Operators see the distinction in the CLI via the `[worker]`/`[daemon]` tag. Not a scope note — a stable design choice.

**Routing fork** lives at a single point in `aaos_tools::ToolInvocation::invoke` — after capability check + audit prefix, `route_for(tool_name, backend_kind)` returns `ToolExecutionSurface::{Daemon, Worker}`. The surface flows into the post-execution `ToolInvoked` audit event so operators can see in the CLI stream whether confinement ran (`[worker]` green) or not (`[daemon]` dim).

**Landlock is the second gate, not the only gate.** Daemon-side capability grants are narrower than the worker's Landlock scope by design: a capability grant to `/data` still fails at Landlock if the worker's scratch is `/var/lib/aaos/session-<id>`. That's correct — operators see `ToolDenied { reason: "landlock: ..." }` — but it also means every worker-side tool call runs inside a *narrower* policy than the one that approved it.

**Out of scope v1:** LLM loop stays daemon-side (keeps API keys out of the sandbox); per-tool Landlock scoping (all worker tools share one ruleset); mid-invoke cancellation (60s timeout + worker-kill fallback).

**Which agents get worker confinement, honestly.**
- **`spawn_agent`-launched children** — automatically go through `backend.launch`, get a worker session, tool calls route worker-side. Default behaviour on namespaced builds.
- **Plan-executor subtasks (LLM roles: analyzer, writer, generalist)** — spawn inline via `execute_agent_for_subtask` which now calls `NamespacedBackend::launch` for each subtask when `AAOS_CONFINE_SUBTASKS=1` (default on). The LLM loop stays in the daemon (keeps API keys out of the sandbox); only tool invocations cross the broker. Workspace + declared output paths are bind-mounted into the worker at the same absolute path (see "Three layers of confinement" below). Set `AAOS_CONFINE_SUBTASKS=0` to force legacy daemon-side execution.
- **Scaffold roles (fetcher, anything with `scaffold: {kind: ...}`)** — always run daemon-side. They ARE the workspace plumbing (fetch → write under workspace → hand-off). Trivial Rust, no LLM loop, no security gain from confining them.
- **Inline `NoSession` fallback** — `ToolInvocation::invoke` distinguishes `WorkerInvokeError::NoSession` (agent has no worker — legitimate when an inline path skips `backend.launch`) from `Transport` (real broker failure). On `NoSession`, fall back to daemon-side execution and record `execution_surface: Daemon` on the audit event so the CLI `[daemon]` tag is honest.

**Capability forwarding across the broker.** Each `Request::InvokeTool` carries a `Vec<CapabilityToken>` — the daemon resolves the agent's `CapabilityHandle`s to token structs via `CapabilityRegistry::resolve_tokens()` and includes them in the wire payload. The worker builds a per-call `CapabilityRegistry`, reinserts the tokens, and hands handles into the `InvocationContext`. Tools' internal `ctx.capability_registry.permits(handle, agent_id, &required)` check then succeeds against the forwarded tokens. Revoked/expired tokens still deny correctly — reinsertion preserves `revoked_at`/`expires_at`.

**Three layers of confinement, each doing a different job.** A worker-side tool call must satisfy all three:
- **Bind-mount** provides *visibility*. `AgentLaunchSpec.workspace_path` + `extract_capability_roots(manifest)` flow into `PolicyDescription.workspace` and `extra_writable_roots`. `clone_and_launch_worker` bind-mounts each at the same absolute path inside the worker's mount namespace so tool code using host paths resolves identically daemon-side vs worker-side.
- **Landlock** provides *filesystem access control*. One `PathBeneath` read-write rule per workspace + extra-root path; shared libs are read-only; everything else is denied by the ruleset.
- **Capability tokens** provide *per-call authorization*. Even within Landlock-permitted paths, the tool's own `ctx.capability_registry.permits(handle, agent_id, &required)` check decides if this specific file is in scope for this specific agent.

Missing any layer means the call fails: no bind-mount → ENOENT; no Landlock rule → EACCES/EROFS; no capability → structured `CoreError::CapabilityDenied`. All three must agree.

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

Built-in tools: `echo`, `web_fetch`, `file_read`, `file_read_many`, `file_list`, `file_write`, `file_edit`, `grep`, `spawn_agent`, `spawn_agents`, `memory_store`, `memory_query`, `memory_delete`, `skill_read`, `cargo_run`, `git_commit`. External tools integrate via the Tool trait.

**`file_read_many`** — Batch read of 2-16 files in parallel. Each path is capability-checked individually; per-file failures (capability denied, not found, too large) appear in the result array alongside successes so one bad path doesn't abort the batch. Introduced in the Phase 1 speed work after Run 7b's code-reader spent ~4m of ~5m37s on sequential `file_read` loops. Cuts scan-phase latency 3-5x compared to per-file loops. Explicit opt-in (tool-level) rather than executor-level parallelism — same-turn tool calls can be semantically dependent, so generic parallelism is a footgun.

**`spawn_agents`** — Batch version of `spawn_agent`. Spawns up to `AAOS_SPAWN_AGENTS_BATCH_CAP` (default 3) independent children concurrently and returns their results indexed to input order. **Best-effort semantics**: preflight is a fast-fail snapshot against the agent limit (not atomic — snapshot may be stale by fan-out); past preflight each child is independent — one child's failure does not abort siblings. Per-child cleanup reuses `SpawnAgentTool`'s scopeguard via delegation, so panics and errors all funnel through the registry's centralized `remove_agent`. A task-level panic (programming bug) surfaces as a batch error but the JoinSet is drained so non-panicking children's cleanup still runs. Use when subtasks are independent (e.g., scanning different crates); use sequential `spawn_agent` with `prior_findings` when a child's output feeds the next.

**`file_list`** — List directory contents (name, kind, size) or return metadata for a single file. Introduced after run 4 analysis showed children were guessing paths and calling `file_read` on directories to explore them. Uses the same `FileRead` capability glob as `file_read`, same lexical path normalization — capability model unchanged.

**`cargo_run`** — Run `cargo <subcommand>` in a Rust workspace under a `CargoRun { workspace }` capability. Allowlisted subcommands: `check`, `test`, `clippy`, `fmt` — anything else (`install`, `publish`, custom subcommands) is refused. Workspace must contain a `Cargo.toml`; output is captured (stdout + stderr, 8KB inline cap) with exit code and wall-clock duration in the result. 4-minute timeout per invocation so a runaway build can't hang an agent. Designed to let aaOS build and test Rust code (including itself) without granting a general shell-exec tool.

**`file_edit`** — Surgical find/replace primitive: `{ path, old_string, new_string, replace_all? }`. Refuses the edit if `old_string` matches more than once unless `replace_all: true`, avoiding the common LLM mistake of rewriting the first occurrence when a different one was meant. Requires both `FileRead` and `FileWrite` capability for the path. Matches the Edit-tool idiom from Claude Code, Cursor, and Aider. Added after the first self-build attempt surfaced the "whole-file `file_write` blows the output budget" failure mode: for a 3-line change in a 100KB source file the agent would otherwise have to emit the entire file as one tool-call argument.

**`file_read` with offset + limit** — The same `file_read` tool now takes optional `offset` (1-indexed line number) and `limit` (line count, default 2000) parameters and returns line-numbered content (cat -n style). Lets agents page through large files under their own control instead of dumping whole files into the context, and gives the LLM line numbers it can reference in subsequent `file_edit` calls.

**`grep`** — Regex search primitive backed by ripgrep (`rg`). Input: `{ pattern, path, glob?, case_insensitive? }`. Requires `FileRead` capability on the search root — the tool cannot return results from paths the agent isn't authorized to read. Output is a JSON array of `{ file, line, text }` matches, capped at 200 entries / 16 KB inline output with a truncation flag; per-match text is cut at 512 bytes. 30-second wall-clock timeout. ripgrep is declared as a runtime dep in the `.deb` so a fresh install has the binary available. Added after run 8 of the self-build loop — navigation primitive that closes the "agent knows which file to open" assumption baked into earlier tools.

**`git_commit`** — Run `git add` + `git commit` in a git repository under a `GitCommit { workspace }` capability. Subcommand allowlist is hard-coded to `add` and `commit` (nothing that mutates history or remotes: no push, rebase, reset, checkout, config). Input: `{ workspace, message, paths? }` — paths defaults to `["."]`. Message that starts with `-` is rejected to prevent flag injection; only `-m` is ever passed to git. Workspace must contain a `.git/` directory. Returns exit code, duration, stdout/stderr preview (2 KB cap), and the new commit SHA from `git rev-parse HEAD`. "Nothing to commit" is reported as success with a `nothing_to_commit: true` flag rather than as an error. 60-second timeout per invocation. Designed to let an aaOS agent close its own loop into version control — pair with `cargo_run` and the `file_edit`/`grep` coding surface and a self-build run can land, verify, and persist its work without a general shell-exec tool. Added after run 12 of the self-build loop.

**AgentSkills Support** — Implements the [AgentSkills](https://agentskills.io) open standard by Anthropic. Skills are folders with `SKILL.md` files containing YAML frontmatter + markdown instructions. `SkillRegistry` discovers skills at startup from `/etc/aaos/skills/` and `AAOS_SKILLS_DIR`. Skill catalog (names + descriptions) injected into agent system prompts (progressive disclosure tier 1). `skill_read` tool serves full instructions and reference files on demand (tiers 2+3). Path traversal protection on reference file reads. 21 production-grade skills bundled from addyosmani/agent-skills.

**Constraint Enforcement** — `CapabilityToken.permits()` checks `max_invocations` against `invocation_count`. `record_use()` increments the counter after successful operations. Tokens with exhausted invocation limits are denied. Previously constraints were declared but never enforced — found and fixed by the system's own self-reflection.

**Inference Scheduling** — `ScheduledLlmClient` decorator wraps any `LlmClient` with a `tokio::sync::Semaphore` for concurrency control (default max 3 concurrent API calls). Optional rate smoothing via minimum delay between calls. Configurable via `AAOS_MAX_CONCURRENT_INFERENCE` and `AAOS_MIN_INFERENCE_DELAY_MS`. Prevents API stampedes when multiple agents fire simultaneously.

**Multi-Provider LLM** — `AnthropicClient` (Anthropic Messages API) and `OpenAiCompatibleClient` (any OpenAI-compatible API — DeepSeek, OpenRouter, etc.). The daemon checks `DEEPSEEK_API_KEY` first, falls back to `ANTHROPIC_API_KEY`. Model-specific `max_tokens` capping (deepseek-chat: 8192, deepseek-reasoner: 32768). Bootstrap Agent uses deepseek-reasoner (thinking mode), children use deepseek-chat.

### 5. IPC Layer (`aaos-ipc`)

Internal inter-agent communication uses an aaOS-native JSON-RPC envelope historically branded "MCP" inside the codebase. That internal bus is distinct from the real Model Context Protocol support added in `aaos-mcp` — see the dedicated section below.

- **McpMessage** — JSON-RPC 2.0 envelope with aaOS metadata (sender, recipient, trace_id, capability token). Despite the name, this is not the MCP wire protocol; it's the legacy internal bus.
- **McpResponse** — Success/error response with responder metadata
- **MessageRouter** — Routes messages with capability validation. Supports both fire-and-forget (`route()`) and request-response (`register_pending()` / `respond()`) via a `DashMap<Uuid, oneshot::Sender<McpResponse>>` pending-response map.
- **SchemaValidator** — Validates payloads against registered schemas
- **`send_and_wait()`** — Method on `AgentServices` for request-response IPC. Creates a oneshot channel, registers it on the router, routes the message, and awaits the response with a configurable timeout. Capability-checked.

### 5b. Model Context Protocol Integration (`aaos-mcp`, feature-gated)

New in Phase F. Bidirectional MCP (2024-11 spec) support lives in the `aaos-mcp` crate and is wired into `agentd` behind the `mcp` cargo feature. Config is loaded from `/etc/aaos/mcp-servers.yaml` at startup; if the file is absent, both subsystems are silently disabled and the daemon behaves identically to a non-mcp build.

- **MCP client** — For each configured server (transport: `stdio` or `http`), `aaos-mcp::client::McpClient::connect_and_register` opens a session (JSON-RPC `initialize` → `tools/list`), wraps each remote tool in an `McpToolProxy`, and registers it into the runtime's `ToolRegistry` under the name `mcp.<server>.<tool>`. Proxied tools invoke exactly like built-ins: capability-checked at the registry boundary, audited on invoke/result, narrowable via the existing `Capability::ToolInvoke { tool_name }` mechanism. Per-session reconnect loop runs with exponential backoff (1s → 30s cap). A session that goes unhealthy returns `CoreError::ToolUnavailable` on subsequent calls until it recovers.
- **MCP server** — When `server.enabled: true` in config, an axum HTTP+SSE listener binds `127.0.0.1:3781` (loopback only — no auth; operator's job to expose it over SSH tunnel or Tailscale if remote access is needed). Exposes three tools:
  - `submit_goal(goal, role?)` — routes the goal to the persistent bootstrap agent via the existing `ensure_bootstrap_running()` / `route_goal_to()` path. Returns the bootstrap's `AgentId` as `run_id`.
  - `get_agent_status(run_id)` — returns `running`, `completed`, `failed`, or `notfound`.
  - `cancel_agent(run_id)` — delegates to `AgentRegistry::stop_sync`.
- **Server-Sent Events** — `GET /mcp/events?run_id=<id>` subscribes to the `BroadcastAuditLog` and streams events filtered to the given agent as SSE frames. The stream terminates on client disconnect without affecting the run.
- **No new capability variants** — Remote MCP tools are granted the same way as built-ins: manifest entry `tool: mcp.<server>.<tool>` produces a `Capability::ToolInvoke` for that tool name. The MCP server itself enforces its own input-level auth; aaOS treats the remote as a trusted tool source.

### 6. Bootstrap & Orchestration Layer

The system can run autonomously in a Docker container with `agentd` as PID 1. Two orchestration paths coexist; the active one depends on whether `/etc/aaos/roles/*.yaml` is populated at daemon startup.

- **Computed orchestration (active when `/etc/aaos/roles/` loads).** The `aaos-runtime::plan` module owns two halves. A **Planner** takes the operator's goal + the loaded `RoleCatalog` and emits one structured JSON Plan via a cheap single-shot LLM call (`deepseek-chat`, zero temperature, no tools). A deterministic **PlanExecutor** walks the resulting DAG in dependency-ordered batches (computed by `topo_batches`), spawning each subtask through a per-role scaffold (`Role::render_manifest` + `render_message`) and running independent subtasks concurrently via `futures::try_join_all`. No LLM is in the orchestration loop — orchestration is pure Rust. Four roles ship in `/etc/aaos/roles/`: `fetcher`, `writer`, `analyzer`, `generalist`. Operator-extensible: drop a new YAML in the directory, restart the daemon. When the Planner fails its initial call (malformed JSON, no match), the runtime falls back to a single `generalist` subtask — the goal always runs. `plan.json` persists at `/var/lib/aaos/workspace/<run-id>/` for operator inspection.
- **Bootstrap Agent (fallback when catalog absent).** A persistent DeepSeek Reasoner agent that receives goals, decomposes them into agent roles, writes child manifests, spawns children (DeepSeek Chat) with narrowed capabilities, coordinates work, and produces output. Few-shot manifest examples in the system prompt guide reliable YAML generation. Used by `run-aaos.sh` in the Docker deployment path and by any install that has no role catalog.
- **Persistent Goal Queue** — Bootstrap runs as a persistent agent accepting goals via the Unix socket API. Container stays alive between tasks.
- **Workspace Isolation** — Each goal gets `/data/workspace/{name}/`. Children write intermediate files there. Output goes to `/output/`.
- **Stable Bootstrap Identity (opt-in)** — Normally every agent gets a fresh runtime-generated UUID at spawn. Bootstrap is the exception: its `AgentId` is resolved from `AAOS_BOOTSTRAP_ID` or `/var/lib/aaos/bootstrap_id` so episodic memory accumulates across container restarts. Only the Bootstrap path uses `AgentRegistry::spawn_with_id()`; regular `agent.spawn` RPC is unchanged. Reset via `AAOS_RESET_MEMORY=1`. The `AgentId::from_uuid()` constructor is kernel-only and flagged as such — a concession to persistence that slightly bends the "IDs are fresh kernel-generated process IDs" model. Long-term a separate *system memory identity* distinct from `AgentId` may be cleaner.
- **Cross-run learning (opt-in, minimal)** — When `AAOS_PERSISTENT_MEMORY=1`, the `run-aaos.sh` launcher bind-mounts the host's `./memory/` into `/var/lib/aaos/memory`, so the SQLite episodic store and the stable Bootstrap ID survive restarts. The Bootstrap manifest instructs the agent to `memory_query` before decomposing a goal and `memory_store` a compact run summary after completion. Children do not persist — they return findings to Bootstrap, which decides what to keep. Deliberately minimal: no new crate, no pattern schema, no reflection service — just existing primitives wired up. The plan is to observe 10-20 runs, then design a structured `PatternStore` only if recurring patterns justify one. Per-run behavior and lessons are logged in [`docs/reflection/`](reflection/README.md).
- **Safety Guardrails** — Agent count limit (100), spawn depth limit (5), parent⊆child capability enforcement, automatic retry of failed children.
- **Stable-identity gate on private memory** — `SpawnAgentTool` refuses any child manifest that declares `tool: memory_store`, and `AgentRegistry::spawn_with_tokens` defensively rejects the capability. `AgentProcess.persistent_identity` (runtime-owned, set only by `spawn_with_id`) marks agents with stable identity; only those may hold private memory. Introduced after run 6 observed Bootstrap granting children `memory_store` despite manifest prose forbidding it — "prompts persuade, only the kernel enforces."
- **Structured child-to-child handoff** — `spawn_agent` tool accepts an optional `prior_findings: string` field (≤ 32 KB). The `aaos-runtime::handoff` module wraps it with kernel-authored BEGIN/END delimiters, a timestamp, the parent agent name, and a prompt-injection warning. The parent LLM cannot remove the wrapping. Introduced after run 6 observed a `proposal-writer` confabulating when no structured channel existed for the prior `code-analyzer`'s output. Caveat: this is parent-provided continuity, not cryptographic provenance — a future handoff-handle design would verify findings against the audit log.
- **StdoutAuditLog** — Audit events streamed as JSON-lines to stdout for `docker logs -f` observability.
- **BroadcastAuditLog** — Fan-out wrapper over an inner `AuditLog`. Every recorded event goes to the inner sink AND to any subscribers (tokio `broadcast::channel`). The daemon's streaming JSON-RPC methods (`agent.submit_streaming`, `agent.logs_streaming`) subscribe and forward filtered events over the client's Unix socket as NDJSON frames.

#### Orchestration modes

`agentd submit` auto-detects whether to use `plan` or `persistent` mode based on the goal text. A cheap single-shot LLM call (via `LlmOrchestrationClassifier`, ~50 input / 1 output token) inspects the goal and picks the mode before any agent work begins. Pass `--orchestration [plan|persistent]` to override the auto-detected mode; the classifier is bypassed when the flag is present.

- **`plan`** — Planner + PlanExecutor DAG. Right for structured goals with declared outputs per subtask (fetch → analyse → write). Requires a loaded role catalog at startup; returns a legible error if the catalog failed to load rather than silently falling back. Each run is stateless: no agent survives beyond the goal.
- **`persistent`** — Bootstrap persistent agent. Right for open-ended, exploratory, or long-context goals where a single multi-turn agent manages its own context and spawns children as needed. Does not require a role catalog.

The routing gate lives in `server.rs:handle_submit_streaming`. After mode resolution, an `AuditEventKind::OrchestrationSelected { mode, source }` event is emitted (`source: "explicit"` or `"auto"`) so operators can always see which path was chosen and why. The classifier falls back to `plan` on any LLM error; when no LLM client is configured, a `NoopOrchestrationClassifier` picks `plan` immediately without a network call.

When to pick which: use `plan` for structured data pipelines with known output contracts; use `persistent` for investigation, reflection, or any goal where the agent must adapt its own strategy mid-run. For most goals, leaving the flag absent and trusting the classifier is the right default.

### 7. Human Supervision Layer

Read-only observation plus an operator surface for driving the daemon. Deliberately last — the system must be functional without it.

**Status:**
- `StdoutAuditLog` provides JSON-lines observability; `journalctl -u agentd` is the default operator query path once installed as a `.deb`.
- Verbose executor logging streams full agent thoughts, tool calls with arguments, and tool results.
- **Operator CLI** (`agentd submit|list|status|stop|logs`). Same binary as the daemon; subcommands connect to `/run/agentd/agentd.sock` over Unix-socket JSON-RPC. Operators join the `aaos` system group (created by the `.deb`'s `postinst`) to get socket access. `agentd submit` streams live audit events filtered to Bootstrap's goal tree; `agentd logs <id>` attaches to a single agent's stream. Ctrl-C detaches without killing the agent.
- Legacy tooling: `tools/dashboard.py` and `run-aaos.sh` still work for the Docker deployment path. Web dashboard remains future work.

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
- **Forgery threat model — split.** Four distinct threat classes with different current status; the blanket "not cryptographically unforgeable" shorthand is replaced by specific claims. (1) **In-process forgery by tool code — closed.** Tools see handles, not tokens; `CapabilityHandle`'s inner field is `aaos-core`-private so tool crates cannot fabricate one from a raw integer; `registry.permits` checks handle-to-agent ownership on every resolve. (2) **Worker-side forgery on NamespacedBackend — closed, transport exercised.** Workers receive no handles in the launch protocol and couldn't fabricate one locally. The broker↔worker socket is peer-creds-authenticated and, as of `7f7894d`, carries a persistent post-handshake stream with request/response correlation; `Ping` and `Poke` round-trip under sandbox on Debian 13 / kernel 6.12.43 and CI's Ubuntu 24.04 (see `tests/namespaced_backend.rs`). Routing actual tool invocations through the same stream instead of executing them host-side is the remaining runtime-side confinement work (tracked separately in `ideas.md`); forgery at this layer is already structurally impossible. (3) **Registry memory tampering by an attacker with Rust-level execution inside `agentd` — open.** In-process HMAC with an in-process key doesn't fix this; real defenses are OS-level (Landlock ptrace denial, seccomp on `agentd` itself) or hardware isolation (Phase G MicroVM, or HMAC with a key held in TPM2 / memfd_secret / an external signer subprocess). (4) **Cross-process / cross-host transport — N/A today, open when Phase G or multi-host lands.** No such transport exists yet; HMAC-signed `(agent_id, capability, constraints, issued_at)` with external key storage is the target fix when the signal fires. Full discussion and signals-to-reconsider in [`docs/ideas.md`](ideas.md#capability-token-forgery--threat-model-split).
- **Narrowable only** — Tokens can be constrained further, never escalated. Narrowing happens via `CapabilityRegistry::narrow(parent_handle, parent_agent, child_agent, additional)`, which atomically validates the parent's ownership, clones the token with the narrower constraints applied, and issues a fresh handle owned by the child.
- **Revocable** — Revocation flips `revoked_at` on the registry-held token; subsequent `authorize_and_record` calls return `CapabilityDenied::Revoked`. `AgentRegistry::revoke_capability()` and `revoke_all_capabilities()` delegate to the registry. `CapabilityRevoked` audit event schema unchanged.
- **Audited** — Every grant, denial, and revocation is logged. Durability depends on the configured audit backend.
- **Scope of enforcement** — Bundled tools (in `aaos-tools`) check via the registry at the call boundary. Third-party tool plugins must also route through the registry — the runtime hands them the handle, not the token, so direct inspection isn't possible without a registry reference. The registry's mutation API (`insert`, `narrow`, `revoke`, `remove_agent`) is marked `pub` with `RUNTIME-INTERNAL` rustdoc warnings for cross-crate accessibility from `aaos-runtime`; discipline is naming-convention-enforced rather than visibility-enforced because `pub(crate)` can't cross crate boundaries.
- **Kernel-level enforcement (namespaced backend only)** — When an agent runs under `NamespacedBackend`, the worker subprocess applies Landlock + seccomp (after `PR_SET_NO_NEW_PRIVS`) before entering the agent loop, and all tool invocations route through a peer-creds-authenticated Unix socket to the broker in `agentd`. The worker holds no `CapabilityHandle` values at all. This closes the in-process memory-attack threat class entirely for those agents. In-process backend agents continue to rely on handle opacity + registry discipline. Verified end-to-end on Debian 13 / kernel 6.12.43: live workers' `/proc/<pid>/status` reports `NoNewPrivs: 1`, `Seccomp: 2`, `Seccomp_filters: 2`; re-confirmed against commit `3e1b207` on 2026-04-17.

## Agent Backends

`AgentServices` is the agent-facing ABI; `AgentBackend` is the lower-level
"how do I actually run an agent's execution context" contract. Two backends
exist today, with a clean path to more:

- **`InProcessBackend`** (`crates/aaos-runtime/src/backend_in_process.rs`) —
  Today's default. Spawns a tokio task running `persistent_agent_loop` in
  the same process as `agentd`. Low overhead, trusts the process boundary.
- **`NamespacedBackend`** (`crates/aaos-backend-linux/src/lib.rs`) — Opt-in
  via `namespaced-agents` feature and `AAOS_DEFAULT_BACKEND=namespaced` env
  var. Handshake protocol, peer-creds session binding, Landlock + seccomp
  compilers, worker binary, and the `clone() + uid_map + pivot_root + exec`
  launch path are all implemented and verified end-to-end on Debian 13 /
  kernel 6.12.43: the 4 integration tests in
  `crates/aaos-backend-linux/tests/namespaced_backend.rs` pass under
  `--ignored`, and a live worker's `/proc/<pid>/status` shows
  `NoNewPrivs: 1`, `Seccomp: 2` (filter mode), and
  `Seccomp_filters: 2` (both stacked filters installed). Re-verified
  against commit `3e1b207` on 2026-04-17 — no regression since the
  2026-04-15 baseline. Still opt-in on the `.deb` install default until
  F-b ships the namespaced-by-default cloud image.

  **Scope of isolation today.** The namespaced backend isolates the agent
  worker's process — namespaces, Landlock, and seccomp apply to that worker.
  Production tool invocations for namespaced agents currently execute in the
  `agentd` process, not in the worker: the worker's broker protocol handles
  launch + `sandboxed-ready` handshake + `PokeOp`-style integration-test
  messages only. The broker↔worker tool-invocation stream (tracked in
  `docs/ideas.md`) is the piece that, when landed, will route every tool call
  through the peer-creds-authenticated socket so the worker actually drives
  the agent loop. Until then, `AAOS_DEFAULT_BACKEND=namespaced` buys you
  launch-side isolation primitives without yet buying runtime-side confinement
  of tool execution.

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

Events include trace IDs for request-level correlation and parent event IDs for causal tracing.

Computed-orchestration additions (2026-04-16):

- `PlanProduced { subtask_count, replans_used }` — emitted after the Planner returns the Plan that ultimately ran.
- `PlanReplanned { reason }` — emitted when the executor asks the Planner to revise on a correctable failure (unknown role, bad params, malformed plan).
- `SubtaskStarted { subtask_id, role }` — emitted as each DAG node spawns.
- `SubtaskCompleted { subtask_id, success }` — emitted when each DAG node exits.

26 event kinds total.
