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
