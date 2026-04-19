# Roadmap

The prototype demonstrates that agent-first OS abstractions work: capability-based security, structured IPC, tool execution with two-level enforcement, agent orchestration with capability narrowing, and human-in-the-loop approval. Everything below builds on this foundation.

## Phase A: Runtime Prototype *(complete)*

The original agent runtime: 6 Rust crates (later grown to 7), capability-based security, tool registry with two-level enforcement (tool access + resource path), LLM execution loop, agent orchestration with capability narrowing, MCP message routing, human-in-the-loop approval queue. Landed as commit `029d90b` on 2026-03-21.

**What was built:** `aaos-core` (types, traits, `AgentServices`, `Tool`, capability model), `aaos-runtime` (process table, registry, LLM execution loop), `aaos-ipc` (MCP message router), `aaos-tools` (tool registry + built-in tools + capability-checked invocation), `aaos-llm` (Anthropic client + agent executor), `agentd` (daemon binary + Unix socket API). 3,917 production lines + tests, 111 passing, verified end-to-end against the real Anthropic API.

**What this enables:** Everything else. The capability system, `AgentServices` trait, `Tool` trait, and manifest format established in Phase A are the same interfaces all later phases build against — see [retrospective.md](retrospective.md#phase-a-48-hours) for the full chronicle and design trade-offs.

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

## Phase D: Self-Bootstrapping Agent VM *(complete)*

A Docker container where `agentd` is PID 1 and a Bootstrap Agent autonomously builds agent swarms to accomplish goals.

**What was built:** Bootstrap Agent manifest (Sonnet) with few-shot child manifest examples, persistent goal queue via Unix socket, workspace isolation per goal (`/data/workspace/{name}/`), spawn depth limit (5), global agent count limit (100), parent⊆child capability enforcement (already existed from Phase A), automatic retry of failed child agents, `StdoutAuditLog` for container observability.

**What this proves:** The OS vision works. A container boots, receives a goal ("fetch HN and summarize the top 5 stories"), and the Bootstrap Agent self-organizes: spawns a Fetcher agent with `web_fetch` capability, spawns a Writer agent with `file_write:/output/*`, coordinates their work, and produces a real output file. The capability system enforces isolation — the Bootstrap Agent correctly cannot read `/output/*` even though its child wrote there. Total time ~75 seconds, ~$0.03. The container stays alive accepting additional goals via the socket.

**What this enables:** Autonomous agent systems that self-organize for arbitrary goals. The OS manages agent lifecycle, capability enforcement, and observability. Humans provide goals, not instructions.

## Phase E: Multi-Provider LLM Support & Inference Scheduling *(complete)*

**E1: Multi-provider API support.** *(complete)* `OpenAiCompatibleClient` in `aaos-llm` speaks the OpenAI Chat Completions format — works with DeepSeek, OpenRouter, and any OpenAI-compatible provider. The daemon checks `DEEPSEEK_API_KEY` first, falls back to `ANTHROPIC_API_KEY`. Bootstrap uses `deepseek-reasoner` (thinking mode), children use `deepseek-chat`. 15 unit tests. Verified end-to-end: Bootstrap + 3 child agents designed Phase E autonomously for ~$0.02.

**What was built (E1):** `OpenAiCompatConfig::deepseek_from_env()`, request translation (system-as-first-message, tool_calls as function format, role:"tool" for results), response translation (choices[0].message, finish_reason mapping, prompt_tokens/completion_tokens), auth via `Authorization: Bearer`. Manifest model field routes to the correct provider.

**E2: Inference scheduling.** *(complete)* `ScheduledLlmClient` decorator wraps any `LlmClient` with a `tokio::sync::Semaphore` to limit concurrent API calls (default 3). Optional rate smoothing via configurable minimum delay between calls. Both bootstrap and normal daemon modes use the scheduler. 4 new tests.

**What was built (E2):** `ScheduledLlmClient`, `InferenceSchedulingConfig::from_env()`. Env vars: `AAOS_MAX_CONCURRENT_INFERENCE` (default 3), `AAOS_MIN_INFERENCE_DELAY_MS` (default 0).

**E3: Budget enforcement.** *(complete)* Per-agent token budgets declared in the manifest. `BudgetTracker` uses atomic CAS operations for lock-free tracking. Wired into `InProcessAgentServices::report_usage()` — agents exceeding their budget get `BudgetExceeded` errors. Optional — agents without `budget_config` have no enforcement. 5 new tests.

**What was built (E3):** `BudgetConfig` + `BudgetTracker` + `BudgetExceeded` in `aaos-core`, `budget_config: Option<BudgetConfig>` on `AgentManifest`, `budget_tracker: Option<Arc<BudgetTracker>>` on `AgentProcess`, `track_token_usage()` on `AgentRegistry`. The E3 design was produced by aaOS itself — Bootstrap spawned code-reader, budget-tracker-designer, and rust-implementer agents that read 24K tokens of real source code and produced the implementation. GPT-5.4 peer-reviewed the first design, we integrated with compile fixes.

**Also built:** `run-aaos.sh` launcher with auto-launching live dashboard. Verbose executor logging (full agent thoughts, tool calls, tool results). Source code mounted read-only at `/src/` so agents can read and understand the codebase.

**What this enables:** Cost-effective agent fleets using cheap API providers. A team of 20 agents where most use DeepSeek Chat ($0.27/M input) and a few use Claude for complex reasoning. Provider selection, scheduling, and budget enforcement as kernel concerns.

## AgentSkills Integration *(complete)*

aaOS now supports the [AgentSkills](https://agentskills.io) open standard by Anthropic. Skills are the universal way to give agents capabilities — used by Claude Code, Copilot CLI, Gemini CLI, Qwen CLI, OpenCode, Goose, and VS Code.

**What was built:** Skill loader (`aaos-core::skill`) parses SKILL.md files per the specification. `SkillRegistry` manages loaded skills. `skill_read` tool serves full instructions and reference files with path traversal protection. Skill catalog injected into agent system prompts at spawn time (progressive disclosure tier 1). 21 production-grade skills bundled from [addyosmani/agent-skills](https://github.com/addyosmani/agent-skills).

**What this enables:** Any AgentSkills-compatible skill works in aaOS — but under capability enforcement that no other runtime provides. The same skill that has open shell access in Claude Code runs under unforgeable capability tokens in aaOS. Skills become the "driver model" for agent capabilities; the runtime provides the security boundary.

## Self-Reflection Rounds *(ongoing)*

The runtime reads its own code, finds bugs, proposes features, and — as of 2026-04-17 — produces tested patches end to end. The reflection log under [`reflection/`](reflection/README.md) is the authoritative record; highlights:

- **Runs 1–3** — real bug fixes (path traversal, capability revocation, constraint enforcement).
- **Run 4** — feature proposal (Meta-Cognitive Coordination Layer) shipped as a minimal version after external review.
- **Runs 5–10** — memory protocol, kernel-level handoff gaps, adversarial bug-hunt finding seven bugs including a symlink-bypass of the run-1 traversal fix, four-agent chain producing a grounded error-handling proposal.
- **Phase F-a (2026-04-15)** — `agentd` as a Debian `.deb`, CLI, computed orchestration with a structured Planner + deterministic PlanExecutor, role catalog.
- **Phase F-a tuning (2026-04-16 / 17)** — Planner prompt fixes, role-budget wiring, enriched telemetry (args/result previews), replan-on-subtask-failure, NamespacedBackend re-verification, secret isolation (env scrub + 0600 conffile), gitleaks pre-commit + SECURITY.md.
- **First self-build run (2026-04-17)** — `cargo_run` + `builder` role let an agent read a plan, run `cargo check/test` against aaOS from inside aaOS, and correctly report "already implemented" with zero fabricated edits.
- **Tool-gap iteration (2026-04-17)** — runs 5–6 of the second self-build attempt failed to produce a diff — not from the model but because `file_read` returned whole files and there was no `file_edit` primitive. Diagnosis: self-build is tool-bound, not model-bound. Shipped `file_edit` + `file_read(offset, limit)` in commit `2819921`.
- **aaOS edits aaOS (2026-04-17)** — first end-to-end self-build success. 471 s wall clock. Nine `file_read(offset, limit)` calls paged through the 2700-line file; five `file_edit` calls applied all anchors on first try; `cargo check` + `cargo test` both passed. The agent's diff was byte-identical to the maintainer's manual fix. Same LLM as the failing runs; the only difference was the tools.
- **Junior-senior workflow (2026-04-17, runs 8–12)** — aaOS itself is now the author of new code. Senior (human) writes plans + reviews; junior (agent on an ephemeral droplet) applies the edits. Runs 8–10 shipped the `grep` navigation primitive end-to-end. Run 11 added the **tool-repeat guard** (hint injection at attempt ≥ 3 on same `(agent, tool, input_hash)`), plus a budget bump (`builder.retry.max_attempts` 30 → 60) and a plan-complete checklist in the role prompt. Run 12 shipped the `git_commit` tool — narrow `git add` + `git commit` under a `GitCommit { workspace }` capability with subcommand allowlist and flag-injection guards — completing the five-tool coding surface (`file_read(offset, limit)`, `file_edit`, `file_list`, `grep`, `git_commit` — `cargo_run` for build/test).

Cross-cutting lessons distilled from the runs (LLM calendar estimates aren't real, cost from token-math ≠ dashboard, skill adherence evolves, prompts persuade but only the kernel enforces, structured handoff beats opaque prompts, coding agents are tool-bound not model-bound) live in [`patterns.md`](patterns.md).

**What's deferred pending more data:** the structured `PatternStore`, new `aaos-reflection` crate, and `CoordinationPattern` schema are still not warranted. The minimal protocol (stable Bootstrap ID + opt-in persistent memory + query-before/store-after in the manifest) is the empirical foundation. If 10-20 runs surface recurring patterns worth indexing formally, the structured system gets designed against real data — not speculation.

## Phase F: Debian Derivative *(next)*

Full component sketch in [`distribution-architecture.md`](distribution-architecture.md). Short version below.

**Scope framing up front.** Phase F is a **Debian derivative**, not a from-scratch distribution. Upstream Debian 13 + our `.deb` preinstalled + opinionated systemd/config defaults, built via Packer, shipped as bootable ISO + cloud snapshots. We inherit Debian's kernel, apt repos, CVE response, and release engineering — we maintain only the aaOS-specific layers. Scope model: Home Assistant OS, Raspberry Pi OS, DietPi, Tailscale's prebuilt images. Not Fedora CoreOS, Bottlerocket, or Talos (those are full distributions built and released by teams of dozens). A solo maintainer can run a derivative. A solo maintainer cannot run a distribution.

**Why this shape, not a microkernel fork.** aaOS's differentiation is capability semantics, delegation, auditability, and policy compilation — not owning a kernel. A microkernel migration pushes the "it ships" date years out while losing the Linux ecosystem (GPU drivers, package management, every tool an agent might call through typed wrappers). A Debian derivative puts the capability model in real users' hands within quarters, not years.

Phase F splits into three explicit milestones: **F-a** ships the `.deb` (complete); **F-b** closes the Standard-spec Agent-Kernel gaps the rubber-duck design named (reasoning-slot scheduler, dynamic model routing, runtime-side tool confinement, per-task TTL/latency); **F-c** bakes the derivative image.

### Phase F-a: `agentd` as a Debian package *(complete)*

The `.deb` itself — installable on any Debian 13 host.

**Deliverable:** `apt install ./aaos_*.deb` on a fresh Debian 13 host brings up `agentd.service` and the system is ready to accept goals.

**What shipped.** Commits `5717906` (packaging scaffold) and `8d45691` (release-build fix — `CapabilityRegistry::inspect` was `cfg(debug_assertions)`-only and two production callers depended on it; replaced with `token_id_of`). Built via `cargo deb -p agentd` — no hand-maintained `debian/` tree; metadata lives in `[package.metadata.deb]` on the `agentd` crate.

**Package contents (verified on a Debian 13 VM).**
- `/usr/bin/agentd` — the daemon binary.
- `/usr/bin/aaos-agent-worker` — the namespaced worker binary (Phase F-a ships both; the feature flag decides whether it's used).
- `/etc/aaos/manifests/bootstrap.yaml` — default Bootstrap manifest, marked as a conffile so operator edits survive upgrades.
- `/lib/systemd/system/agentd.service` — the service unit.
- `/usr/share/doc/aaos/` — README + autogenerated copyright.

**Service user and layout.** `postinst` creates the `aaos` system user (nologin shell, home `/var/lib/aaos`, no home dir created). Systemd's `StateDirectory=aaos` and `RuntimeDirectory=agentd` own directory creation — `postinst` stays narrow. Socket lives at `/run/agentd/agentd.sock` (under `RuntimeDirectory=`). `postrm purge` removes the user and `/var/lib/aaos`; non-purge removal leaves state intact.

**Hardening in the unit.** `NoNewPrivileges`, `ProtectSystem=full`, `ProtectHome`, `PrivateTmp`, `ProtectKernelTunables`, `ProtectKernelModules`, `ProtectControlGroups`. Landlock/seccomp profiles come in F-b after the write-path audit that lets us tighten `ProtectSystem` to `strict`.

**Dependencies.** `$auto, systemd, ca-certificates` — nothing else. `curl`, `jq`, etc. are tool-wrapper concerns and belong in a separate `aaos-wrappers-core` package when wrappers land.

**What stays the same.** The `AgentServices` trait. The `Tool` trait. The manifest format. The runtime API methods. Packaging is a distribution concern; the programming model is the product.

**CI (not yet done).** The build still runs manually — `cargo deb -p agentd` on a Debian 13 host. A GitHub Actions workflow to build in `debian:13` on tag push is a follow-up when the first real release is cut; there's nothing to cut today.

**Operator CLI (complete).** Five subcommands (`submit`, `list`, `status`, `stop`, `logs`) + server-side NDJSON streaming (`agent.submit_streaming`, `agent.logs_streaming`) + `BroadcastAuditLog` + explicit `aaos` system group + `agentd(1)` man page. End-to-end verified on a fresh Debian 13 cloud VM as a non-root operator in the `aaos` group; two DeepSeek-backed goals ran successfully (5s and 3s respectively). Commits `58dd1bb` through `5e01acc` — eighteen incremental commits, subagent-driven implementation. The droplet verification caught a socket-permissions bug (`UnixListener::bind` inherits the process umask; needed explicit `chmod 0660` after bind) that the test suite missed because tests all run as root.

**Computed orchestration (complete).** Two-phase boot replacing Bootstrap-as-LLM-orchestrator. A cheap-LLM Planner (`deepseek-chat`, single-shot, structured JSON output) emits a typed `Plan { subtasks, depends_on, final_output }`. A deterministic `PlanExecutor` walks the resulting DAG in dependency-ordered batches, spawning each subtask via role-based scaffold (the `Role::render_manifest` + `render_message` path) and running independent subtasks concurrently via `futures::try_join_all`. 17 commits (`9b001cb` through `cbd3dc7`), 126 new runtime tests, subagent-driven with model-per-task-complexity. Role catalog lives at `/etc/aaos/roles/*.yaml`; four roles ship (fetcher, writer, analyzer, generalist). `/etc/aaos/roles/` is operator-extensible without rebuild — new roles load at daemon start. `agentd roles list|show|validate` subcommand inspects the catalog. End-to-end verified with a real DeepSeek submit of "fetch HN and lobste.rs, compare top 3, write to /data/compare.md" — planner produced the expected 5-subtask DAG with 2 parallel fetchers, 2 parallel analyzers, and the writer picked up the fan-in cleanly. Bootstrap path preserved as fallback when `/etc/aaos/roles/` is absent.

**Follow-up iterations (2026-04-17).** Four benchmark runs tightened the computed-orchestration path from a 5m30s baseline to **28s** on the canonical HN + lobste.rs compare goal:

- `dfb97f9` — Planner prompt rules (path shapes, operator-absolute paths preserved, anti-over-decomposition). Produces clean 4-subtask plans with parallel fetchers.
- `6b2387e` — `{inputs.*}` capability expansion: writer/analyzer roles declare `file_read: {inputs.*}`, and `render_manifest` now expands that into one real capability per array element (previously a literal string that never matched).
- `ef45e61` — role `budget` + `retry` fields now actually reach per-subtask `ExecutorConfig` via a new `SubtaskExecutorOverrides` passed through the `SubtaskRunner` signature. Root cause of the fetcher stall: `Role::render_manifest` dropped the budget silently and `execute_agent_for_subtask` used `ExecutorConfig::default()`.
- `c412a14` — tightened fetcher / analyzer / writer system prompts. Analyzer + writer now error loudly with `ERROR: missing input <path>` instead of fabricating from training data (silent-quality-failure mode closed).

**Known remaining bug**: the fetcher role, even with tight budgets and explicit step-by-step prompts, still sometimes emits a plausible `"written to <path>"` ack without actually calling `file_write`. The LLM satisfies the surface contract without performing the mechanical I/O. The honest fix is a deterministic fetcher scaffold — runtime-side `web_fetch` → `file_write` in Rust, no LLM loop for the mechanical role. Next iteration will ship that alongside an infrastructure bit that lets any role opt into scaffold execution via a `scaffold: true` marker. Analyzer + writer stay LLM-powered (genuinely LLM-shaped work); fetcher becomes the first bundled scaffold.

**`cargo_run` tool + `builder` role (2026-04-17).** A new `cargo_run` tool (commit `45ce06b`) executes `cargo {check,test,clippy,fmt}` in a capability-scoped workspace. Subcommand allowlist refuses anything that mutates state outside the workspace (no `install`, no `publish`, no custom subcommands); 4-minute wall-clock timeout; 8KB inline output cap. Paired with the `builder` role YAML, this is the minimum surface for aaOS to read a markdown implementation plan and apply it to a Rust workspace — verifying each change compiles and tests pass before moving on. The obvious first workload is aaOS applying plans to its own source tree on a throwaway host.

**Bidirectional MCP integration (2026-04-18).** New `aaos-mcp` crate, wired into `agentd` behind `--features mcp`. **Client:** for each entry in `/etc/aaos/mcp-servers.yaml` the runtime opens a stdio or HTTP session, runs the MCP `initialize` + `tools/list` handshake, and registers every remote tool into the existing `ToolRegistry` as `mcp.<server>.<tool>`. Remote tools invoke through the same capability-check/audit/narrow boundary as built-ins; no new `Capability` variants. Per-session reconnect loop with exponential backoff. **Server:** axum HTTP+SSE listener on `127.0.0.1:3781` (loopback only — no auth built in; operator's job to expose via SSH tunnel or Tailscale if needed). Exposes `submit_goal`, `get_agent_status`, `cancel_agent` as MCP tools so Claude Code, Cursor, or any other MCP client can delegate goals to aaOS. SSE stream at `GET /mcp/events?run_id=<id>` bridges audit events per run. Fifteen commits across 14 subagent-driven tasks, spec + quality review gated between each; integration tests plus an ignored stdio echo-server e2e. End-to-end verified on an ephemeral DigitalOcean droplet: `tools/call` for `submit_goal` spawns the bootstrap agent, routes the goal through the real DeepSeek LLM + tool path, and the capability system denies cross-trust writes as expected.

### Phase F-b: Standard-spec completion *(next)*

The rubber-duck design for an Agentic OS names a Standard tier with five Agent-Kernel primitives: a **Collaborative Framework** — task scheduler, semantic memory, standardized IAC, resource monitoring, abstracted filesystem. aaOS ships most of them today (see the audit in [architecture.md](architecture.md)). Phase F-b closes the four named gaps so a reader of "Agentic OS" finds the words map to shipped code, not to deferred entries in `ideas.md`.

Scope-bounded; each gap is tracked in `ideas.md` with a linked design note and is promoted here because the Standard spec names it explicitly, not because a specific buyer asked for it.

**Gap 1 — Reasoning-slot scheduler.** *Shipped 2026-04-18.* A runtime-owned `ReasoningScheduler` in `crates/aaos-runtime/src/scheduler/` awards LLM inference slots via a `BinaryHeap<Reverse<ReasoningRequest>>` priority queue keyed on the subtask's wall-clock deadline, with FIFO tiebreak via a monotonic insertion id. Slot pool size honors `AAOS_MAX_CONCURRENT_INFERENCE`. No-TTL requests get a 60-second synthetic deadline so they compete fairly against short-deadline peers. Slot granularity is one `complete()` call — no mid-inference preemption. Dispatcher survives dropped wakers (cancelled subtasks) by discarding the permit and looping. A `SchedulerView` wraps the LLM client **per subtask agent** so the AgentExecutor path is unchanged for subtask work. Every subtask's `complete()` call routes through the scheduler and records its elapsed time in a `LatencyTracker` (minimal `SubtaskWallClockTracker` impl today; Gap 2 adds per-model aggregation). **Scope note:** the Planner's own LLM call (DAG production) and the Bootstrap agent's LLM calls still go through the raw `llm_client` directly, not through a `SchedulerView`. Inference concurrency for those is bounded by the legacy `ScheduledLlmClient` semaphore, which wraps every outbound call at construction time. So `AAOS_MAX_CONCURRENT_INFERENCE` is still load-bearing — the new scheduler is an inner gate for subtask-agent traffic, not a wholesale replacement. Retiring `ScheduledLlmClient` requires threading a SchedulerView into the planner + bootstrap paths; deferred until a workload asks for per-plan scheduler policies.

**Gap 2 — Dynamic model routing.** *Shipped 2026-04-19.* Each `Role` declares an optional `model_ladder: Vec<String>` (defaults to `[role.model]`, keeping every pre-existing role back-compat) + `escalate_on: Vec<EscalationSignal>` (defaults to all three: `replan_retry`, `tool_repeat_guard`, `max_tokens`). `Subtask.current_model_tier: u8` tracks the ladder index; planner sets 0, executor increments on replan when a configured signal fired during the failed attempt. `SubtaskModelEscalated` + `ToolRepeatGuardFired` audit events fire on every bump and are operator-visible in the default `agentd submit` stream. A second `LatencyTracker` impl — `PerModelLatencyTracker` — collects per-model p50/p95 into 256-sample bounded rings; **v1 observability only**, no routing decisions consume it. **Scope note:** routing is purely signal-based in v1. No cost/price math, no classifier-based router, no cross-run persistent preference. A future sub-project can build cost-aware routing on top of `PerModelLatencyTracker` once there's real-world distribution data.

**Gap 3 — Runtime-side confinement of tool execution on `NamespacedBackend`.** *Shipped 2026-04-19; see `docs/reflection/2026-04-19-f-b3-e2e-qa.md` + `docs/reflection/2026-04-19-f-b3b-gap-fix.md` + `docs/reflection/2026-04-19-f-b3c-workspace-mount.md`. Final canonical-goal verification: 152s run, `/data/compare.md` = 6034 bytes, 5 `[worker]` + 4 `[daemon]` tags, zero tool failures, zero panics.* When `AAOS_DEFAULT_BACKEND=namespaced`, tool invocations for agents launched via `backend.launch` (i.e. the `spawn_agent` tool path) execute inside the worker under Landlock + seccomp. Daemon-side `ToolInvocation::invoke` routes via `route_for(tool_name, backend_kind)` → worker-side over the post-handshake broker stream with a `oneshot::Sender` demux for concurrency, or daemon-side as today. Worker-side whitelist: `file_read`, `file_write`, `file_edit`, `file_list`, `file_read_many`, `grep`. CLI shows `[worker]`/`[daemon]` tag per tool line. **Scope reality (named honestly after the e2e run):** plan-executor subtasks spawn inline via `execute_agent_for_subtask` and never go through `backend.launch`, so they have no broker session; `WorkerInvokeError::NoSession` falls back cleanly to daemon-side with honest audit tag, but in practice **worker confinement activates only for `spawn_agent`-launched children, not for plan-executor subtasks**. Additional known gap: tools encode their own capability re-check that fails inside the worker's minimal `InvocationContext` — Landlock-denial-based test passes end-to-end, but positive worker-side tool calls need either a `Tool::invoke_no_cap_check` variant or capability-token forwarding across the broker. Network tools (`web_fetch`) and subprocess tools (`cargo_run`, `git_commit`) stay daemon-side in v1 (seccomp allowlist has no `socket`/`connect`, kill-filter denies `execve`). LLM loop stays daemon-side (API keys out of sandbox). Follow-up sub-projects needed: (a) route plan-executor subtasks through `backend.launch` OR document the inline-path scope permanently, (b) fix tool-layer capability re-check in the worker, (c) confine network tools, (d) confine subprocess tools. Shipped across commits `0a47bb3` through `7a017f6`.

**Gap 4 — Explicit per-task TTL + latency as a first-class resource.** *Shipped 2026-04-18.* A `TaskTtl { max_hops: Option<u32>, max_wall_clock: Option<Duration> }` field lives on `Subtask`; the planner fills in `None` ttls from `AAOS_DEFAULT_TASK_TTL_HOPS` + `AAOS_DEFAULT_TASK_TTL_WALL_CLOCK_S` env defaults. `PlanExecutor::spawn_subtask` refuses launch when `max_hops == 0` and emits `SubtaskTtlExpired{reason:"hops_exhausted"}`; wall-clock expiry is enforced via a `tokio::select!` race in a `race_deadline` helper that cancels the runner future and emits `SubtaskTtlExpired{reason:"wall_clock_exceeded"}`. Returns `Correctable` so the plan executor's existing partial-failure logic cascades to dependents without new code. Latency tracking rides on the same `LatencyTracker` trait introduced for Gap 1; per-subtask wall-clock is queryable today, per-model aggregation arrives with Gap 2.

**What this phase does not do.** No new isolation tier (that's Phase G). No distributed runtime (deferred in ideas.md). No cryptographic identity (deferred in ideas.md). No Agent Market or Natural Language Dashboard (those are Advanced spec, not Standard). No new kernel-level security primitive — Landlock + seccomp + namespaces stay the backstop.

### Phase F-c: Debian-derivative reference image

A Packer pipeline that starts from an upstream Debian 13 base image, preinstalls the aaOS `.deb`, enables the service, and bakes opinionated defaults.

**Deliverable.** Bootable ISO + cloud snapshots (Debian publishes official images on AWS, DigitalOcean, Hetzner — our derivative ships on the same targets).

**Opinionated defaults baked into the image.**
- `AAOS_DEFAULT_BACKEND=namespaced` when the host kernel supports unprivileged user namespaces + Landlock (Linux 5.13+).
- `NamespacedBackend` Landlock-backed by default, seccomp stacked on top, cgroups v2 quotas per agent.
- Desktop meta-packages stripped (no X11, no Wayland, no LibreOffice — headless appliance).
- Custom motd pointing at the socket, the journal, and the docs URL.
- journald as the default audit sink.

**What the derivative does not do.** We do not maintain our own apt repos. We do not track CVEs. We do not maintain the kernel. We do not run a release-engineering cadence. Upstream Debian does all of that; the derivative pulls from `deb.debian.org` like every other Debian install. Our work is confined to the `.deb` (Phase F-a) and the Packer pipeline + default config (Phase F-c).

**Isolation layers used by the derivative.**
- **Namespaces** for per-agent isolation (mount, pid, net, user, cgroup).
- **Seccomp-BPF** as a damage-limiter. Syscall allowlists per agent derived from manifest capabilities.
- **Landlock** (Linux 5.13+) for filesystem capability enforcement at the kernel layer. Path-glob capabilities compile to Landlock rulesets.
- **cgroups v2** for CPU/memory/I/O quotas per agent — resource budgets become first-class.
- **Typed MCP wrappers for Linux tools.** `grep`, `jq`, `git`, `cargo`, `gcc`, `ffmpeg`, `pandoc` — each exposed as a tool with a declared capability. Full POSIX ecosystem for agents, every call capability-checked at the wrapper boundary.

Capability tokens stay the policy model; Linux primitives are the defense-in-depth backstop. `agentd` still runs as a systemd service (not PID 1 — that branding costs more edge-case burden than it's worth).

### Progress

Second `AgentBackend` implementation (`NamespacedBackend`)
landed across commits `a84cd98` + `a73e062` (scaffolding) + `1d6ec97` +
`67c7fc3` (kernel launch mechanics). Handshake protocol, Landlock +
seccomp compilation, broker session with peer-creds, fail-closed
missing-Landlock detection all working and unit-tested. The full
`clone() + uid_map + pivot_root + execve` path is implemented and
verified end-to-end on Debian 13 / kernel 6.12.43: spawned workers
show `Seccomp: 2` and `NoNewPrivs: 1` in `/proc/<pid>/status`.

Four integration tests in `tests/namespaced_backend.rs` pass under
`--ignored` on a capable host (Linux 5.13+ with unprivileged user
namespaces and the worker binary built):

- `launch_reaches_sandboxed_ready` — end-to-end spawn + confinement.
- `stop_is_idempotent` — second stop is a no-op.
- `health_detects_exit` — SIGKILL detection via real `waitpid` check.
- `worker_cannot_execve` — placeholder (broker-side `TryExecve` poke
  op not wired yet; scaffolded to launch + stop).

Phase F-a shipped 2026-04-15: `.deb` build reproducible via `cargo deb -p agentd`, installs cleanly on Debian 13, service starts, socket live at `/run/agentd/agentd.sock`, purge cleans state + user. `NamespacedBackend` available under the `namespaced-agents` feature but default stays `InProcessBackend` on the package install until there's CI coverage of the feature-on build on Debian 13.

Phase F-b v1 complete 2026-04-19. Three sub-projects: Sub-project 1 (Gaps 1 + 4 — reasoning-slot scheduler + per-task TTL/latency) shipped across commits `c2b56de` through `9b8e15a`; Sub-project 2 (Gap 2 — dynamic model routing) shipped across commits `cd55c8c` through `68c9112`; Sub-project 3 (Gap 3 — runtime-side tool confinement on `NamespacedBackend`) shipped v1 across commits `0a47bb3` through `7a017f6`. Sub-project 3's e2e QA exposed that plan-executor subtasks bypass the worker path (they spawn inline, not via `backend.launch`) and that tools' internal capability re-checks fail inside the worker's minimal `InvocationContext` — both documented as honest scope in architecture.md; sub-project 3b (closing the inline-subtask gap + worker-side capability handling) is the next Phase F-b task.

After Phase F-b closes, Phase F-c: Packer pipeline producing a Debian-derivative image with the `.deb` preinstalled, `namespaced` backend as default, desktop meta-packages stripped, opinionated motd/config. First cloud target + bootable ISO.

## Phase G: Isolation Ladder *(research branch)*

With two backend implementations already proving `AgentServices` is substrate-agnostic, Phase G adds a third: MicroVM-per-agent via Firecracker or Kata. The same agent manifest runs on different isolation levels depending on threat model:

- **Level 1 — Process** (current): Linux process with seccomp+Landlock. Low overhead, appropriate for trusted workloads.
- **Level 2 — MicroVM**: Firecracker / Kata / gVisor per agent (or per swarm). Hardware-virtualized isolation; what AWS Lambda and Fly.io use. Strong tenant isolation without writing a kernel.
- **Level 3 — Microkernel** (research): seL4 or Redox backend, only pursued if a specific market segment (high-assurance regulated deployments) demands formally verified isolation enough to fund it. Not prioritized; documented as a backend option on a clean ABI so the door stays open.

**Why this matters.** The `AgentServices` trait was originally pitched as "future syscall interface." Reframe: it's a **substrate-agnostic ABI**. An operator picks their isolation level based on threat model and resource budget, not on what kernel we happened to build.

**Prerequisites.** Phase F ships. Real workloads on hardened Linux prove the capability model. If tenant-isolation pressure emerges, MicroVM backend is the next layer. Microkernel only if formally-verified enforcement is the buyer's gating requirement.

## Known architectural gaps

The roadmap above describes what's *shipped* and what's *queued as a phase*. There's a third category: capabilities a reader of "Agentic Operating System" would reasonably expect that aaOS **has deliberately deferred** with named signals to reconsider. Naming them here, next to the ship log, so the gap between aspiration and delivery stays honest.

Each item links to the full "why deferred + signal to reconsider" entry in [`ideas.md`](ideas.md).

Three items from the original "Standard spec" rubber-duck have been promoted to the active Phase F-b scope above (reasoning-slot scheduler, dynamic model routing, runtime-side confinement of tool execution, per-task TTL + latency). One — runtime tool authoring via MCP — already shipped (Phase F-a follow-up, 2026-04-18). What remains below are the genuinely Advanced-tier items the rubber-duck frames as "ecosystem" concerns, deferred with named signals to reconsider.

- **[Distributed / multi-host agent runtime](ideas.md#distributed--multi-host-agent-runtime).** Every agent runs in a single `agentd` on a single host. Cross-host delegation, multi-tenant swarms, and the HMAC-signed-token transport that would require are all Phase-G-or-later.
- **[Cryptographic agent identity](ideas.md#cryptographic-agent-identity).** Commit trailers carry a prose "Co-Authored-By: aaOS builder role (ephemeral droplet, run X)" but there's no signature. Meaningful only once either multi-host transport lands or key storage moves out of `agentd`'s address space (TPM2 / HSM / enclave).

The pattern is intentional: we ship for the single-operator, single-node, trusted-`agentd` threat model that a Debian derivative actually serves, and defer the distributed / cryptographic / cost-optimizing layers until a specific workload or buyer demands them. Each deferral has a concrete signal-to-reconsider; promotion from ideas.md to roadmap happens when that signal fires, not on speculation.
