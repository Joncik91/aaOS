# Roadmap

The prototype demonstrates that agent-first OS abstractions work: capability-based security, structured IPC, tool execution with two-level enforcement, agent orchestration with capability narrowing, and human-in-the-loop approval. Everything below builds on that foundation.

The roadmap is organized in three sections:

- **Build history** — shipped work, ordered by landing date. Flat numbering (1…N); no nested alphanumerics.
- **Active milestones** — the next concrete deliverables. Numbered M1, M2, …
- **Research branch** — directions we expect to explore when a specific workload or buyer forces the question.

Plus two ongoing strands — **AgentSkills** and **Self-reflection runs** — that are continuous, not phased.

Where an old label (e.g. "Phase F-b/3" or "C2") appears in reflection logs, commit messages, or external notes, the `ex-<old label>` line under each heading below preserves the mapping.

For a release-by-release summary (what landed in `v0.0.1` through `v0.0.5` and the pre-tagged `0.0.0` body of work), see [`CHANGELOG.md`](../CHANGELOG.md).

---

## Build history

### 1. Runtime prototype
*ex-"Phase A" · complete 2026-03-21*

The original agent runtime: 6 Rust crates (later grown to 7), capability-based security, tool registry with two-level enforcement (tool access + resource path), LLM execution loop, agent orchestration with capability narrowing, MCP message routing, human-in-the-loop approval queue. Landed as commit `029d90b`.

**What was built:** `aaos-core` (types, traits, `AgentServices`, `Tool`, capability model), `aaos-runtime` (process table, registry, LLM execution loop), `aaos-ipc` (MCP message router), `aaos-tools` (tool registry + built-in tools + capability-checked invocation), `aaos-llm` (Anthropic client + agent executor), `agentd` (daemon binary + Unix socket API). 3,917 production lines + tests, 111 passing, verified end-to-end against the real Anthropic API.

**What this enables:** Everything else. The capability system, `AgentServices` trait, `Tool` trait, and manifest format established here are the same interfaces every later milestone builds against — see [retrospective.md](retrospective.md) for the full chronicle and design trade-offs.

### 2. Persistent agents + request-response IPC
*ex-"Phase B" · complete*

Persistent agents run continuously in a tokio background task, processing messages sequentially from a channel. Request-response IPC uses a `DashMap<Uuid, oneshot::Sender>` pending-response map on the router. Conversation history persists in JSONL files via a `SessionStore` trait, loaded once at startup and appended after each turn.

**What was built:** `persistent_agent_loop()`, `start_persistent_loop()` on registry, `send_and_wait()` on `AgentServices`, `SessionStore` trait + `JsonlSessionStore`, `run_with_history()` on `AgentExecutor` with transcript delta, `max_history_messages` config, Pause/Resume/Stop commands, 3 new audit events, `MailboxFull`/`Timeout` error variants. 141 tests (30 new), verified end-to-end with real Haiku 4.5 API.

**What this enables:** Agents that remember context across interactions. Multi-agent workflows where peers communicate directly via `send_and_wait`.

### 3. Managed context windows
*ex-"Phase C1" · complete*

The runtime manages what's in the agent's context window. When the conversation grows too long, `ContextManager` summarizes older messages via an LLM call and archives the originals to disk. The agent sees a coherent conversation; the runtime handles the compression transparently. `TokenBudget` estimates context size using a chars/4 heuristic, triggering summarization at a configurable threshold (default 70%). Summary messages are folded into the system prompt prefix, preserving User/Assistant turn alternation. Tool call/result pairs are kept atomic during summarization. Fallback to hard truncation on LLM failure.

**What was built:** `TokenBudget` type with `from_config()`, `ContextManager` with `prepare_context()`, `Message::Summary` variant, `ArchiveSegment` + archive methods on `SessionStore` trait, `LlmClient::max_context_tokens()`, `run_with_history_and_prompt()` on `AgentExecutor`, 2 new audit events. 25 new tests (166 total). Verified end-to-end with real Haiku 4.5 — summarization preserves facts across compression boundaries.

### 4. Episodic memory store
*ex-"Phase C2" · complete*

Per-agent persistent memory via explicit `memory_store`, `memory_query`, and `memory_delete` tools. Agents store facts, observations, decisions, and preferences; they query by meaning via cosine similarity over embeddings. In-memory store with brute-force search (SQLite+sqlite-vec planned for persistence). Embeddings via Ollama's nomic-embed-text model (768 dims, OpenAI-compatible `/v1/embeddings` endpoint).

**What was built:** New `aaos-memory` crate (7th workspace member) with `MemoryStore` trait, `InMemoryMemoryStore` (cosine similarity, agent isolation, LRU cap eviction, replaces/update semantics, dimension mismatch handling), `EmbeddingSource` trait with `MockEmbeddingSource` and `OllamaEmbeddingSource`. Three new tools in `aaos-tools`. `MemoryConfig` with episodic fields. 2 new audit events. 39 new tests (205 total). Verified end-to-end with real Haiku + Ollama nomic-embed-text.

**Deferred:** cross-agent shared knowledge graph (ex-"Phase C3"). Not buildable until the items above have production usage, a cross-agent capability model, and a proven multi-agent need. Tracked in `ideas.md`.

### 5. Self-bootstrapping agent swarm
*ex-"Phase D" · complete*

A Docker container where `agentd` is PID 1 and a Bootstrap Agent autonomously builds agent swarms to accomplish goals.

**What was built:** Bootstrap Agent manifest (Sonnet) with few-shot child manifest examples, persistent goal queue via Unix socket, workspace isolation per goal (`/data/workspace/{name}/`), spawn depth limit (5), global agent count limit (100), parent⊆child capability enforcement, automatic retry of failed child agents, `StdoutAuditLog` for container observability.

**What this proves:** a container boots, receives a goal ("fetch HN and summarize the top 5 stories"), and the Bootstrap Agent self-organizes: spawns a Fetcher agent with `web_fetch` capability, spawns a Writer agent with `file_write:/output/*`, coordinates their work, and produces a real output file. The capability system enforces isolation — Bootstrap correctly cannot read `/output/*` even though its child wrote there. ~75s, ~$0.03. The container stays alive accepting additional goals via the socket.

### 6. Multi-provider LLM support
*ex-"Phase E1" · complete*

`OpenAiCompatibleClient` in `aaos-llm` speaks the OpenAI Chat Completions format — works with DeepSeek, OpenRouter, and any OpenAI-compatible provider. The daemon checks `DEEPSEEK_API_KEY` first, falls back to `ANTHROPIC_API_KEY`. Bootstrap uses `deepseek-reasoner` (thinking mode), children use `deepseek-chat`. 15 unit tests. Verified end-to-end: Bootstrap + 3 child agents designed the subsequent milestones autonomously for ~$0.02.

**What was built:** `OpenAiCompatConfig::deepseek_from_env()`, request translation (system-as-first-message, tool_calls as function format, role:"tool" for results), response translation (choices[0].message, finish_reason mapping, prompt_tokens/completion_tokens), auth via `Authorization: Bearer`. Manifest model field routes to the correct provider.

### 7. Inference scheduler (semaphore-bounded)
*ex-"Phase E2" · complete*

`ScheduledLlmClient` decorator wraps any `LlmClient` with a `tokio::sync::Semaphore` to limit concurrent API calls (default 3). Optional rate smoothing via configurable minimum delay between calls. Both bootstrap and normal daemon modes use the scheduler. 4 new tests.

**What was built:** `ScheduledLlmClient`, `InferenceSchedulingConfig::from_env()`. Env vars: `AAOS_MAX_CONCURRENT_INFERENCE` (default 3), `AAOS_MIN_INFERENCE_DELAY_MS` (default 0).

### 8. Per-agent budget enforcement
*ex-"Phase E3" · complete*

Per-agent token budgets declared in the manifest. `BudgetTracker` uses atomic CAS operations for lock-free tracking. Wired into `InProcessAgentServices::report_usage()` — agents exceeding their budget get `BudgetExceeded` errors. Optional — agents without `budget_config` have no enforcement. 5 new tests.

**What was built:** `BudgetConfig` + `BudgetTracker` + `BudgetExceeded` in `aaos-core`, `budget_config: Option<BudgetConfig>` on `AgentManifest`, `budget_tracker: Option<Arc<BudgetTracker>>` on `AgentProcess`, `track_token_usage()` on `AgentRegistry`. The design was produced by aaOS itself — Bootstrap spawned code-reader, budget-tracker-designer, and rust-implementer agents that read 24K tokens of real source code and produced the implementation. GPT-5.4 peer-reviewed the first design, we integrated with compile fixes.

Also built around the same time: `run-aaos.sh` launcher with auto-launching live dashboard; verbose executor logging (full agent thoughts, tool calls, tool results); source code mounted read-only at `/src/` so agents can read and understand the codebase.

**What this enables:** Cost-effective agent fleets using cheap API providers. A team of 20 agents where most use DeepSeek Chat ($0.27/M input) and a few use Claude for complex reasoning. Provider selection, scheduling, and budget enforcement as kernel concerns.

### 9. Debian package (`.deb`)
*ex-"Phase F-a" · complete 2026-04-15 (CLI + computed orchestration follow-ups through 2026-04-18)*

The `.deb` itself — installable on any Debian 13 host.

**Deliverable:** `apt install ./aaos_*.deb` on a fresh Debian 13 host brings up `agentd.service` and the system is ready to accept goals.

**What shipped.** Commits `5717906` (packaging scaffold) and `8d45691` (release-build fix — `CapabilityRegistry::inspect` was `cfg(debug_assertions)`-only and two production callers depended on it; replaced with `token_id_of`). Built via `cargo deb -p agentd` — no hand-maintained `debian/` tree; metadata lives in `[package.metadata.deb]` on the `agentd` crate.

**Package contents (verified on a Debian 13 VM).**
- `/usr/bin/agentd` — the daemon binary.
- `/usr/bin/aaos-agent-worker` — the namespaced worker binary.
- `/etc/aaos/manifests/bootstrap.yaml` — default Bootstrap manifest, marked as a conffile so operator edits survive upgrades.
- `/etc/aaos/roles/*.yaml` — fetcher, writer, analyzer, generalist, all conffiles.
- `/lib/systemd/system/agentd.service` — the service unit.
- `/usr/share/doc/aaos/` — README + autogenerated copyright.
- `/usr/share/man/man1/agentd.1.gz` — man page.

**Service user and layout.** `postinst` creates the `aaos` system user (nologin shell, home `/var/lib/aaos`, no home dir created). Systemd's `StateDirectory=aaos` and `RuntimeDirectory=agentd` own directory creation. Socket lives at `/run/agentd/agentd.sock` (under `RuntimeDirectory=`). `postrm purge` removes the user and `/var/lib/aaos`; non-purge removal leaves state intact.

**Hardening in the unit.** `NoNewPrivileges`, `ProtectSystem=full`, `ProtectHome`, `PrivateTmp`, `ProtectKernelTunables`, `ProtectKernelModules`, `ProtectControlGroups`. Landlock/seccomp profiles arrive with #12 (runtime-side tool confinement).

**Operator CLI.** Five subcommands (`submit`, `list`, `status`, `stop`, `logs`) + server-side NDJSON streaming (`agent.submit_streaming`, `agent.logs_streaming`) + `BroadcastAuditLog` + explicit `aaos` system group + `agentd(1)` man page. End-to-end verified on a fresh Debian 13 cloud VM as a non-root operator in the `aaos` group. The droplet verification caught a socket-permissions bug (`UnixListener::bind` inherits the process umask; needed explicit `chmod 0660` after bind) that the test suite missed because tests all run as root.

**Computed orchestration.** Two-phase boot replacing Bootstrap-as-LLM-orchestrator. A cheap-LLM Planner (`deepseek-chat`, single-shot, structured JSON output) emits a typed `Plan { subtasks, depends_on, final_output }`. A deterministic `PlanExecutor` walks the DAG in dependency-ordered batches, spawning each subtask via role-based scaffold (the `Role::render_manifest` + `render_message` path) and running independent subtasks concurrently via `futures::try_join_all`. 17 commits (`9b001cb` through `cbd3dc7`), 126 new runtime tests. Role catalog lives at `/etc/aaos/roles/*.yaml`; four roles ship. `agentd roles list|show|validate` subcommand inspects the catalog. End-to-end verified with a real DeepSeek submit of "fetch HN and lobste.rs, compare top 3, write to /data/compare.md" — planner produced the expected 5-subtask DAG with 2 parallel fetchers, 2 parallel analyzers, and the writer picked up the fan-in cleanly. Bootstrap path preserved as fallback when `/etc/aaos/roles/` is absent.

**Follow-up iterations (2026-04-17)** tightened the computed-orchestration path from a 5m30s baseline to **28s** on the canonical HN + lobste.rs compare goal:
- `dfb97f9` — Planner prompt rules (path shapes, operator-absolute paths preserved, anti-over-decomposition).
- `6b2387e` — `{inputs.*}` capability expansion: writer/analyzer roles declare `file_read: {inputs.*}`, and `render_manifest` expands that into one real capability per array element.
- `ef45e61` — role `budget` + `retry` fields now actually reach per-subtask `ExecutorConfig` via a new `SubtaskExecutorOverrides` passed through the `SubtaskRunner` signature. Root cause of the fetcher stall: `Role::render_manifest` dropped the budget silently and `execute_agent_for_subtask` used `ExecutorConfig::default()`.
- `c412a14` — tightened fetcher / analyzer / writer system prompts. Analyzer + writer now error loudly with `ERROR: missing input <path>` instead of fabricating from training data.

**Deterministic scaffold roles (commit `2b8ed6d`).** Fetcher's LLM previously emitted plausible `"written to <path>"` acks without calling `file_write`. Fixed by adding `scaffold: {kind}` as an optional role field: when set, `PlanExecutor` dispatches to a `ScaffoldRunner` closure that runs the role in deterministic Rust instead of an LLM loop. `fetcher.yaml` ships with `scaffold: kind: fetcher`; the daemon-side `scaffold_fetcher` implementation does `web_fetch → file_write → return workspace path` with HTTP-status + empty-body rejection. Capability checks and audit events flow through the normal `tool_invocation` path.

**`cargo_run` tool + `builder` role (commit `45ce06b`).** Executes `cargo {check,test,clippy,fmt}` in a capability-scoped workspace. Subcommand allowlist refuses anything that mutates state outside the workspace; 4-minute wall-clock timeout; 8KB inline output cap.

**Bidirectional MCP integration (2026-04-18).** New `aaos-mcp` crate, wired into `agentd` behind `--features mcp`. **Client:** for each entry in `/etc/aaos/mcp-servers.yaml` the runtime opens a stdio or HTTP session, runs the MCP `initialize` + `tools/list` handshake, and registers every remote tool into the existing `ToolRegistry` as `mcp.<server>.<tool>`. Remote tools invoke through the same capability-check/audit/narrow boundary as built-ins. Per-session reconnect loop with exponential backoff. **Server:** axum HTTP+SSE listener on `127.0.0.1:3781` (loopback only). Exposes `submit_goal`, `get_agent_status`, `cancel_agent` as MCP tools so Claude Code, Cursor, or any other MCP client can delegate goals to aaOS. SSE stream at `GET /mcp/events?run_id=<id>` bridges audit events per run.

### 10. Reasoning-slot scheduler
*ex-"Phase F-b Gap 1" · complete 2026-04-18*

A runtime-owned `ReasoningScheduler` in `crates/aaos-runtime/src/scheduler/` awards LLM inference slots via a `BinaryHeap<Reverse<ReasoningRequest>>` priority queue keyed on the subtask's wall-clock deadline, with FIFO tiebreak via a monotonic insertion id. Slot pool size honors `AAOS_MAX_CONCURRENT_INFERENCE`. No-TTL requests get a 60-second synthetic deadline so they compete fairly against short-deadline peers. Slot granularity is one `complete()` call — no mid-inference preemption. Dispatcher survives dropped wakers (cancelled subtasks) by discarding the permit and looping. A `SchedulerView` wraps the LLM client **per subtask agent** so the AgentExecutor path is unchanged for subtask work. Every subtask's `complete()` call routes through the scheduler and records its elapsed time in a `LatencyTracker`.

**Scope note:** the Planner's own LLM call and the Bootstrap agent's LLM calls still go through the raw `llm_client` directly, not through a `SchedulerView`. Inference concurrency for those is bounded by the legacy `ScheduledLlmClient` (from #7), which wraps every outbound call at construction time. So `AAOS_MAX_CONCURRENT_INFERENCE` is still load-bearing — the new scheduler is an inner gate for subtask-agent traffic, not a wholesale replacement. Retiring `ScheduledLlmClient` requires threading a SchedulerView into the planner + bootstrap paths; deferred until a workload asks for per-plan scheduler policies.

### 11. Dynamic model routing
*ex-"Phase F-b Gap 2" · complete 2026-04-19*

Each `Role` declares an optional `model_ladder: Vec<String>` (defaults to `[role.model]`, keeping every pre-existing role back-compat) + `escalate_on: Vec<EscalationSignal>` (defaults to all three: `replan_retry`, `tool_repeat_guard`, `max_tokens`). `Subtask.current_model_tier: u8` tracks the ladder index; planner sets 0, executor increments on replan when a configured signal fired during the failed attempt. `SubtaskModelEscalated` + `ToolRepeatGuardFired` audit events fire on every bump and are operator-visible in the default `agentd submit` stream. A second `LatencyTracker` impl — `PerModelLatencyTracker` — collects per-model p50/p95 into 256-sample bounded rings; **v1 observability only**, no routing decisions consume it yet.

**Scope note:** routing is purely signal-based in v1. No cost/price math, no classifier-based router, no cross-run persistent preference. A future milestone can build cost-aware routing on top of `PerModelLatencyTracker` once there's real-world distribution data.

### 12. Runtime-side tool confinement
*ex-"Phase F-b Gap 3" · complete 2026-04-19*

When `AAOS_DEFAULT_BACKEND=namespaced`, every plan-executor subtask + every `spawn_agent`-launched child runs its filesystem + compute tools inside the worker under Landlock + seccomp. `ToolInvocation::invoke` routes via `route_for(tool_name, backend_kind)` → worker over the post-handshake broker stream (request/response correlation via `oneshot::Sender` demux), or daemon-side for tools that inherently need the daemon's authority. Capability tokens are forwarded with each `InvokeTool` so the worker's per-call `CapabilityRegistry` satisfies the tool's internal `permits()` check. Workspace paths + manifest-declared output roots are bind-mounted into the worker's mount namespace at the same absolute paths; Landlock permits each with a `PathBeneath` read-write rule. Worker-side whitelist: `file_read`, `file_write`, `file_edit`, `file_list`, `file_read_many`, `grep`. CLI shows `[worker]`/`[daemon]` tag per tool line.

**Permanently daemon-side** (design, not deferral): `web_fetch` (network), `cargo_run` + `git_commit` (subprocess execution), the LLM loop itself. Moving these to the worker would require broker-mediated network / subprocess proxies whose security line is *still the daemon* — the round-trip would be cosmetic. Scaffold roles (fetcher) run daemon-side too: they're the workspace plumbing, not a security boundary. Shipped across commits `0a47bb3` through `7adc147`. Reflections: `docs/reflection/2026-04-19-f-b3-e2e-qa.md` + `-f-b3b-gap-fix.md` + `-f-b3c-workspace-mount.md`. Final canonical-goal verification on a fresh droplet: 152s run, `/data/compare.md` = 6034 bytes, 5 `[worker]` + 4 `[daemon]` tags, zero tool failures.

### 13. Per-task TTL + latency as first-class resources
*ex-"Phase F-b Gap 4" · complete 2026-04-18*

A `TaskTtl { max_hops: Option<u32>, max_wall_clock: Option<Duration> }` field lives on `Subtask`; the planner fills in `None` ttls from `AAOS_DEFAULT_TASK_TTL_HOPS` + `AAOS_DEFAULT_TASK_TTL_WALL_CLOCK_S` env defaults. `PlanExecutor::spawn_subtask` refuses launch when `max_hops == 0` and emits `SubtaskTtlExpired{reason:"hops_exhausted"}`; wall-clock expiry is enforced via a `tokio::select!` race in a `race_deadline` helper that cancels the runner future and emits `SubtaskTtlExpired{reason:"wall_clock_exceeded"}`. Returns `Correctable` so the plan executor's existing partial-failure logic cascades to dependents without new code. Latency tracking rides on the same `LatencyTracker` trait introduced in #10; per-subtask wall-clock is queryable today, per-model aggregation arrived with #11.

### 14. Initial release (v0.0.1)
*complete 2026-04-19*

First tagged release with a CI-built `.deb` attached via `.github/workflows/release.yml`. Four CI edges closed alongside:
- `namespaced-agents` feature-on compile check in the fast job (`801c08d`).
- Clippy flipped to `-D warnings` after fixing 57 latent lints (`d1c4274`).
- Release workflow on `v*` tag push (`1ae9432` + `f61a967`).
- Crate versions bumped 0.0.0 → 0.0.1 (`779dd62`).

Release: https://github.com/Joncik91/aaOS/releases/tag/v0.0.1 — `aaos_0.0.1-1_amd64.deb`, 4.25 MB, built inside a `debian:13` container so cargo-deb encodes Debian's libc/systemd minimums.

### 15. Agentic-by-default `.deb`
*ex-"M1" · complete 2026-04-19*

The 2026-04-19 `.deb` audit found that the package installed green but a fresh operator still had to paste an LLM key, know about `--features mcp`, and discover `AAOS_SKILLS_DIR` before any agent could do useful work. Five `.deb`-level fixes closed that gap without touching the runtime:

- **MCP baked into the release build** (`a6c993b`). `packaging/build-deb.sh` now passes `--features mcp` by default (`AAOS_BUILD_FEATURES` env var overrides). Both the MCP client (external tools register as `mcp.<server>.<tool>`) and the loopback server (`127.0.0.1:3781`) are on by default. Claude Code, Cursor, any MCP client can delegate goals to aaOS without rebuilding. Binary size 4.25 MB → 4.69 MB.
- **`/etc/aaos/mcp-servers.yaml.example` template** (`54499de`). Shipped as a deb asset with commented-out GitHub MCP (HTTP), filesystem MCP (stdio via npx), and git MCP (stdio via uvx) entries. Both client and server subsystems stay off-by-default — operators copy-and-uncomment to opt in.
- **21 AgentSkills bundled under `/usr/share/aaos/skills/`** (`5c78a04`). FHS-correct vendor-supplied read-only data location. `discover_all_skills` now probes three paths in order: `/usr/share/aaos/skills/` (bundled), `/etc/aaos/skills/` (operator conffiles), `/var/lib/aaos/skills/` (runtime-installed); `AAOS_SKILLS_DIR` overrides append last.
- **Kernel-probe-driven backend default** (`9f18848`). `packaging/debian/postinst` now probes `/sys/kernel/security/lsm` for `landlock` + `/proc/sys/kernel/unprivileged_userns_clone` and generates `/etc/default/aaos.example` with `AAOS_DEFAULT_BACKEND=namespaced` + `AAOS_CONFINE_SUBTASKS=1` uncommented when both pass. Falls back to commented-out defaults with inline reason on older kernels.
- **`agentd configure` subcommand** (`4bb5e38`). Interactive first-boot setup: prompts for a DeepSeek or Anthropic API key, atomically writes `/etc/default/aaos` mode 0600 root:root (tempfile + fsync + rename — no window at looser mode), runs `systemctl daemon-reload && restart agentd`. Non-interactive mode via `--key-from-env VAR`. Daemon's missing-key startup log now points at the command instead of a dead-end "unavailable" message. 5 new tests (107 agentd-lib tests total).

**Deliverable met.** `apt install ./aaos_0.0.5-1_amd64.deb` followed by one `sudo agentd configure` produces a daemon that: (a) confines subtasks under Landlock + seccomp where the kernel supports it, (b) can register external MCP tools from the installed template, (c) exposes goals to external MCP clients on loopback, (d) has a skills catalog agents can actually query.

### 16. v0.0.2 release — droplet QA + six bug fixes
*complete 2026-04-19*

Fresh-droplet QA of the v0.0.1 `.deb` (see [`reflection/2026-04-19-v0.0.1-droplet-qa.md`](reflection/2026-04-19-v0.0.1-droplet-qa.md)) surfaced six bugs. v0.0.2 closes all six.

- **Bug 1 (critical)** — `.deb` shipped without `--features namespaced-agents`, so `AAOS_DEFAULT_BACKEND=namespaced` silently fell through to `InProcessBackend` and every tool call audit-tagged `[daemon]`. Fixed in `packaging/build-deb.sh` (`160861f`): default `AAOS_BUILD_FEATURES` now `mcp,namespaced-agents`. Confinement verified on droplet: writer agent's `file_read`/`file_write` audit-tag `[worker]` as designed; fetcher scaffold correctly stays `[daemon]`.
- **Bug 2** — zombie `aaos-agent-worker` processes after every run. `NamespacedBackend::stop` sent SIGTERM but never `waitpid`-reaped. Fixed: SIGTERM → 500ms WNOHANG polling → SIGKILL + blocking reap escalation.
- **Bug 3** — invalid API key produced `bootstrap failed (0k in / 0k out, 0s)` with zero log context. Fixed: `agentd submit` CLI now renders the `error` field from the streaming `end` frame; daemon emits a structured `tracing::error!` with `run_id` so `journalctl -u agentd` carries the long form.
- **Bug 4** — MCP subsystem completely silent (no startup log lines even with configured servers). Fixed: INFO lines for attempt/registered/listener-bind; no-config case now prints `"no /etc/aaos/mcp-servers.yaml — MCP disabled (copy .example to enable)"`.
- **Bug 5** — "using NamespacedBackend" printed twice per startup (both `Server::new()` and `Server::with_llm_client()` built a backend). Fixed: `OnceLock` guard.
- **Bug 6** — 11 lintian errors. Fixed via `packaging/debian/{copyright,changelog,lintian-overrides}`, merged-usr systemd path (`usr/lib/systemd/`), and explicit `strip` in `build-deb.sh`. **Final lintian: 0 errors, 0 warnings.** `.deb` shrinks 4.92 MB → 4.29 MB from stripping.

All six fixes verified on the same droplet after rebuild + reinstall. Tagged as `v0.0.2` (`d1cbe8c` + release workflow run 24638517499). Release: https://github.com/Joncik91/aaOS/releases/tag/v0.0.2 — `aaos_0.0.2-1_amd64.deb`, 4.29 MB.

**Extended verification pass (see [`reflection/2026-04-19-v0.0.2-droplet-qa.md`](reflection/2026-04-19-v0.0.2-droplet-qa.md))** — full-suite re-run with the tests the first sweep skipped: purge/install idempotency, non-root operator-group access, stop/start lifecycle, corrupt env, purge-while-running, `kill -9` mid-run recovery, OOM via cgroup cap, disk-full via tmpfs, skill shadowing, external MCP client registration via mock stdio server. All six original bugs remain closed. One new **Bug 7 (medium)** surfaced: `memory_{store,query,delete}` route `[worker]` but aren't in `WORKER_SIDE_TOOLS`, so they fail with `tool error: tool memory_X not available in worker` under confinement. Fixed in #17 below.

### 17. v0.0.3 release — Bug 7 patch
*complete 2026-04-24*

Patch-level release shipping the Bug 7 fix queued from the v0.0.2 extended QA pass.  No new features.  `memory_store`, `memory_query`, and `memory_delete` now live in `DAEMON_SIDE_TOOLS` in `aaos-core::tool_surface`, which is the right home for them: the memory store needs HTTP access to the embedding endpoint, and the worker sandbox can't provide it.  They join `web_fetch`, `cargo_run`, and `git_commit` as daemon-side tools — agents running confined still get the full memory surface, it just resolves across the broker stream instead of inside the worker process.  Commit `03d384f`.

Tagged as `v0.0.3`.  Release: https://github.com/Joncik91/aaOS/releases/tag/v0.0.3 — `aaos_0.0.3-1_amd64.deb`.

### 18. v0.0.4 release — Bug 8 patch, surfaced by self-reflection
*complete 2026-04-24*

Same-day patch.  The v0.0.3 self-reflection run (aaOS reading its own source tree on a fresh Debian 13 droplet under Landlock + seccomp) surfaced Bug 8 within 45 seconds of investigation: the `grep` tool routes `[worker]` in `aaos-backend-linux::worker_tools::WORKER_SIDE_TOOLS`, but grep shells out to `rg` (ripgrep) as a subprocess — and the worker's seccomp kill-filter denies `execve`.  Every grep call under the namespaced backend failed with `failed to spawn rg: Operation not permitted`.  The reflector role couldn't verify candidate findings with grep, returned empty, and the Planner fell back to a generalist that wrote a "NOT COMPLETED" marker without surfacing the tool failure — itself queued as Bug 9 under `CHANGELOG.md` `[Unreleased]`.

Fix is the same pattern as Bug 7: move `"grep"` from `WORKER_SIDE_TOOLS` to `DAEMON_SIDE_TOOLS`, drop the `GrepTool` registration from `build_worker_registry`, flip the routing tests.  Commit `aaf82a3`.  The structural defect (two routing lists that can drift) remains; a refactor to a single source of truth is tracked as a watchlist item in `docs/reflection/2026-04-24-v0.0.3-self-reflection.md`.

Tagged as `v0.0.4`.  Release: https://github.com/Joncik91/aaOS/releases/tag/v0.0.4 — `aaos_0.0.4-1_amd64.deb`.

### 19. v0.0.5 release — per-submit orchestration with auto-detect
*complete 2026-04-24*

Third same-day release.  Adds the first real fork in the execution surface since computed-orchestration shipped in #14 — `agentd submit` now picks between the Planner + PlanExecutor DAG path (structured goals with clear inputs / outputs, like fetch-analyse-write) and the Bootstrap persistent agent path (open-ended exploration, investigation, code-reading) on a per-submit basis.

The Bootstrap persistent path always existed in the source tree but was only reachable via docker `run-aaos.sh` or by deleting the role catalog — an all-or-nothing server-startup switch.  v0.0.5 exposes it per submit and, critically, makes the selection automatic by default.  A cheap single-shot LLM classifier (~50 input / 1 output token) inspects the goal text before any agent work begins and emits an `OrchestrationSelected { mode, source }` audit event so operators see which path was picked.  Operators who want to force a mode can still pass `--orchestration [plan|persistent]` — explicit overrides auto-detect.

Motivated by the v0.0.3 and v0.0.4 self-reflection runs, which showed the computed-orchestration path was architecturally wrong for bug-hunting-class goals: per-subtask LLMs are single-shot with a capped iteration budget, they exhaust the budget exploring, and the output-contract enforcement + fallback-generalist path produces hallucinated "completion" without real findings.  Bootstrap avoids that path by owning its own multi-turn context.  The Run 9 (2026-04-14) pattern of Bootstrap spawning child code-reader agents and synthesising is restored as a first-class routing target, now *composable* with the structured DAG path rather than replacing it.

Commits `1beaf22`, `a9bbfe2`, `976aa95`, `5dc20fd`, `4ddc959`, `e1c3d73` (+release-bump).  Test count 592 → 613.

Tagged as `v0.0.5`.  Release: https://github.com/Joncik91/aaOS/releases/tag/v0.0.5 — `aaos_0.0.5-1_amd64.deb`.

### 20. v0.1.0 release — unified PlanExecutor path, Bug 9 closed
*complete 2026-04-24*

Architectural minor version.  The root diagnosis from the v0.0.3–v0.0.5 runs: **single-shot subtasks were the bug, not the DAG**.  Bumping iteration limits doesn't fix a model that exhausts its budget exploring and never commits — that's an execution model mismatch, not a count problem.  v0.1.0 changes the inside of each DAG node from a single LLM call (budget: `retry.max_attempts + 10`, floor 10) to a full multi-turn agent loop (budget: `role.orchestration.max_iterations`, default 50).

**Three structural changes, in dependency order:**

1. **`SubtaskOutputStatus` enum + advisory audit event** (`aaos-core`).  `check_declared_outputs_exist` now returns `Present` / `MissingAdvisory` / `MissingFatal` instead of `Option<String>`.  Advisory path emits `AuditEventKind::SubtaskOutputMissing` and marks the subtask succeeded; fatal path (only when `role.require_declared_output: true`) propagates as a subtask failure.  Fetcher declares `require_declared_output: true` — a fetcher that didn't write its file is always a hard failure.

2. **Role YAML additions** (`aaos-runtime`).  `role.orchestration.max_iterations` (u32, default 50) replaces the old formula.  `role.require_declared_output` (bool, default false) controls the output-contract severity.  Bundled role budgets: fetcher 10, writer 30, analyzer 30, generalist 50, builder 50.  Both fields are optional and backward-compatible — existing YAMLs without them get the defaults.

3. **Unified PlanExecutor path + Bug 9 closed** (`agentd`).  Both orchestration modes now route through PlanExecutor:
   - `plan` / `decompose` → `PlanExecutor::run()` as before (Planner builds the multi-node DAG).
   - `persistent` / `direct` → new `PlanExecutor::run_with_plan()` called with a 1-node inline plan built by `inline_direct_plan()`.  No Planner LLM call, no Bootstrap agent.
   - `fallback_generalist_plan` deleted → Bug 9 closed.  A malformed Planner response propagates as `ExecutorError::Correctable`; the replan loop retries; after `max_replans` exhausted the run fails cleanly.  The hallucinated-report failure mode is structurally impossible.

**Classifier change.**  Prompt updated from "plan or persistent?" to "does this goal have independent parallelisable subtasks?".  Output changed from `plan`/`persistent` to `decompose`/`direct`.  Fallback on LLM error changed from `plan` to `direct` (direct is cheaper — skips the Planner call).  Wire API (`--orchestration plan|persistent`) preserved; operator commands written for v0.0.5 still work.

**Test delta:** 613 → 625 workspace-wide.  Bootstrap streaming integration test deleted (no longer a live path); replaced by new tests for `SubtaskOutputStatus` variants, `RoleOrchestration` defaults and round-trips, `run_with_plan` execution, and `decompose`/`direct` routing under all three source permutations (explicit plan, explicit persistent, auto-detect each direction).

Tagged as `v0.1.0`.  Release: https://github.com/Joncik91/aaOS/releases/tag/v0.1.0 — `aaos_0.1.0-1_amd64.deb`.

### 21. v0.1.1 release — 5-bug patch from self-reflection run
*complete 2026-04-25*

Patch release closing all 5 bugs surfaced by the first successful v0.1.0 self-reflection run and a parallel Sonnet audit on the same day.  No new features; no API or wire-protocol changes.  625 → 631 tests.

- **Bug 12 closed** — `glob_matches` separator-boundary check.  `/data/*` no longer matches `/data-foo/x`.  Fix: require the byte after the normalized prefix to be absent or `/`.
- **Bug 15 closed** — `pending_responses` RAII cleanup.  `send_and_wait` leaked a `DashMap` entry on every route error or timeout.  Fix: `MessageRouter::cancel_pending` + a `PendingGuard` that removes the entry on any early return.
- **Bug 16 closed** — `SqliteMemoryStore::store` atomic replace.  DELETE + INSERT were separate auto-commits; a failed INSERT permanently deleted the old record.  Fix: wrap both in `conn.transaction()` + `tx.commit()`.
- **Bug 10 closed** — `max_invocations` enforced at the `ToolInvocation` layer.  `permits()` was called (read-only) but `authorize_and_record()` was never called; capability constraints were dead code.  Fix: call `authorize_and_record` on the matching handle after a successful tool execution.
- **Bug 11 narrowed** — Revoked/expired tokens filtered before forwarding to workers.  `resolve_tokens` previously forwarded all tokens including revoked ones.  Fix: filter `is_revoked() || is_expired()` in `resolve_tokens`.  Residual race (token revoked after resolve but before worker invokes) requires Option A push-revocation protocol — queued for v0.2.x.

Tagged as `v0.1.1`.  Release: https://github.com/Joncik91/aaOS/releases/tag/v0.1.1 — `aaos_0.1.1-1_amd64.deb`.

### 22. v0.1.2 release — Bug 13 drain fix verified, Bug 17 surfaced and fixed
*complete 2026-04-25*

Same-day continuation.  Bug 13 (agent-stop race) had been queued from yesterday's v0.1.0 self-reflection run; diagnosis took one Sonnet sub-agent call that traced tokio cancellation propagating from `exec_task.abort()` through `.await` at `invoke_tool`, dropping the in-flight tool future and firing the scopeguard-stop before the side-effect ran.  Fix: 500 ms drain window via `tokio::time::timeout(&mut exec_task)` at all four `exec_task.abort()` sites in `crates/agentd/src/server.rs` (plan + direct branches, write-failure + RecvError::Closed cases) so pending tool invocations complete before cancellation.  Plus a `tracing::warn!` in `race_deadline` so TTL-triggered drops are visible in journald.

Verified end-to-end on the droplet: a v0.1.0-source bug-hunt that previously lost its findings.md to the abort cancel succeeded — a 10.9 KB self-reflection report with three new candidate bug findings (TOCTOU in `narrow`, `clone3` seccomp allowlist gap, `BudgetTracker::maybe_reset` race) landed on disk for the first time.  Commit `34b018e`.

The same verification run surfaced **Bug 17** — `inline_direct_plan` hardcoded `workspace: "{run}/output.md"` so the file landed at the workspace path instead of where the operator's goal text said ("/data/findings.md").  Same silent-misdelivery class as Bug 9 was, at the workspace-path layer.  Fix: omit the workspace param entirely from `inline_direct_plan`; the generalist's "if no workspace, follow the task description" fallback path then triggers and the LLM writes to whatever path the goal text named.  Commit `77bbe9d`.

Three candidate bug findings (Bugs 18/19/20) from the agent's report queued in CHANGELOG `[Unreleased]` for triage.  Full reflection: [`reflection/2026-04-25-v0.1.2-bug-13-and-17.md`](reflection/2026-04-25-v0.1.2-bug-13-and-17.md).

Tagged as `v0.1.2`.  Release: https://github.com/Joncik91/aaOS/releases/tag/v0.1.2 — `aaos_0.1.2-1_amd64.deb`.

### 23. v0.1.3 release — Bug 21 fix + triage of v0.1.2 candidates
*complete 2026-04-25*

Same-day patch closing Bug 21 (missing `CapabilityRevoked` audit events at agent shutdown — surfaced by aaOS reading its own source on v0.1.2).  `remove_agent` now routes through `revoke_all_capabilities()` instead of the raw registry call, so the audit event fires.  Also tightened `revoke_all_capabilities` itself by replacing a dead placeholder loop.  Commit `7d8db0f`.

Triaged Bugs 18/19/20 from the v0.1.2 reflection run via Sonnet sub-agent against current source: all three are theoretical or accepted-risk under the existing design.  Bug 18 (TOCTOU in `narrow`) is the same race class as Bug 11 — already tracked as v0.2.x Option-A protocol work.  Bug 19 (`clone3` seccomp) is contained by defense-in-depth (user-namespace + `PR_SET_NO_NEW_PRIVS` + `execve` kill-list); tightening filed to [`docs/ideas.md`](ideas.md) with concrete reconsider signals.  Bug 20 (`BudgetTracker` TOCTOU) — double reset is benign, no token loss possible due to CAS on `used_tokens`; closed as accepted-risk.

Tagged as `v0.1.3`.  Release: https://github.com/Joncik91/aaOS/releases/tag/v0.1.3 — `aaos_0.1.3-1_amd64.deb`.

### 24. v0.1.4 release — Bug 22 (workspace-GC symlink) + Bug 23 (approval timeout)
*complete 2026-04-25*

Same-day patch closing two real findings from the v0.1.2 self-reflection run that targeted v0.1.2 source.  The 79-second run produced three candidate findings; Sonnet triage identified two as real-and-shippable.

Bug 22 (high) — **Privilege escalation via workspace GC.**  An agent with a legitimate `FileWrite: /var/lib/aaos/workspace/*` capability could plant a symlink at the workspace root pointing to any `aaos`-uid-writable path; the next GC tick chased the symlink via `path.is_dir()` and `remove_dir_all`, deleting the target.  Fix: `DirEntry::file_type()` (does not follow symlinks) + explicit `is_symlink()` rejection.

Bug 23 (medium) — **Approval queue had no timeout.**  Agents blocked forever on operator absence; pending `DashMap` entries leaked across daemon lifetime.  Fix: `tokio::time::timeout(1h)` around the response oneshot; remove + deny on expiry.

The third candidate (TOCTOU in `CapabilityToken::permits`) was already closed by Bug 10's v0.1.1 fix (atomic check-and-record via DashMap exclusive shard-lock); no action needed.  Approval-queue persistence (separate sub-issue) is REAL-DESIGN — needs storage-layer decision before fixing.

This is the second iteration of the self-reflection-then-fix loop on v0.1.x: the system finds bugs, ships fixes, finds more bugs.  Bugs found by aaOS reading its own source this session: 10, 11, 12, 21, 22, 23.  Bugs from parallel Sonnet audit: 15, 16.  All eight closed within 24 hours of being found.

Tagged as `v0.1.4`.  Release: https://github.com/Joncik91/aaOS/releases/tag/v0.1.4 — `aaos_0.1.4-1_amd64.deb`.

### 25. v0.1.5 release — Bug 24 (broker_session doc) + Bug 25 (DashMap guard across await)
*complete 2026-04-25*

Round-3 self-reflection on v0.1.4 source.  Three candidate findings; two real-and-shippable, one deferred.

Bug 24 (low): `broker_session.rs` module doc made a false security claim that "seccomp denying `dup2`" was the post-`SO_PEERCRED` mitigation against fd-handoff attacks.  Wrong on two counts — `seccomp_compile.rs:99` allows `dup3` (tokio uses it for stdio) and `dup2` falls through to EPERM, not the SIGSYS the comment implied.  Corrected doc to reflect real mitigations (Landlock + user-ns + broker session-id correlation).  Runtime unchanged.

Bug 25 (low-medium): `registry.rs::stop()` held a DashMap shard guard across an `mpsc::send().await` — could stall any other task on the same shard under buffer pressure.  Fix: clone `command_tx` before the guard scope ends.  Standard async-Rust pattern.

Deferred: FileWriteTool TOCTOU between `create_dir_all` and `write`.  Real race window but constrained by Landlock + user-ns at the deployment layer; proper fix needs `openat`/`O_PATH` component-walk rewrite, not v0.1.x material.

Investigation: agent's claim that Bug 21's fix introduced a deadlock was **disproved** by source review.  `registry.rs:252` explicitly drops the entry guard before `remove_agent` runs; no re-entrant lock.

Tagged as `v0.1.5`.  Release: https://github.com/Joncik91/aaOS/releases/tag/v0.1.5 — `aaos_0.1.5-1_amd64.deb`.

### 26. v0.1.6 release — Bug 26 (charge capability before invoke)
*complete 2026-04-25*

Round-4 self-reflection on v0.1.5 source.  Three candidate findings; one shipped, two filed to `ideas.md` as deferred or future-architecture entries.

Bug 26 (medium): the Bug 10 v0.1.1 fix had charged the capability use AFTER tool invocation.  If the token expired or was revoked between `permits()` and the post-invoke `authorize_and_record`, the tool had already run with no count recorded — a free invocation past `max_invocations`.  Fix: charge BEFORE invoke; on failure the tool doesn't run; on success the count stays charged regardless of whether the tool then succeeds or errors.  Charge-on-attempt semantics, fail-closed.

Deferred: round-4 Finding 1 (`canonical_for_match` symlink-swap TOCTOU) is documented technical debt with a tracked entry; needs `O_NOFOLLOW`/proc-self-fd plumbing.  Round-4 Finding 2 (`McpMessage` sender spoofing) is theoretical under current architecture — becomes real when a wire protocol is added; filed as ideas.md entry with concrete reconsider signals.

Tagged as `v0.1.6`.  Release: https://github.com/Joncik91/aaOS/releases/tag/v0.1.6 — `aaos_0.1.6-1_amd64.deb`.

### 27. v0.1.7 release — Bug 27 (parent constraint inheritance on spawn)
*complete 2026-04-26*

Round-5 self-reflection on v0.1.6 source on a fresh droplet (the v0.0.5–v0.1.5 droplet had been destroyed; a re-launch attempt against the recycled IP was correctly rejected by ssh host-key verification before any payload landed).  Three findings reported; one new and real, two reproduced from earlier rounds and already filed in `docs/ideas.md`.

Bug 27 (high): both spawn paths in `spawn_tool.rs` issued child capability tokens with `Constraints::default()`, silently dropping parent `max_invocations` / rate_limit / expiry.  Phase A's run-1 finding #3 had originally fixed this; it regressed in a later refactor when the spawn paths needed capability substitution (parent's `file_read: /src/*` → child's `file_read: /src/crates/*`) that the existing `CapabilityToken::narrow()` couldn't provide.  Fix: new `narrow_with_capability()` method that substitutes the narrower capability AND inherits parent constraints atomically.  Both spawn paths refactored to use it.  Commit `c064531`.

Tagged as `v0.1.7`.  Release: https://github.com/Joncik91/aaOS/releases/tag/v0.1.7 — `aaos_0.1.7-1_amd64.deb`.

### 28. v0.2.0 release — cleared-queue release (push-revocation, approval persistence, TOCTOU fix)
*complete 2026-04-26*

Five rounds of v0.1.x self-reflection had exhausted the patch-level bug surface.  Four architectural items remained on the carryover queue, each correctly deferred during v0.1.x because they needed real design work, not patches: push-revocation, approval persistence, the `canonical_for_match` TOCTOU, and `clone3` seccomp tightening.  v0.2.0 was scoped as a cleared-queue release — ship the three that were buildable, surface the fourth as not-buildable with a written reason.

**Push-revocation protocol (Bugs 11 + 18).**  `CapabilityRegistry::revoke()` had been a single-shot mutation on the daemon-side table; tokens revoked between `resolve_tokens` and worker `permits()` were still honored.  Fix: `RevokeNotifier` trait in `aaos-core`, `SessionMapNotifier` impl in `aaos-backend-linux` that pushes a `Request::RevokeToken { token_id }` frame to every active worker session.  Worker-side: session-level `Arc<CapabilityRegistry>` initialized at session start, handles the frame by calling `registry.revoke(token_id)`.  `revoke_all_for_agent` also fires the notifier per token, so capability-wipe and lifecycle-exit paths no longer silently drop revocations.  Wired into agentd's three Server constructors via `wire_revocation_notifier`.  Commits `294024b`, `13d08c1`.

**Approval queue persistence.**  `ApprovalQueue` had been pure in-memory; daemon restart lost every pending entry.  Fix: `crates/agentd/src/approval_store.rs` is a SQLite store mirroring the in-memory shape.  `ApprovalQueue::with_store(store)` writes through on insert/respond/timeout.  `Server::build_approval_queue` reads `AAOS_APPROVAL_DB` (default `/var/lib/aaos/approvals.db`), purges entries past `DEFAULT_APPROVAL_TIMEOUT` at startup, and clears the rest because the agents that owned them are gone after restart.  A full reload-and-rearm path was deferred — needs persistent-agent-side cooperation that the current state machine doesn't expose.  Commit `860491c`.

**`canonical_for_match` TOCTOU.**  File tools previously did "canonicalize string → glob match → re-open by string for I/O".  An attacker with write access to any path component could swap a regular file for a symlink to a forbidden target between the two operations.  Fix: `aaos-tools::path_safe::safe_open_for_capability(path, mode)` opens with `O_NOFOLLOW | O_CLOEXEC`, resolves the resulting fd's `/proc/self/fd/<fd>` to a kernel-pinned canonical, and hands back both an `OwnedFd` and that canonical string.  `Token::permits_canonical_file` / `glob_matches_canonical` skip the second `fs::canonicalize`.  All six file tools migrated.  `file_write` and `file_edit` perform their I/O on the same fd that powered the capability check.  Commit `8b8f03b`.

**`clone3` seccomp tightening — not buildable.**  The Round-5-era plan was an argument-filter rule on `clone3` so only `CLONE_THREAD` flags would be permitted.  Discovered during implementation that this is structurally infeasible: `clone3(struct clone_args *args)` takes a pointer to userspace memory, and seccomp-BPF programs run before the syscall executes with access only to the syscall *register* values — not the memory they point to.  This is a deliberate kernel design choice (`Documentation/userspace-api/seccomp_filter.rst`), not a missing feature we can compose around.  Filed as STRUCTURALLY INFEASIBLE in `docs/ideas.md` with concrete reconsider signals: kernel exposes `clone3` flags to BPF, or worker confinement moves to a substrate that can intercept argument memory (eBPF LSM, microVM hypervisor).  Until then the namespace-creation defense is layered at the unprivileged-user-ns boundary, where it has always been load-bearing.

Tagged as `v0.2.0`.  Release: https://github.com/Joncik91/aaOS/releases/tag/v0.2.0 — `aaos_0.2.0-1_amd64.deb`.

### 29. v0.2.1 release — droplet-QA regressions in v0.2.0's TOCTOU path
*complete 2026-04-26*

Same-day patch.  v0.2.0's lib-test suite passed (135/135) and the release commit was clean, but droplet QA before tag-push exposed that the TOCTOU fix's `/proc/self/fd/<fd>` canonicalization path was broken inside the namespaced backend in three orthogonal ways.  Every worker-side `file_read` failed; without QA this would have shipped as a hard regression for any user with `AAOS_DEFAULT_BACKEND=namespaced` (the default on a 0.0.2+ install where the postinst probe detects Landlock + unprivileged userns).

The bugs:

- **Worker rootfs had no `/proc` mount.**  Pivot_root onto a tmpfs with bind-mounts for workspace/scratch/libs/socket/binary, but procfs was never mounted inside.  `readlinkat("/proc/self/fd/N")` returned ENOENT.  Added `mount("proc", "/proc", "proc", MS_NOSUID|MS_NODEV|MS_NOEXEC)` as Step E2 in the worker setup.  Commit `278aa52`.

- **`std::fs::read_link` calls bare `readlink`, not `readlinkat`.**  Worker seccomp permits `readlinkat` but not the older `readlink` syscall.  Rust's stdlib resolves `read_link` to the bare syscall on Linux x86_64 glibc, returning EPERM under seccomp.  Switched `path_safe::canonical_path_for_fd` to `nix::fcntl::readlinkat(None, …)` so the call goes through the syscall the worker is permitted to make.  Commit `8d63860`.

- **Landlock policy missing `/proc` read-only rule.**  Even with `/proc` mounted and `readlinkat` allowed, Landlock has to permit reading inside `/proc` for the readlinkat to succeed.  Added `PathBeneath(/proc, READ_ONLY)` in `landlock_compile.rs::build_ruleset`.  Commit `cd8bc28`.

Plus two cosmetic fixes (release-mode unused-imports warning for `CapabilitySnapshot`, duplicate `wire_revocation_notifier` install warning on every daemon restart).  Commits `c8737b0`, `8f29ab7`.

Verification on droplet a fresh DigitalOcean Debian 13 droplet: canonical fetch-HN goal completes in 12.6s with a real `/data/final-test.md` comparison file; symlink read attempt rejected with `O_NOFOLLOW (capability TOCTOU guard)`; approval-DB write-restart-clear cycle exercises the persistence path; `wire_revocation_notifier` fires cleanly with no warnings.

Tagged as `v0.2.1`.  Release: https://github.com/Joncik91/aaOS/releases/tag/v0.2.1 — `aaos_0.2.1-1_amd64.deb`.

### 30. v0.2.2 release — round 6 self-reflection on v0.2.1 (3/3 real findings)
*complete 2026-04-26*

Sixth iteration of the self-reflection-then-fix loop, first run against the v0.2.x line.  v0.2.1 source on the same DigitalOcean droplet (4 vCPU / 8 GiB) used for round 5; `AAOS_DEFAULT_BACKEND` left unset so the source-reading agent operated under the legacy InProcessBackend (matches all prior rounds — namespaced workers can't see `/src` since the worker rootfs is a tmpfs without bind-mounts for arbitrary host paths).

The v0.1.x → v0.2.x transition reopened design ground (push-revocation, approval persistence, the path_safe TOCTOU subsystem) so the reachable bug surface widened back out for the first time in months.  v0.1.x rounds had been picking through patch-level ground at a noise floor that climbed steadily; round 6 produced three real findings out of three — the highest hit rate since round 1.

The findings:

- **Bug 28 (high) — `web_fetch` redirect host bypass.**  `Policy::limited(5)` followed redirects with no per-hop validation against the agent's `NetworkAccess` grant.  An attacker on a permitted host could 302 to an attacker-controlled host; the response silently exfiltrated as the tool result.  Fixed by `Policy::none()` + manual redirect-following with `check_url_permitted` re-check on every hop.  Commit `eca9ddb`.

- **Bug 29 (medium) — `file_list` residual TOCTOU.**  v0.2.1 opened the path with `O_PATH | O_NOFOLLOW` for capability check, then dropped the fd and re-opened by canonical-path-string for the metadata + listing.  Race window between fd drop and second open let a directory-rename / symlink-swap subvert the listing.  Code's own comment had flagged this as deferred follow-up.  Fixed by performing the listing through the pinned fd: new `AccessMode::ReadDir` + `nix::dir::Dir::from_fd`.  Commit `6b24cf7`.

- **Bug 30 (high) — non-atomic session-store rewrite.**  `persistent_agent_loop` summarization called `session_store.clear()` then `append()` non-atomically.  A daemon crash or partial write between the two left an empty file on disk; in-memory history was intact but a daemon restart loaded the empty file and the agent's session history was permanently destroyed.  Code's own comment flagged this as deferred follow-up.  Fixed by adding `SessionStore::replace` as a trait primitive, with `JsonlSessionStore` overriding via write-temp + fsync + `rename(2)`.  Commit `4bdfb5b`.

**Pattern lifted.**  Two of the three findings came from inline `// NOTE: this is deferred follow-up` comments in v0.2.x code.  The reflection loop reads those comments and (correctly) calls them as bugs.  Convention going forward: deferred-by-design goes in `docs/ideas.md` with a reconsider signal and the code comment is DELETED; known-issues-pending-fix go in `CHANGELOG.md` with a forward-pointer.  In-code `// TODO: deferred` without an external paper trail is noise that the next round will turn into work.

Tagged as `v0.2.2`.  Release: https://github.com/Joncik91/aaOS/releases/tag/v0.2.2 — `aaos_0.2.2-1_amd64.deb`.

### 31. v0.2.3 release — round 7 self-reflection on v0.2.2 (3/3 real findings)
*complete 2026-04-26*

Seventh iteration of the loop, second on the v0.2.x line.  v0.2.2 source on the same droplet as round 6 with `AAOS_DEFAULT_BACKEND` unset (rounds-1-7 protocol).  Open-ended prompt with an explicit "say nothing if nothing" clause — three real findings, plus a fourth candidate that the agent reasoned through and explicitly removed mid-write (the discipline works).  No duplicates of earlier rounds.

The findings:

- **Bug 31 (medium) — `BudgetTracker` reset race.**  `maybe_reset()` did `load(last) → if too soon return → store(now) → if period elapsed reset()`.  Two threads near a period boundary could both pass the rate-limit gate, both store, and both call `reset()` — clobbering tokens already tracked in the new period.  Fixed via CAS-loop reset claim (commit `8c06449`).

- **Bug 32 (high) — intermediate-component symlink TOCTOU.**  `safe_open_for_capability` used `open()` with `O_NOFOLLOW`, which only protects the *leaf* component.  An attacker who could swap any intermediate directory for a symlink would steer the open to a forbidden tree even though the leaf was a non-symlink filename.  v0.2.2's code disclaimed this in a comment — the third "deferred follow-up" to become a v0.N+1 finding.  Fixed by routing through `openat2(RESOLVE_NO_SYMLINKS)` which rejects symlinks at every component.  Available since Linux 5.6; falls back to `open()` on older kernels.  Worker seccomp gains `SYS_openat2` (commit `67e7d24`).

- **Bug 33 (medium) — `InMemoryAuditLog::with_cap(0)` deadlock.**  `debug_assert!(max >= 1)` compiled out in release.  With `max == 0`, `record()`'s `while events.len() >= max` loop was always true and `pop_front()` on an empty `VecDeque` was a silent no-op — infinite spin while holding the audit mutex.  Fixed via always-on `assert!` (commit `627846e`).

**Pattern reinforced.**  Three rounds in a row, three "deferred follow-up" code comments turned into v0.N+1 findings.  v0.2.3 deletes the last surviving such comment in `file_write.rs` — Bug 32's `openat2(RESOLVE_NO_SYMLINKS)` fix subsumes the disclaimed limitation.  The loop reads code comments and turns "this is deferred" into work; the contract is now: deferred-by-design goes in `docs/ideas.md` with a reconsider signal, known-issues-pending-fix go in `CHANGELOG.md`'s known-issues block, in-code TODOs are noise that the next round will turn into a finding.

Tagged as `v0.2.3`.  Release: https://github.com/Joncik91/aaOS/releases/tag/v0.2.3 — `aaos_0.2.3-1_amd64.deb`.

### 32. v0.2.4 release — round 8 self-reflection on v0.2.3 (1/3 fixed, 2/3 deferred)
*complete 2026-04-26*

Eighth iteration of the loop, third on the v0.2.x line.  Same droplet, `AAOS_DEFAULT_BACKEND` unset for the source-reading agent (rounds-1-7 protocol).  **First round where some findings genuinely deferred rather than fixed.**  Rounds 6 and 7 each produced 3 fixable findings; round 8 produced 1 fixable + 2 filed to `docs/ideas.md` with concrete reconsider signals.  Wall clock 97 s — quickest v0.2.x round.

The fixed bug:

- **Bug 34 (medium) — seccomp socket allowlist over-permissive + lying docs.**  `SYS_socket` and `SYS_socketpair` were allowed unconditionally; server-side primitives (`bind`/`listen`/`accept`/`accept4`) were also in the allowlist even though the worker is a Unix-socket client only.  Two downstream doc comments claimed "no socket/connect syscalls" — factually wrong.  Argument-filter `SYS_socket`/`SYS_socketpair` to `AF_UNIX` only via `SeccompCondition`, drop the server primitives, rewrite both doc comments to be honest about what's allowed (commit `<TBD>`).

The deferred findings, both filed in `docs/ideas.md`:

- **Token-generation counter to close the `resolve_tokens` wire race.**  v0.2.0's push-revocation protocol closes the *post-dispatch* race for the worker's session-level registry but does NOT close the *wire-race* window where an `InvokeTool` and a `RevokeToken` cross on the broker stream.  The fix (sequence/generation counter on every token, verified at result-return) is heavyweight: touches wire format, broker schema, audit shape, adds latency to every tool call.  Race is microsecond-scale and requires the attacker to be already inside the daemon.  Cost-vs-value ratio poor at v0.2.x size; reconsider when (a) multi-operator daemon needs sub-call revocation latency, OR (b) broker protocol gains synchronous result-ack for other reasons.

- **Replace hand-rolled `SchemaValidator` with the `jsonschema` crate.**  Validator is shallow — accepts `{"path": 42}` against a `"type": "string"` schema.  Tools defend their own inputs today; validator is a developer ergonomic, not a trust boundary.  Reconsider when externally-authored manifests need pre-tool-body schema enforcement, or MCP integration honours remote schemas locally.

**Pattern reinforced.**  v0.2.4's `resolve_tokens` doc rewrite explicitly cites `docs/ideas.md` instead of "queued for v0.N+1" — comments that defer must point at an external paper trail, never as in-code TODOs.  Round 8 deletes the last surviving such comment.  Future rounds: any inline `// queued for ...` without a corresponding `ideas.md` entry is a finding waiting to happen.

If round 9 on v0.2.4 source produces 0 real fixable findings, the v0.2.x patch surface is depleted and the pivot to M1 (Debian-derivative reference image) becomes the right move.

Tagged as `v0.2.4`.  Release: https://github.com/Joncik91/aaOS/releases/tag/v0.2.4 — `aaos_0.2.4-1_amd64.deb`.

### 33. v0.2.5 release — concurrency stress probe finds Bug 35
*complete 2026-04-26*

After round 9 (0 source-reading findings) and the fuzz pass (0 panics across 137M inputs) both depleted on v0.2.4, the next probe of different kind was concurrency stress.  A bash + socat + jq harness on the droplet opens 32 concurrent connections to the daemon and hammers `agent.spawn` + `agent.stop` cycles (16k per pass, no LLM round-trip — exercises the registry / capability / session-map shard locks, not the executor).

First serious run found **Bug 35**: `InMemoryAuditLog::new()` is unbounded by design (its own doc-comment says so for test harnesses), and both Server constructors called the unbounded `::new()` directly.  Each spawn-stop cycle emits ~10 audit events × ~120 bytes = +1.2 KB/cycle.  Three consecutive 16k passes on the same daemon: RSS +20MB → +18MB → +20MB.  Linear leak.  Fixed by switching to `InMemoryAuditLog::with_cap(50_000)` (override via `AAOS_AUDIT_LOG_CAP`).  Verified bounded on v0.2.5.

Pattern lifted: a primitive's `::new()` default that is *unsafe for production* (unbounded, blocking-on-default-timeout, no-rate-limit) becomes a production bug if callers use it directly.  The bug-shape that source-reading misses: a code comment honestly documenting "this is unbounded" reads like documentation, not a warning, when callers `use ::new()`.  Future v0.2.x convention: such constructors should be `#[cfg(test)]`-gated, OR the doc-comment must explicitly direct production callers to the bounded variant.  Queued as v0.2.6+ work to tighten `::new()` itself.

The v0.2.x line has now closed bugs across three independent probes:
- Source-reading reflection (rounds 6–8): Bugs 27–34
- Fuzzing randomized inputs (137M): 0 findings, surface confirmed robust
- Concurrency stress: Bug 35

Each probe finds bugs the others can't.

Tagged as `v0.2.5`.  Release: https://github.com/Joncik91/aaOS/releases/tag/v0.2.5 — `aaos_0.2.5-1_amd64.deb`.

### 34. v0.2.6 release — namespaced-backend stress probe finds Bugs 36 + 37
*complete 2026-04-26*

After v0.2.5 closed Bug 35 (audit log unbounded under churn — InProcess stress probe), the natural next probe was the same harness with `AAOS_DEFAULT_BACKEND=namespaced` to exercise the broker `SessionMap` + worker fork/exec/landlock/seccomp setup.  Two real bugs surfaced:

- **Bug 36 (high) — `mount("proc", ..., "proc", ...)` fails inside unprivileged user namespace.**  v0.2.1's procfs-mount fix (Step E2) needs CLONE_NEWPID, which the worker deliberately doesn't unshare.  EPERM on every `agent.spawn` with `lifecycle:persistent` under namespaced.  Fixed by bind-mounting host `/proc` instead of mounting fresh procfs.

- **Bug 37 (high) — `agent.stop` leaked the worker subprocess.**  `AgentRegistry::stop` ended the in-daemon persistent loop but never told the namespaced backend to terminate the worker.  `backend.stop()` was only called from tests.  20 spawns → 20 leaked workers.  Fixed by adding `AgentBackend::stop_by_agent_id` (default no-op for backends without subprocesses; `NamespacedBackend` overrides to SIGTERM+waitpid the worker by id).  `Server::handle_agent_stop` calls it after `registry.stop` succeeds.

Both bugs had been latent since v0.2.1 / v0.0.x respectively — every droplet QA since v0.0.2 had silently skipped them because the canonical fetch-HN goal goes through inline plan-executor subtasks (no `backend.launch`), never through the `agent.spawn`+namespaced+persistent path that production operators using JSON-RPC directly hit.

**Pattern lifted.**  "Test the path the canonical goal actually uses" (the v0.1.x convention) misses bugs in surfaces the canonical doesn't touch.  Stress probes that exercise every JSON-RPC method catch this class.  Wiring `stress-droplet.sh` (under both InProcess and namespaced) into the release checklist alongside the canonical fetch-HN run would prevent the regression class going forward.

The v0.2.x line has now closed bugs across four independent probe types:
- Source-reading reflection (rounds 6–8): Bugs 27–34
- Fuzzing randomized inputs (137M): 0 findings
- Stress InProcess: Bug 35
- Stress namespaced: **Bugs 36, 37**

Tagged as `v0.2.6`.  Release: https://github.com/Joncik91/aaOS/releases/tag/v0.2.6 — `aaos_0.2.6-1_amd64.deb`.

### 35. v0.2.7 release — round 10 deep self-reflection finds Bugs 38 + 40
*complete 2026-04-26*

Round 9 had declared the v0.2.x source-reading surface depleted (0/3 real findings on v0.2.4 with the standard prompt).  The intervening stress probes (Bugs 35, 36, 37) proved that was a depletion-of-the-prompt-shape, not a depletion-of-the-runtime.  Round 10 used a deeper-investigation prompt that explicitly steered toward the missed-pattern shapes:
- Trait methods with only test callers
- Doc-warned defaults that production callers use anyway
- Lifecycle events that don't reach all subsystems

3 findings reported in 143 s; 2 real and shipped, 1 deferred:

- **Bug 38 (medium) — `SessionStore::clear` had no production caller.**  Every persistent agent's session-store entry leaked on stop (in-memory store accumulated DashMap entries unbounded across spawn-stop cycles).  Fixed by wiring `clear` into the persistent loop's exit handler, gated on `!persistent_identity` so Bootstrap-shaped agents preserve history across stop+respawn.  Same shape as Bug 35 (audit log unbounded by default).

- **Bug 40 (high) — `agent.spawn_and_run` + `lifecycle: persistent` leaks.**  The handler called `agent.spawn` (which starts a persistent loop) then ran a one-shot `execute_agent` on the same agent_id.  After the one-shot returned, the persistent loop never received a stop signal — leaked one tokio task (InProcess) or one worker subprocess (namespaced) per call.  Fixed by rejecting persistent manifests in spawn_and_run with a clear error directing the caller to use `agent.spawn` + `agent.run` instead.

- **Bug 39 (deferred) — `JsonlSessionStore` directory scans O(N).**  Real structural inefficiency; unreachable in production since InMemorySessionStore is the default.  Filed in `docs/ideas.md` with reconsider signals.

**Pattern lifted.**  "No findings" from a self-reflection pass is a depletion signal *for the specific prompt shape used*, not for the runtime.  Round 10's prompt structure (explicit shape-steering plus exploit-POC requirement) is now the recommended one for v0.2.x reflection rounds.

The v0.2.x line has now closed bugs across four independent probe types:
- Source-reading reflection rounds 6–8 (standard prompt): Bugs 27–34
- Source-reading reflection round 10 (shape-steering prompt): **Bugs 38, 40**
- Fuzzing randomized inputs (137M): 0 findings
- Stress InProcess: Bug 35
- Stress namespaced: Bugs 36, 37

Tagged as `v0.2.7`.  Release: https://github.com/Joncik91/aaOS/releases/tag/v0.2.7 — `aaos_0.2.7-1_amd64.deb`.

### 36. v0.2.8 release — round 11 silent-failure prompt finds Bugs 41/42/43
*complete 2026-04-26*

Third distinct prompt-shape steer in three rounds:
- Round 9 (standard prompt, v0.2.4): 0 real findings → "depleted"
- Round 10 (trait-with-only-test-callers shape, v0.2.6): 2 real (Bugs 38, 40)
- Round 11 (silent-failure-discards shape, v0.2.7): **3 real (Bugs 41, 42, 43)**

Same source code being read each round (the rounds differ only by the bugfixes applied between).  Round 11 prompt: "find every `let _ = result_returning_call()` discard, classify each, identify the ones hiding real bugs."  Agent enumerated all 47 such sites, classified 44 as defensibly discarded (with reasons), flagged 3 as real bugs:

- **Bug 41 (high) — `archive_segment` failure permanently destroys conversation history.**  `persistent_agent_loop` summarization path discarded archive errors then unconditionally drained the archived messages from history.  If the archive write failed, the original messages existed nowhere.  Fixed by skipping the summarization cycle on archive failure (preserve original history for retry).

- **Bug 42 (high) — `append` failure silently truncates session history.**  Same shape: per-turn append errors discarded; daemon restart loaded a shorter history than the operator expected.  Fixed by adding an audit event + throttled error log.

- **Bug 43 (medium) — silent SQLite orphan rows in approval store.**  Three sites in `ApprovalQueue::request` plus the daemon's startup purge did `let _ = store.remove(id);`.  Under read-only-fs or SQLite-locked conditions the row stayed; the table grew unbounded across daemon lifetimes.  Fixed by logging at warn level.

**Pattern confirmed.**  "No findings" from a self-reflection pass is a depletion signal *for the specific prompt shape used*, not for the runtime.  Three distinct shapes have surfaced 5 real bugs in three consecutive rounds (38, 40, 41, 42, 43).  Round-12+ candidate prompt shapes are listed in the reflection log; the loop has more reach than any single prompt finds.

The agent's classification of all 47 `let _ = ` discards is preserved in the round-11 report as a future-maintenance reference — when someone modifies one of those sites and wonders if the discard is safe, the table answers it.

The v0.2.x line has now closed bugs across five independent probe types:
- Source-reading rounds 6–8 (standard prompt): Bugs 27–34
- Source-reading round 10 (trait-method shape): Bugs 38, 40
- Source-reading round 11 (silent-failure shape): **Bugs 41, 42, 43**
- Fuzzing randomized inputs (137M): 0 findings
- Stress (InProcess + namespaced): Bugs 35, 36, 37

Tagged as `v0.2.8`.  Release: https://github.com/Joncik91/aaOS/releases/tag/v0.2.8 — `aaos_0.2.8-1_amd64.deb`.

---

## Active milestones

### M1 — Debian-derivative reference image
*next (was M2 before #15 shipped)*

A Packer pipeline that starts from an upstream Debian 13 base image, preinstalls the v0.1.0 `.deb`, enables the service, and bakes opinionated defaults.

**Deliverable.** Bootable ISO + cloud snapshots (Debian publishes official images on AWS, DigitalOcean, Hetzner — our derivative ships on the same targets).

**Opinionated defaults baked into the image.**
- `AAOS_DEFAULT_BACKEND=namespaced` on by default (the `#15` postinst probe is no longer needed — the image guarantees a capable kernel).
- `NamespacedBackend` Landlock-backed by default, seccomp stacked on top, cgroups v2 quotas per agent.
- Desktop meta-packages stripped (no X11, no Wayland, no LibreOffice — headless appliance).
- Custom motd pointing at the socket, the journal, and the docs URL.
- journald as the default audit sink.
- Key provisioning via cloud-init user-data (cloud snapshots) or first-boot prompt (ISO). `agentd configure` from `#15` remains the fallback for every path.

**Isolation layers used by the derivative.**
- **Namespaces** for per-agent isolation (mount, pid, net, user, cgroup).
- **Seccomp-BPF** as a damage-limiter. Syscall allowlists per agent derived from manifest capabilities.
- **Landlock** (Linux 5.13+) for filesystem capability enforcement at the kernel layer.
- **cgroups v2** for CPU/memory/I/O quotas per agent.
- **Typed MCP wrappers for Linux tools** — `grep`, `jq`, `git`, `cargo`, `gcc`, `ffmpeg`, `pandoc` — each exposed as a tool with a declared capability.

**Framing.** This is a **Debian derivative**, not a from-scratch distribution. Upstream Debian 13 + our `.deb` preinstalled + opinionated defaults, built via Packer. We inherit Debian's kernel, apt repos, CVE response, and release engineering. Scope model: Home Assistant OS, Raspberry Pi OS, DietPi, Tailscale's prebuilt images. Not Fedora CoreOS / Bottlerocket / Talos (those are full distributions built and released by teams of dozens). A solo maintainer can run a derivative.

Capability tokens stay the policy model; Linux primitives are the defense-in-depth backstop. `agentd` still runs as a systemd service — not PID 1. Full component sketch in [`distribution-architecture.md`](distribution-architecture.md).

---

## Research branch

### R1 — Isolation ladder (MicroVM, microkernel)
*ex-"Phase G"*

With two backend implementations already proving `AgentServices` is substrate-agnostic, R1 explores a third: MicroVM-per-agent via Firecracker or Kata. The same agent manifest runs on different isolation levels depending on threat model:

- **Level 1 — Process** (current): Linux process with seccomp + Landlock. Low overhead, trusted workloads.
- **Level 2 — MicroVM**: Firecracker / Kata / gVisor per agent (or per swarm). Hardware-virtualized isolation; what AWS Lambda and Fly.io use. Strong tenant isolation without writing a kernel.
- **Level 3 — Microkernel**: seL4 or Redox backend, only pursued if a specific market segment (high-assurance regulated deployments) demands formally-verified isolation enough to fund it. Documented as a backend option on a clean ABI so the door stays open.

**Why this matters.** The `AgentServices` trait was originally pitched as "future syscall interface." Reframe: it's a **substrate-agnostic ABI**. An operator picks isolation based on threat model and resource budget, not on what kernel we happened to build.

**Prerequisites.** M1 (image) ships. Real workloads on hardened Linux prove the capability model. If tenant-isolation pressure emerges, MicroVM backend is the next layer. Microkernel only if formally-verified enforcement is a buyer's gating requirement.

---

## Ongoing strands

### AgentSkills
*standard support · ongoing*

aaOS supports the [AgentSkills](https://agentskills.io) open standard by Anthropic. Skills are the universal way to give agents capabilities — used by Claude Code, Copilot CLI, Gemini CLI, Qwen CLI, OpenCode, Goose, and VS Code.

**What shipped:** skill loader (`aaos-core::skill`) parses SKILL.md files per the specification. `SkillRegistry` manages loaded skills. `skill_read` tool serves full instructions and reference files with path traversal protection. Skill catalog injected into agent system prompts at spawn time (progressive disclosure tier 1). 21 production-grade skills bundled from [addyosmani/agent-skills](https://github.com/addyosmani/agent-skills).

**What this enables:** any AgentSkills-compatible skill works in aaOS — but under capability enforcement that no other runtime provides. The same skill that has open shell access in Claude Code runs under unforgeable capability tokens in aaOS. Skills become the "driver model" for agent capabilities; the runtime provides the security boundary.

Bundled-skills installation under `/usr/share/aaos/skills/` shipped with build-history #15 (2026-04-19).

### Self-reflection runs
*ongoing*

The runtime reads its own code, finds bugs, proposes features, and produces tested patches end to end. The reflection log under [`reflection/`](reflection/README.md) is the authoritative record. Highlights:

- **Runs 1–3** — real bug fixes (path traversal, capability revocation, constraint enforcement).
- **Run 4** — feature proposal (Meta-Cognitive Coordination Layer) shipped as a minimal version after external review.
- **Runs 5–10** — memory protocol, kernel-level handoff gaps, adversarial bug-hunt finding seven bugs including a symlink-bypass of the run-1 traversal fix, four-agent chain producing a grounded error-handling proposal.
- **`.deb` packaging runs (2026-04-15)** — `agentd` as a Debian package, CLI, computed orchestration with structured Planner + deterministic PlanExecutor, role catalog.
- **Tuning runs (2026-04-16 / 17)** — Planner prompt fixes, role-budget wiring, enriched telemetry (args/result previews), replan-on-subtask-failure, NamespacedBackend re-verification, secret isolation (env scrub + 0600 conffile), gitleaks pre-commit + SECURITY.md.
- **First self-build run (2026-04-17)** — `cargo_run` + `builder` role let an agent read a plan, run `cargo check/test` against aaOS from inside aaOS, and correctly report "already implemented" with zero fabricated edits.
- **Tool-gap iteration (2026-04-17)** — runs 5–6 of the second self-build attempt failed to produce a diff — not from the model but because `file_read` returned whole files and there was no `file_edit` primitive. Diagnosis: self-build is tool-bound, not model-bound. Shipped `file_edit` + `file_read(offset, limit)` in commit `2819921`.
- **aaOS edits aaOS (2026-04-17)** — first end-to-end self-build success. 471 s wall clock. Nine `file_read(offset, limit)` calls paged through the 2700-line file; five `file_edit` calls applied all anchors on first try; `cargo check` + `cargo test` both passed. The agent's diff was byte-identical to the maintainer's manual fix.
- **Junior-senior workflow (runs 8–12)** — aaOS itself is now the author of new code. Senior (human) writes plans + reviews; junior (agent on an ephemeral droplet) applies the edits. Runs 8–10 shipped the `grep` navigation primitive. Run 11 added the tool-repeat guard (hint injection at attempt ≥ 3 on same `(agent, tool, input_hash)`), plus a budget bump and a plan-complete checklist. Run 12 shipped the `git_commit` tool, completing the five-tool coding surface (`file_read(offset, limit)`, `file_edit`, `file_list`, `grep`, `git_commit` — with `cargo_run` for build/test).
- **Release-gated droplet QA (2026-04-19)** — first tagged release (`v0.0.1`) immediately soak-tested on a fresh Debian 13 droplet via a five-pass QA suite (package hygiene, daemon lifecycle, canonical goal, fault injection, MCP + skills). Six bugs surfaced — Bug 1 critical (`.deb` missing `--features namespaced-agents` → confinement silently disabled). All closed in `v0.0.2` (`160861f` + `d1cbe8c`), verified via a repeat QA pass. Extended full-suite re-run (purge idempotency, non-root operator access, `kill -9` recovery, OOM, disk-full, skill shadowing, external MCP stdio client) surfaced Bug 7 (memory tools not in `WORKER_SIDE_TOOLS` whitelist) — closed in `v0.0.3` (`03d384f`).

Cross-cutting lessons distilled from the runs (LLM calendar estimates aren't real, cost from token-math ≠ dashboard, skill adherence evolves, prompts persuade but only the kernel enforces, structured handoff beats opaque prompts, coding agents are tool-bound not model-bound) live in [`patterns.md`](patterns.md).

**What's deferred pending more data:** the structured `PatternStore`, new `aaos-reflection` crate, and `CoordinationPattern` schema are still not warranted. The minimal protocol (stable Bootstrap ID + opt-in persistent memory + query-before/store-after in the manifest) is the empirical foundation. If 10-20 runs surface recurring patterns worth indexing formally, the structured system gets designed against real data — not speculation.

---

## Known architectural gaps

Beyond the shipped + queued work above, there's a third category: capabilities a reader of "Agentic Operating System" would reasonably expect that aaOS **has deliberately deferred** with named signals to reconsider. Each entry lives in [`ideas.md`](ideas.md).

What remains deferred today:

- **[Distributed / multi-host agent runtime](ideas.md#distributed--multi-host-agent-runtime).** Every agent runs in a single `agentd` on a single host. Cross-host delegation, multi-tenant swarms, and the HMAC-signed-token transport that would require are all R1-or-later.
- **[Cryptographic agent identity](ideas.md#cryptographic-agent-identity).** Commit trailers carry a prose "Co-Authored-By: aaOS builder role (ephemeral droplet, run X)" but there's no signature. Meaningful only once either multi-host transport lands or key storage moves out of `agentd`'s address space (TPM2 / HSM / enclave).
- **Self-evolution — agents that author their own MCP wrappers.** Advanced-tier framing; no concrete workload yet asks for persistent tool-library growth. Tracked in [`ideas.md`](ideas.md).

The pattern is intentional: we ship for the single-operator, single-node, trusted-`agentd` threat model that a Debian derivative actually serves, and defer the distributed / cryptographic / cost-optimizing layers until a specific workload or buyer demands them. Each deferral has a concrete signal-to-reconsider; promotion from `ideas.md` to a numbered milestone happens when that signal fires, not on speculation.
