# Roadmap

The prototype demonstrates that agent-first OS abstractions work: capability-based security, structured IPC, tool execution with two-level enforcement, agent orchestration with capability narrowing, and human-in-the-loop approval. Everything below builds on that foundation.

The roadmap is organized in three sections:

- **Build history** — shipped work, ordered by landing date. Flat numbering (1…N); no nested alphanumerics.
- **Active milestones** — the next concrete deliverables. Numbered M1, M2, …
- **Research branch** — directions we expect to explore when a specific workload or buyer forces the question.

Plus two ongoing strands — **AgentSkills** and **Self-reflection runs** — that are continuous, not phased.

Where an old label (e.g. "Phase F-b/3" or "C2") appears in reflection logs, commit messages, or external notes, the `ex-<old label>` line under each heading below preserves the mapping.

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

---

## Active milestones

### M1 — Agentic-by-default `.deb`
*next*

The 2026-04-19 `.deb` audit surfaced the gap: the package installs green but a fresh operator still has to paste an LLM key, know about `--features mcp`, and discover `AAOS_SKILLS_DIR` before any agent does useful work. M1 closes those at the `.deb` level — no image work, ships via the existing release workflow.

**Scope.**
- **MCP enabled in the release build.** `packaging/build-deb.sh` and `.github/workflows/release.yml` build with `--features mcp` so the MCP client (external tools register as `mcp.<server>.<tool>`) and the loopback server (`127.0.0.1:3781`) are both on by default. Claude Code, Cursor, any MCP client can delegate goals to aaOS without rebuilding.
- **Skills catalog bundled.** Ship the 21 skills from [addyosmani/agent-skills](https://github.com/addyosmani/agent-skills) under `/usr/share/aaos/skills/` as a `.deb` asset. Default `AAOS_SKILLS_DIR=/usr/share/aaos/skills` in an installed `/etc/default/aaos.example` template. First-boot agents get a populated catalog instead of an empty one.
- **Confinement-by-default on capable kernels.** `packaging/debian/postinst` probes Landlock + unprivileged user namespaces; if both are present, the generated `/etc/default/aaos.example` sets `AAOS_DEFAULT_BACKEND=namespaced` and `AAOS_CONFINE_SUBTASKS=1`. Falls back to `inprocess` on older kernels with a logged-at-startup notice explaining why.
- **One-step key bootstrap.** A new `agentd configure` subcommand prompts interactively for an LLM key, writes `/etc/default/aaos` mode 0600 root:root, and `systemctl restart agentd`. The daemon detects a missing key on startup and logs a single actionable line pointing at the command. No free-form config editing unless the operator wants it.
- **MCP server YAML template.** Install `/etc/aaos/mcp-servers.yaml.example` with commented-out GitHub + filesystem MCP entries so an operator copying-and-uncommenting has external tool sources in one minute, not one hour of schema reading.

**Deliverable.** `apt install ./aaos_0.X.Y-1_amd64.deb` + one `agentd configure` produces a daemon that: (a) confines subtasks under Landlock + seccomp where the kernel supports it, (b) can register external MCP tools from a template, (c) exposes goals to external MCP clients on loopback, (d) has a skills catalog agents can actually query.

**Out of scope for M1.** No Packer image work (that's M2). No new runtime features. No web UI. No self-evolution tool authoring (tracked in `ideas.md`).

### M2 — Debian-derivative reference image
*after M1*

A Packer pipeline that starts from an upstream Debian 13 base image, preinstalls the M1 `.deb`, enables the service, and bakes opinionated defaults.

**Deliverable.** Bootable ISO + cloud snapshots (Debian publishes official images on AWS, DigitalOcean, Hetzner — our derivative ships on the same targets).

**Opinionated defaults baked into the image.**
- `AAOS_DEFAULT_BACKEND=namespaced` on by default (the M1 probe is no longer needed — the image guarantees a capable kernel).
- `NamespacedBackend` Landlock-backed by default, seccomp stacked on top, cgroups v2 quotas per agent.
- Desktop meta-packages stripped (no X11, no Wayland, no LibreOffice — headless appliance).
- Custom motd pointing at the socket, the journal, and the docs URL.
- journald as the default audit sink.
- Key provisioning via cloud-init user-data (cloud snapshots) or first-boot prompt (ISO). `agentd configure` from M1 remains the fallback for every path.

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

**Prerequisites.** M1 + M2 ship. Real workloads on hardened Linux prove the capability model. If tenant-isolation pressure emerges, MicroVM backend is the next layer. Microkernel only if formally-verified enforcement is a buyer's gating requirement.

---

## Ongoing strands

### AgentSkills
*standard support · ongoing*

aaOS supports the [AgentSkills](https://agentskills.io) open standard by Anthropic. Skills are the universal way to give agents capabilities — used by Claude Code, Copilot CLI, Gemini CLI, Qwen CLI, OpenCode, Goose, and VS Code.

**What shipped:** skill loader (`aaos-core::skill`) parses SKILL.md files per the specification. `SkillRegistry` manages loaded skills. `skill_read` tool serves full instructions and reference files with path traversal protection. Skill catalog injected into agent system prompts at spawn time (progressive disclosure tier 1). 21 production-grade skills bundled from [addyosmani/agent-skills](https://github.com/addyosmani/agent-skills).

**What this enables:** any AgentSkills-compatible skill works in aaOS — but under capability enforcement that no other runtime provides. The same skill that has open shell access in Claude Code runs under unforgeable capability tokens in aaOS. Skills become the "driver model" for agent capabilities; the runtime provides the security boundary.

Bundled-skills installation under `/usr/share/aaos/skills/` ships with M1.

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
