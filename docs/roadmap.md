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

The runtime has begun reading its own code, finding bugs, and proposing features. Seven runs to date — runs 1-3 produced real bug fixes (path traversal, capability revocation, constraint enforcement), run 4 produced a feature proposal (Meta-Cognitive Coordination Layer) shipped as a minimal version after external review, run 5 exercised the persistent-memory protocol end-to-end and produced three manifest-only tuning fixes, run 6 surfaced two kernel-level gaps in the Run-5 manifest tuning (soft rules aren't enforcement; no structured child-to-child data channel) that shipped as kernel fixes `505f559` and `5feedbe`, and run 7 validated those fixes against real behavior with a four-agent chain producing a grounded error-handling unification proposal.

Full chronological detail per run lives in [`reflection/`](reflection/README.md). Cross-cutting lessons distilled from the runs (LLM calendar estimates aren't real, cost from token-math ≠ dashboard, skill adherence evolves, prompts persuade but only the kernel enforces, structured handoff beats opaque prompts) are in [`patterns.md`](patterns.md).

**What's deferred pending more data:** the structured `PatternStore`, new `aaos-reflection` crate, and `CoordinationPattern` schema are still not warranted. The minimal protocol (stable Bootstrap ID + opt-in persistent memory + query-before/store-after in the manifest) is the empirical foundation. If 10-20 runs surface recurring patterns worth indexing formally, the structured system gets designed against real data — not speculation.

## Phase F: Debian Derivative *(next)*

Full component sketch in [`distribution-architecture.md`](distribution-architecture.md). Short version below.

**Scope framing up front.** Phase F is a **Debian derivative**, not a from-scratch distribution. Upstream Debian 13 + our `.deb` preinstalled + opinionated systemd/config defaults, built via Packer, shipped as bootable ISO + cloud snapshots. We inherit Debian's kernel, apt repos, CVE response, and release engineering — we maintain only the aaOS-specific layers. Scope model: Home Assistant OS, Raspberry Pi OS, DietPi, Tailscale's prebuilt images. Not Fedora CoreOS, Bottlerocket, or Talos (those are full distributions built and released by teams of dozens). A solo maintainer can run a derivative. A solo maintainer cannot run a distribution.

**Why this shape, not a microkernel fork.** aaOS's differentiation is capability semantics, delegation, auditability, and policy compilation — not owning a kernel. A microkernel migration pushes the "it ships" date years out while losing the Linux ecosystem (GPU drivers, package management, every tool an agent might call through typed wrappers). A Debian derivative puts the capability model in real users' hands within quarters, not years.

Phase F splits into two explicit milestones.

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

### Phase F-b: Debian-derivative reference image

A Packer pipeline that starts from an upstream Debian 13 base image, preinstalls the aaOS `.deb`, enables the service, and bakes opinionated defaults.

**Deliverable.** Bootable ISO + cloud snapshots (Debian publishes official images on AWS, DigitalOcean, Hetzner — our derivative ships on the same targets).

**Opinionated defaults baked into the image.**
- `AAOS_DEFAULT_BACKEND=namespaced` when the host kernel supports unprivileged user namespaces + Landlock (Linux 5.13+).
- `NamespacedBackend` Landlock-backed by default, seccomp stacked on top, cgroups v2 quotas per agent.
- Desktop meta-packages stripped (no X11, no Wayland, no LibreOffice — headless appliance).
- Custom motd pointing at the socket, the journal, and the docs URL.
- journald as the default audit sink.

**What the derivative does not do.** We do not maintain our own apt repos. We do not track CVEs. We do not maintain the kernel. We do not run a release-engineering cadence. Upstream Debian does all of that; the derivative pulls from `deb.debian.org` like every other Debian install. Our work is confined to the `.deb` (Phase F-a) and the Packer pipeline + default config (Phase F-b).

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

Next: Phase F-b — Packer pipeline producing a Debian-derivative image with the `.deb` preinstalled, `namespaced` backend as default, desktop meta-packages stripped, opinionated motd/config. First cloud target + bootable ISO.

## Phase G: Isolation Ladder *(research branch)*

With two backend implementations already proving `AgentServices` is substrate-agnostic, Phase G adds a third: MicroVM-per-agent via Firecracker or Kata. The same agent manifest runs on different isolation levels depending on threat model:

- **Level 1 — Process** (current): Linux process with seccomp+Landlock. Low overhead, appropriate for trusted workloads.
- **Level 2 — MicroVM**: Firecracker / Kata / gVisor per agent (or per swarm). Hardware-virtualized isolation; what AWS Lambda and Fly.io use. Strong tenant isolation without writing a kernel.
- **Level 3 — Microkernel** (research): seL4 or Redox backend, only pursued if a specific market segment (high-assurance regulated deployments) demands formally verified isolation enough to fund it. Not prioritized; documented as a backend option on a clean ABI so the door stays open.

**Why this matters.** The `AgentServices` trait was originally pitched as "future syscall interface." Reframe: it's a **substrate-agnostic ABI**. An operator picks their isolation level based on threat model and resource budget, not on what kernel we happened to build.

**Prerequisites.** Phase F ships. Real workloads on hardened Linux prove the capability model. If tenant-isolation pressure emerges, MicroVM backend is the next layer. Microkernel only if formally-verified enforcement is the buyer's gating requirement.
