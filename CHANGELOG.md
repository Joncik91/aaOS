# Changelog

All notable changes to aaOS.  Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); version numbers follow [Semantic Versioning](https://semver.org/).

The dpkg-format changelog at `packaging/debian/changelog` mirrors the tagged releases in short form for the `.deb` package; **this file is the authoritative human-readable record**.

Pre-v0.0.1 work (build-history #1–#13) predates the tagged-release cadence; it's captured under the `[0.0.0]` section below with ship dates and commits drawn from the roadmap's build-history section and the reflection log.

---

## [Unreleased]

Active milestone: **M1 — Debian-derivative reference image** (Packer pipeline producing a bootable ISO + cloud snapshots with the v0.0.4 `.deb` preinstalled).

### Added

- **Auto-routing: `agentd submit` now classifies the goal and picks `plan` or `persistent` automatically.** A cheap single-shot LLM call (~50 input / 1 output token) inspects the goal text before any agent work begins and routes accordingly. Operators who want to force a specific mode can still pass `--orchestration [plan|persistent]` — explicit wins, classifier is bypassed.
  - `plan` — Planner + PlanExecutor DAG. Best for structured goals with declared outputs per subtask (fetch, analyse, write). Requires a loaded role catalog; returns a clear error if the catalog is absent.
  - `persistent` — Bootstrap persistent agent. Best for open-ended, exploratory, or long-context goals where a single multi-turn agent manages its own context and spawns children as needed.
  - Classifier falls back to `plan` on any LLM error. When no LLM client is configured, auto-routes to `plan` immediately (no network call).
  - An `OrchestrationSelected { mode, source }` audit event is emitted on every submit so operators can see which path was chosen and why (`source: "explicit"` or `"auto"`).
  - The `agent.submit_streaming` JSON-RPC `"orchestration"` field is still honoured when present; omitting it now triggers auto-detection instead of defaulting silently to `plan`.

### Known — not yet fixed

- **Bug 9 (high)** — when a subtask fails to produce its declared output (tools succeeded but LLM never wrote, or tools failed silently), the Planner's replan path spawns a fallback generalist that writes a plausible-looking but **hallucinated** failure report to the output file and marks the run `complete`.  Observed twice on the v0.0.4 verification run: a generalist wrote "target directory `/src/aaOS` was not found or could not be read" to `/data/report.md` moments after three other agents had successfully read 40+ files from that path.  Whether the operator sees the real error depends on retry-count arithmetic: `max_replans` exhausted → correct `bootstrap failed`; `max_replans` remaining → silent hallucinated success.  Severity raised from medium to high after v0.0.4 run showed the fallback actively writes false content, not just "NOT COMPLETED" markers.  Fix direction: the fallback agent must inherit the failed subtask's audit trail, or be replaced by a deterministic scaffold that writes an accurate failure summary without invoking an LLM.  Tracked in `docs/reflection/2026-04-24-v0.0.3-self-reflection.md`.

---

## [0.0.4] — 2026-04-24

Second release from the same day as v0.0.3.  The v0.0.3 self-reflection droplet run (aaOS reading its own source tree under confinement) surfaced Bug 8 within 45 seconds of investigation.  No new features; patch-level release.

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.0.4> — `aaos_0.0.4-1_amd64.deb`.

### Fixed

- **Bug 8** — `grep` tool now routes daemon-side under confinement.  `grep` shells out to `rg` (ripgrep) as a subprocess; the worker's seccomp kill-filter denies `execve`, so every grep call under the namespaced backend failed with `ipc error: failed to spawn rg: Operation not permitted (os error 1)`.  Same class as Bug 7 (routing-list drift between `WORKER_SIDE_TOOLS` and `DAEMON_SIDE_TOOLS`).  Moved `"grep"` from `aaos_backend_linux::worker_tools::WORKER_SIDE_TOOLS` to `aaos_core::tool_surface::DAEMON_SIDE_TOOLS`; dropped the `GrepTool` registration from `build_worker_registry`; flipped the routing tests.  Commit `aaf82a3`.

---

## [0.0.3] — 2026-04-24

Ships the Bug 7 fix queued from the v0.0.2 extended QA pass.  No new features; patch-level release to unblock confined agents that call memory tools.

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.0.3> — `aaos_0.0.3-1_amd64.deb`.

**Known issues (fixed in 0.0.4):** Bug 8 — `grep` tool fails with `Operation not permitted` under the namespaced backend because ripgrep subprocess spawn is blocked by seccomp.  Affects any role that uses grep while confined (including the `reflector` role for self-reflection runs).  Upgrade to v0.0.4.

### Fixed

- **Bug 7** — `memory_store`, `memory_query`, `memory_delete` now correctly route daemon-side under confinement.  Previously these tools were absent from both `WORKER_SIDE_TOOLS` and `DAEMON_SIDE_TOOLS`, causing a `tool error: tool memory_X not available in worker` failure under the namespaced backend.  Memory tools need HTTP access to the embedding endpoint that the worker sandbox can't provide, so they join `web_fetch`, `cargo_run`, and `git_commit` in `DAEMON_SIDE_TOOLS` in `aaos-core::tool_surface`.  Surfaced by the v0.0.2 extended-QA pass in [`docs/reflection/2026-04-19-v0.0.2-droplet-qa.md`](docs/reflection/2026-04-19-v0.0.2-droplet-qa.md).  Commit `03d384f`.

---

## [0.0.2] — 2026-04-19

First QA-driven patch.  Fresh-droplet soak test of the v0.0.1 `.deb` surfaced six bugs; this release fixes all of them.  See [`docs/reflection/2026-04-19-v0.0.1-droplet-qa.md`](docs/reflection/2026-04-19-v0.0.1-droplet-qa.md) for the original QA record and [`docs/reflection/2026-04-19-v0.0.2-droplet-qa.md`](docs/reflection/2026-04-19-v0.0.2-droplet-qa.md) for the v0.0.2 verification pass (all six bugs confirmed closed end-to-end; one new Bug 7 surfaced — fixed in `[0.0.3]` above).

**Known issues (fixed in 0.0.3):** Bug 7 — memory tools (`memory_store` / `memory_query` / `memory_delete`) fail under the namespaced backend with `tool error: tool memory_X not available in worker`.  Affects agents that declare memory capabilities while running confined.  No workaround in v0.0.2; upgrade to v0.0.3.

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.0.2> — `aaos_0.0.2-1_amd64.deb`, 4.29 MB.

### Fixed

- **[Critical]** `.deb` now includes the `namespaced-agents` feature.  v0.0.1 shipped with `--features mcp` only, so `AAOS_DEFAULT_BACKEND=namespaced` (whether operator-set or generated by the postinst probe) silently fell through to `InProcessBackend`.  Every tool call audit-tagged `[daemon]` regardless of env.  `packaging/build-deb.sh` default `AAOS_BUILD_FEATURES` is now `mcp,namespaced-agents`.  Commit `160861f`.
- `NamespacedBackend::stop` now reaps child processes.  Prior versions left an `[aaos-agent-worker] <defunct>` zombie in the process table per subtask run; over a long-lived daemon these would accumulate until the PID ceiling.  New flow: SIGTERM → poll WNOHANG for 500 ms → escalate to SIGKILL + blocking reap.
- `agentd submit` now renders the daemon's error message on failed runs.  A mistyped or expired API key previously produced `bootstrap failed (0k in / 0k out, 0s)` with zero context; the error field on the streaming `end` frame was discarded by the CLI.  Now prints `error: <message>` with a pointer at `journalctl -u agentd`.  Daemon also emits a structured `tracing::error!` with `run_id`.
- MCP subsystem now logs startup state.  `"MCP client: attempting to connect to N configured server(s)"`, `"MCP client: N of M server(s) registered"`, `"MCP server: starting loopback listener on <bind>"`.  No-config case prints `"no /etc/aaos/mcp-servers.yaml — MCP disabled (copy .example to enable)"`.  Prior versions were completely silent about MCP state.
- `"using NamespacedBackend"` startup log fires once per process instead of twice.  `Server::new()` + `Server::with_llm_client()` both built a backend; OnceLock guard deduplicates.
- All 11 lintian errors addressed.  `packaging/debian/copyright` references `/usr/share/common-licenses/Apache-2.0` per Debian Policy 12.5; `packaging/debian/changelog` added per Policy 12.7; `packaging/agentd.service` moves to `usr/lib/systemd/system/` per Debian Trixie merged-usr; release binaries explicitly stripped (`strip --strip-unneeded` in `build-deb.sh`); `packaging/debian/lintian-overrides` covers remaining warn-level tags with per-tag rationale.

### Changed

- `.deb` size shrinks 4.92 MB → 4.29 MB after binary stripping.
- Final lintian output: **0 errors, 0 warnings** (down from 11 errors + 9 warnings on v0.0.1).

---

## [0.0.1] — 2026-04-19

First tagged release.  The runtime, capability model, MCP integration, confinement, scheduler, routing, TTL, self-reflection loop, and operator CLI had already shipped as untagged development work (see `[0.0.0]` below); v0.0.1 wrapped them in the release-pipeline infrastructure.

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.0.1> — `aaos_0.0.1-1_amd64.deb`, 4.25 MB.

### Added

- CI-built `.deb` on `v*` tag push via `.github/workflows/release.yml`.  Builds inside a `debian:13` container so cargo-deb encodes Debian's libc/systemd minimums.  Attaches the artifact to an auto-generated GitHub Release with `contents: write` permission.  Commits `1ae9432` + `f61a967`.
- Agentic-by-default `.deb` surface (formerly roadmap milestone M1, now build-history #15).  Five `.deb`-level changes made the package useful out of the box:
    - `packaging/build-deb.sh` ships `agentd` with `--features mcp` by default (bidirectional MCP both directions on from install).  Commit `a6c993b`.
    - `/etc/aaos/mcp-servers.yaml.example` template with commented-out GitHub MCP (HTTP), filesystem MCP (stdio via npx), and git MCP (stdio via uvx) entries.  Commit `54499de`.
    - 21 AgentSkills bundled under `/usr/share/aaos/skills/` (FHS-correct vendor-supplied read-only data).  `discover_all_skills` probes `/usr/share/aaos/skills/` → `/etc/aaos/skills/` → `/var/lib/aaos/skills/`; `AAOS_SKILLS_DIR` appends last.  Commit `5c78a04`.
    - `packaging/debian/postinst` probes `/sys/kernel/security/lsm` for `landlock` + `/proc/sys/kernel/unprivileged_userns_clone`; generates `/etc/default/aaos.example` with `AAOS_DEFAULT_BACKEND=namespaced` + `AAOS_CONFINE_SUBTASKS=1` uncommented when both probes pass.  Commit `9f18848`.
    - `agentd configure` subcommand: interactive or `--key-from-env` non-interactive API-key setup that atomically writes `/etc/default/aaos` mode 0600 root:root (tempfile + fsync + rename) and restarts the daemon.  Commit `4bb5e38`.
- `namespaced-agents` feature-on compile check in the fast CI job (`check-lint`).  Previously only exercised under `--ignored` with kernel primitives; a compile regression could sneak through.  Commit `801c08d`.

### Changed

- Workspace crates bumped `0.0.0` → `0.0.1` (`aaos-mcp` stays on its own `0.1.0` cadence).  Commit `779dd62`.
- Clippy CI gate flipped from advisory (`continue-on-error: true` + `-W clippy::all`) to enforced (`-D warnings`).  Required fixing 57 latent warnings first; most auto-fixed via `cargo clippy --fix`, the remainder got targeted `#[allow(...)]` with rationale for genuinely structural items (type-complexity in MCP transport factory + invocation-test fixtures; too-many-arguments on `persistent_agent_loop` + `build_in_process_backend`; `await_holding_lock` on a sync-mutex-for-env-var test pattern).  Commit `d1c4274`.
- Release workflow `contents: write` permission added so `softprops/action-gh-release@v2` can create Releases and attach the `.deb`.  Commit `f61a967`.

### Known issues (fixed in 0.0.2)

- Confinement silently disabled despite `AAOS_DEFAULT_BACKEND=namespaced` — the release build was missing `--features namespaced-agents`.  Download the `v0.0.2` `.deb` instead.
- Zombie `aaos-agent-worker` children accumulate after each run.
- Invalid API keys fail silently with no error message.
- MCP subsystem startup state completely silent.

---

## [0.0.0] — pre-tagged development (2026-03-21 through 2026-04-19)

Before v0.0.1 there was a month of untagged development.  What existed in the tree at the moment the v0.0.1 tag was cut, collapsed into a retrospective changelog:

### Added — runtime foundation

- **Runtime prototype** (2026-03-21, commit `029d90b`).  6 Rust crates, 3,917 lines, 111 passing tests.  Capability-based security with two-level enforcement (tool access + resource path), `AgentServices` + `Tool` traits, LLM execution loop, MCP message routing, human-in-the-loop approval queue.
- **Persistent agents + request-response IPC** (2026-03-25).  `persistent_agent_loop` on a tokio task; `send_and_wait` on `AgentServices`; JSONL session store; Pause/Resume/Stop commands; 30 new tests, 141 total.
- **Managed context windows** (late March).  `ContextManager` with LLM-based summarization when usage exceeds a configurable threshold.  Older messages archived to disk; Summary messages folded into the system prompt.  25 new tests, 166 total.
- **Episodic memory store** (late March).  New `aaos-memory` crate (7th workspace member) with `MemoryStore` trait + in-memory cosine-similarity impl + Ollama/mock embedding sources.  `memory_store`/`memory_query`/`memory_delete` tools.  39 new tests, 205 total.
- **Self-bootstrapping swarm** (early April).  Docker container with `agentd` as PID 1 and a Bootstrap Agent that self-organizes child agents to accomplish goals.  Canonical run: "fetch HN top 5 and write a summary" completes in ~75 s for ~$0.03.

### Added — provider support + scheduling

- **Multi-provider LLM** (early April).  `OpenAiCompatibleClient` speaks OpenAI Chat Completions; DeepSeek / OpenRouter / any OpenAI-compatible provider works.  Daemon prefers `DEEPSEEK_API_KEY`, falls back to `ANTHROPIC_API_KEY`.
- **Inference scheduler** (early April).  `ScheduledLlmClient` wraps any `LlmClient` with a tokio semaphore (default 3 concurrent) + optional rate smoothing.  Env vars `AAOS_MAX_CONCURRENT_INFERENCE`, `AAOS_MIN_INFERENCE_DELAY_MS`.
- **Per-agent token budgets** (early April).  `BudgetTracker` with lock-free atomic CAS; manifests declare `budget_config: { max_tokens, reset_period_seconds }`; over-budget agents get `BudgetExceeded` errors.

### Added — Debian packaging

- **`agentd` as a Debian package** (2026-04-15, commits `5717906` + `8d45691`).  `.deb` buildable via `cargo deb -p agentd`; `postinst` creates the `aaos` system user + group; systemd `StateDirectory=aaos` + `RuntimeDirectory=agentd` own dir creation; socket at `/run/agentd/agentd.sock` mode 0660.  `postrm purge` cleans state + user.  Hardening: `NoNewPrivileges`, `ProtectSystem=full`, `ProtectHome`, `PrivateTmp`, `ProtectKernelTunables`, `ProtectKernelModules`, `ProtectControlGroups`.  Dependencies: `$auto, systemd, ca-certificates`.
- **Operator CLI** (2026-04-16, commits `58dd1bb`..`5e01acc`).  `agentd submit | list | status | stop | logs | roles` subcommands + server-side NDJSON streaming (`agent.submit_streaming`, `agent.logs_streaming`) + `BroadcastAuditLog` + `aaos` system group + `agentd(1)` man page.
- **Computed orchestration** (2026-04-16, commits `9b001cb`..`cbd3dc7`).  Two-phase boot replacing single-LLM orchestration: cheap-LLM Planner emits a structured `Plan { subtasks, depends_on, final_output }`; deterministic Rust `PlanExecutor` walks the DAG in dependency-ordered batches running independents concurrently via `futures::try_join_all`.  126 new runtime tests.  Role catalog at `/etc/aaos/roles/` (fetcher, writer, analyzer, generalist).
- **Computed-orchestration follow-ups** (2026-04-17).  Planner prompt rules (`dfb97f9`), `{inputs.*}` capability expansion (`6b2387e`), role-budget wiring into per-subtask `ExecutorConfig` (`ef45e61`), tightened fetcher/analyzer/writer prompts (`c412a14`).  Canonical goal timing 5m30s → 28s.
- **Deterministic scaffold roles** (2026-04-17, commit `2b8ed6d`).  Roles can declare `scaffold: kind: <name>`; `PlanExecutor` dispatches to a `ScaffoldRunner` closure instead of an LLM loop.  Fetcher ships as the first scaffold: `web_fetch → file_write → return workspace path` with HTTP-status + empty-body rejection.  Closes the fabrication bug where LLMs emitted plausible `"written to <path>"` acks without calling `file_write`.
- **`cargo_run` tool + `builder` role** (2026-04-17, commit `45ce06b`).  Executes `cargo {check,test,clippy,fmt}` in a capability-scoped workspace; subcommand allowlist + 4-minute timeout + 8 KB inline output cap.
- **Bidirectional MCP integration** (2026-04-18).  `aaos-mcp` crate.  **Client:** per-entry stdio or HTTP sessions with `initialize` + `tools/list` handshake, tools register as `mcp.<server>.<tool>`, per-session reconnect loop with exponential backoff.  **Server:** axum HTTP+SSE listener on `127.0.0.1:3781` exposing `submit_goal`, `get_agent_status`, `cancel_agent`; SSE stream at `GET /mcp/events?run_id=<id>`.

### Added — Agent-kernel primitives

- **Reasoning-slot scheduler** (2026-04-18, commits `c2b56de`..`9b8e15a`).  Runtime-owned `ReasoningScheduler` awards LLM inference slots via a `BinaryHeap<Reverse<ReasoningRequest>>` priority queue keyed on subtask wall-clock deadline.  `SchedulerView` wraps the LLM client per subtask; every subtask's `complete()` call routes through the scheduler and records elapsed time in a `LatencyTracker`.
- **Per-task TTL + latency** (2026-04-18).  `TaskTtl { max_hops, max_wall_clock }` on `Subtask`.  `spawn_subtask` refuses launch when `max_hops == 0`; `tokio::select!` race cancels the runner future on wall-clock expiry.  Emits `SubtaskTtlExpired { reason }` audit events.
- **Dynamic model routing** (2026-04-19, commits `cd55c8c`..`68c9112`).  Roles declare `model_ladder: Vec<String>` + `escalate_on: Vec<EscalationSignal>`.  `Subtask.current_model_tier` bumps on replan when a configured signal (`ReplanRetry`, `ToolRepeatGuard`, `MaxTokens`) fired during the failed attempt.  `SubtaskModelEscalated` + `ToolRepeatGuardFired` audit events operator-visible in the default `agentd submit` stream.
- **Runtime-side tool confinement** (2026-04-19, commits `0a47bb3`..`7adc147`).  When `AAOS_DEFAULT_BACKEND=namespaced`, plan-executor subtasks + `spawn_agent` children run their filesystem + compute tools inside the worker under Landlock + seccomp.  `ToolInvocation::invoke` routes via `route_for(tool_name, backend_kind)` → worker over the post-handshake broker stream, or daemon-side for tools that inherently need the daemon's authority.  Capability tokens forwarded with each `InvokeTool`.  Workspace + manifest-declared output roots bind-mounted at matching absolute paths.  Worker-side whitelist: `file_read`, `file_write`, `file_edit`, `file_list`, `file_read_many`, `grep`.  Daemon-side permanently: `web_fetch`, `cargo_run`, `git_commit`, the LLM loop.

### Added — supporting infrastructure

- **Self-reflection log** — 41 dated run entries under `docs/reflection/`; each captures setup, what worked, what the run exposed, what shipped, and cost.  Cross-cutting lessons lifted into `docs/patterns.md`.
- **AgentSkills support** — skill loader parsing upstream `SKILL.md` files; `SkillRegistry` + `skill_read` tool with path-traversal protection; skill catalog injected into agent system prompts at spawn time.  21 skills bundled from [addyosmani/agent-skills](https://github.com/addyosmani/agent-skills).
- **Capability-token forgery — threat-model split.** Four distinct threat classes enumerated in `docs/ideas.md`; in-process forgery closed (handle-opaque tokens, handle field private to `aaos-core`), worker-side forgery closed (peer-creds on broker, no handles in launch protocol), registry memory tampering named as open (needs external key storage), cross-process transport named as N/A-until-Phase-G.

### Changed

- `agentd` 6 crates → 9 crates, ~4 k LoC → ~37 k LoC, 111 tests → 605+ passing + 19 `#[ignore]`-gated.
- Handle-based capability tokens: `aaos-tools` never sees a `CapabilityToken` struct, only opaque `CapabilityHandle` values; the handle's inner `u64` is `aaos-core`-private.

No `.deb` was attached to a `v0.0.0` tag — this release was the untagged development line.  The first installable artifact is `v0.0.1`'s `.deb` (see above).

---

[Unreleased]: https://github.com/Joncik91/aaOS/compare/v0.0.4...HEAD
[0.0.4]: https://github.com/Joncik91/aaOS/releases/tag/v0.0.4
[0.0.3]: https://github.com/Joncik91/aaOS/releases/tag/v0.0.3
[0.0.2]: https://github.com/Joncik91/aaOS/releases/tag/v0.0.2
[0.0.1]: https://github.com/Joncik91/aaOS/releases/tag/v0.0.1
[0.0.0]: https://github.com/Joncik91/aaOS/commits/779dd62
