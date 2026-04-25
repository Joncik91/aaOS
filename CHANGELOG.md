# Changelog

All notable changes to aaOS.  Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); version numbers follow [Semantic Versioning](https://semver.org/).

The dpkg-format changelog at `packaging/debian/changelog` mirrors the tagged releases in short form for the `.deb` package; **this file is the authoritative human-readable record**.

Pre-v0.0.1 work (build-history #1–#13) predates the tagged-release cadence; it's captured under the `[0.0.0]` section below with ship dates and commits drawn from the roadmap's build-history section and the reflection log.

---

## [Unreleased]

Active milestone: **M1 — Debian-derivative reference image** (Packer pipeline producing a bootable ISO + cloud snapshots with the v0.1.5 `.deb` preinstalled).

---

## [0.1.5] — 2026-04-25

Same-day patch closing two findings from the round-3 v0.1.4 self-reflection run.  Third finding deferred (FileWriteTool TOCTOU — theoretical under current threat model).

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.1.5> — `aaos_0.1.5-1_amd64.deb`.

### Fixed

- **Bug 24 (low — security doc correctness)** — `crates/aaos-backend-linux/src/broker_session.rs` module-level documentation claimed "seccomp denying `dup2`" was the mitigation against fd-handoff attacks after `SO_PEERCRED` validation.  Two factually wrong claims: (a) `seccomp_compile.rs:99` explicitly *allows* `dup3` (tokio uses it for stdio plumbing), and (b) `dup2` is not on either list — it falls through to default EPERM, not the SIGSYS the comment implied.  Corrected the doc to reflect the actual mitigations: Landlock (filesystem confinement) + user namespace (process scope) + broker session-id correlation at `register_session()` time.  Runtime behaviour is unchanged — only the documentation was misleading.  Commit `5f8b7c5`.

- **Bug 25 (low-medium — async correctness)** — `crates/aaos-runtime/src/registry.rs::stop()` held a `DashMap` shard guard across an `mpsc::send().await`.  Under heavy mpsc-buffer pressure (a slow agent loop draining commands), the await would stall until the buffer drained, blocking any other task contending on the same shard.  Fixed: clone `command_tx` before the guard's scope ends, await outside.  Standard async-Rust pattern.  Commit `5f8b7c5`.

### Investigation

- The agent's claim that the Bug 21 fix (`7d8db0f`) introduced a deadlock was **disproved** by source review: `registry.rs:252` explicitly `drop(entry)` releases the `agents`-DashMap lock before `remove_agent` is called at line 260, and `remove_agent`'s `revoke_all_capabilities` call hits `capability_registry` (a separate `Arc`) — no re-entrant lock.  Bug 21's fix is correct; no revert needed.

### Deferred

- **Finding 1 — FileWriteTool parent-dir-then-write TOCTOU.**  Real race window between `fs::create_dir_all(parent)` and `fs::write(path, content)` in `crates/aaos-tools/src/file_write.rs`, but an attacker requires both a capability token AND independent filesystem write access to the workspace.  Worker confinement (Landlock + user namespace) constrains the symlink-redirect surface.  Proper fix needs `openat`/`O_PATH` component-walk; out of scope for v0.1.x.  Logged as a future hardening item.

---

## [0.1.4] — 2026-04-25

### Known — still open (triaged 2026-04-25, none blocking)

- **Bug 14 (informational)** — `commit_nudges` mechanism added in v0.1.0 (`cba106b`).  v0.1.2 expanded its trigger condition (`commit-nudges` now fires on empty `tool_uses` with `stop_reason=ToolUse`, not just `EndTurn`); both forms remain in place as safety nets.
- **Bug 18 (theoretical, accepted-risk)** — TOCTOU in `CapabilityRegistry::narrow`.  Same race class as Bug 11; tracked under v0.2.x Option-A push-revocation protocol.
- **Bug 19 (theoretical, deferred to ideas.md)** — `clone3` seccomp allowlist gap.  Tracked in [`docs/ideas.md`](docs/ideas.md) with concrete reconsider signals.
- **Bug 20 (theoretical, accepted-risk)** — `BudgetTracker::maybe_reset` benign double-reset.
- **v0.1.4 Finding 1 (theoretical)** — claimed TOCTOU in `CapabilityToken::permits` was already closed by Bug 10's v0.1.1 fix (atomic check-and-record via DashMap exclusive shard-lock in `authorize_and_record`).  No action needed.
- **v0.1.4 Finding 3 broader scope** — approval queue has no persistence across daemon restart.  REAL-DESIGN: needs a storage-layer decision before fixing.  v0.1.4 ships the timeout sub-issue (Bug 23); the persistence question is queued.

---

## [0.1.4] — 2026-04-25

Same-day patch closing two real findings from the v0.1.2 self-reflection run that targeted the v0.1.2 source.  Both were triaged via Sonnet sub-agent against current source.

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.1.4> — `aaos_0.1.4-1_amd64.deb`.

### Fixed

- **Bug 22 (high)** — Workspace GC follows symlinks, allowing privilege escalation.  An agent with a legitimate `FileWrite: /var/lib/aaos/workspace/*` capability could plant a symlink at the workspace root pointing to `/etc/cron.d`, `/etc/sudoers.d`, or any `aaos`-uid-writable path; the next GC tick (`crates/agentd/src/workspace_gc.rs`) saw the symlink via `path.is_dir()` (which follows symlinks), passed it to `remove_dir_all` (also follows), and deleted the target under the `aaos` uid.  Real privilege boundary violation: agent capability scoped to workspace, GC elevates to arbitrary filesystem deletion.  **Fix**: use `DirEntry::file_type()` (does NOT follow symlinks, unlike `path.is_dir()`) and explicitly reject `ft.is_symlink()` before considering an entry for removal.  Commit `5ac9479`.

- **Bug 23 (medium)** — Approval queue requests have no timeout.  `crates/agentd/src/approval.rs:112` awaited the response oneshot with no upper bound; if no operator responded, the agent blocked forever and the pending `DashMap` entry leaked across daemon lifetime (resource leak on operator absence).  **Fix**: wrap `rx.await` in `tokio::time::timeout(DEFAULT_APPROVAL_TIMEOUT = 1h)`.  On timeout the entry is removed, a warn is logged, and the call returns `ApprovalResult::Denied` with a timeout reason.  Commit `5ac9479`.

---

## [0.1.3] — 2026-04-25

Same-day patch closing Bug 21, surfaced by the v0.1.2 self-reflection run that verified the Bug 13 + Bug 17 fixes.  Plus triage of Bugs 18/19/20 (all theoretical, no fixes needed) and a new `docs/ideas.md` entry for the `clone3` seccomp tightening.

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.1.3> — `aaos_0.1.3-1_amd64.deb`.

### Fixed

- **Bug 21 (medium)** — Missing `CapabilityRevoked` audit events during agent shutdown.  `crates/aaos-runtime/src/registry.rs::remove_agent` (line 138) called `capability_registry.revoke_all_for_agent(id)` directly, bypassing the public `revoke_all_capabilities()` wrapper (line 408) which is the only path that emits the `CapabilityRevoked` audit event.  Result: every agent's `CapabilityGranted` events at spawn-time had no matching `CapabilityRevoked` events at shutdown — audit trail incomplete for security forensics.  Fix: route `remove_agent` through `revoke_all_capabilities()`.  Also tightened `revoke_all_capabilities` itself: replaced the dead `for i in 0..count { let _ = i; }` placeholder loop with a single bulk audit event whose `capability` string carries the count.  Surfaced by aaOS reading its own source on v0.1.2.  Commit `7d8db0f`.

### Documentation

- **`docs/ideas.md`** — added "Tighten `clone3` seccomp filter to `CLONE_THREAD` only" with concrete reconsider signals (third-party audit recommendation, M1 Debian-derivative milestone, or a demonstrated escape).

---

## [0.1.2] — 2026-04-25

Same-day patch closing two bugs uncovered while verifying v0.1.1.  Bug 13 (agent-stop race) had been queued from yesterday's v0.1.0 run; Bug 17 (workspace path mismatch) was surfaced by the same run that verified Bug 13's fix.  Full reflection: [`docs/reflection/2026-04-25-v0.1.2-bug-13-and-17.md`](docs/reflection/2026-04-25-v0.1.2-bug-13-and-17.md).

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.1.2> — `aaos_0.1.2-1_amd64.deb`.

### Fixed

- **Bug 13 (high)** — Agent stop races with in-flight tool invocation.  When the streaming JSON-RPC client disconnected (Ctrl-C from CLI, broken pipe, broadcast channel closed), `crates/agentd/src/server.rs` immediately called `exec_task.abort()`.  Tokio cancellation propagated inward to the nearest `.await` — which is `invoke_tool(...).await` inside the executor's ToolUse arm.  The future was dropped, the scopeguard fired `stop_sync(agent)`, the in-flight `file_write`/`git_commit` side-effect was lost.  Visible failure: missing output file.  Invisible failure (more dangerous): tool with side-effects executes but agent is stopped before recording the audit event.  **Fix**: 500 ms drain window via `tokio::time::timeout(&mut exec_task)` at all four `exec_task.abort()` sites (plan + direct branches, write-failure + RecvError::Closed cases) so pending tool invocations complete before cancellation.  Also added a `tracing::warn!` to `race_deadline` in `crates/aaos-runtime/src/plan/executor.rs` so TTL-triggered drops are visible in journald (same drop-mid-tool-call mechanism, just triggered by wall-clock instead of disconnect).  Diagnosis took one Sonnet sub-agent call; verified end-to-end on a fresh-clone droplet — a 10.9 KB self-reflection report landed on disk for the first time.  Commit `34b018e`.

- **Bug 17 (medium-high)** — `inline_direct_plan` hardcoded the workspace path, ignoring operator-stated output paths.  The Direct orchestration path (`--orchestration persistent`) constructed a 1-node Plan with `workspace: "{run}/output.md"` always set.  The generalist's system_prompt at `packaging/roles/generalist.yaml` prioritises the workspace param over the goal text — so when the operator's goal said "write to /data/findings.md," the LLM dutifully wrote to the workspace path instead.  Operator never saw the file at the path they asked for.  Same silent-misdelivery class as Bug 9 was, just at a different layer.  Concretely: the v0.1.2 self-reflection run wrote a 10.9 KB findings report to `/var/lib/aaos/workspace/<run-id>/output.md` instead of `/data/findings.md`.  **Fix**: omit the workspace param entirely from `inline_direct_plan`; the generalist's "if no workspace, follow the task description" fallback path then triggers and the LLM writes to whatever path the goal text named.  Tightened the EXECUTION CONTRACT block to explicitly say "the operator-specified path."  Risk if the LLM picks a path the generalist's caps don't cover: a clean capability-denied error rather than silent misdelivery — the better failure mode.  Commit `77bbe9d`.

- **Bug 14 (escalated, narrowed)** — Empty `tool_uses` with `stop_reason=ToolUse` now counts as an `EndTurn` for commit-nudge purposes.  DeepSeek (v3/v4) emits `stop_reason=ToolUse` even when the response contains zero `tool_use` blocks (thought-only text).  The existing `EndTurn`-arm nudge never fired for these.  Fix: when `tool_uses` is empty AND commit_nudges remain, inject the same nudge user-message and loop; once nudges exhausted, accept as `Complete`.  This is what made multi-turn bug-hunt runs actually commit findings on v0.1.2 — without it, the LLM would emit thought-only text under stop_reason=ToolUse and the executor would loop until token budget exhausted with no file_write call.  Bug 14 was previously informational; this v0.1.2 fix promotes it to an active failure mode that's now closed.  Commit `5dd0e09`.

- **Default `ExecutorConfig.max_total_tokens` raised 1M → 5M.**  Multi-turn investigation agents accumulate ~50-100k tokens per turn (full message history re-sent each call).  20-iteration runs routinely hit 1M on v4-priced runs and silently returned `MaxTokens`.  5M gives ~50-turn headroom; cost is unaffected (charged per-API-call, not per-config-value).  Also added a `tracing::warn!` log for the budget-exhaustion path (was silent) and a `tracing::info!` at the loop top for diagnosing stuck runs.  Commit `5dd0e09`.

### Known issues (fixed in 0.1.2)

The v0.1.1 release shipped with Bug 13 still open; that's now closed.

---

## [0.1.1] — 2026-04-25

Patch release closing 5 production bugs surfaced by the v0.1.0 self-reflection run and a parallel senior-engineer audit.  No new features; no API or wire-protocol changes.  Full report: [`docs/reflection/2026-04-25-v0.1.0-first-real-findings.md`](docs/reflection/2026-04-25-v0.1.0-first-real-findings.md).

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.1.1> — `aaos_0.1.1-1_amd64.deb`.

### Fixed

- **Bug 12 (medium)** — `glob_matches` separator-boundary check.  `crates/aaos-core/src/capability.rs` checked `canonical.starts_with(&norm_prefix)` without verifying the following byte is a path separator.  Pattern `/data/*` incorrectly accepted `/data-foo/x` and `/data_foo/x`.  Fixed: require that the character immediately after the prefix is absent (exact-dir match) or `/`.  Two new regression tests: `glob_boundary_dash_prefix_denied` and `glob_boundary_underscore_prefix_denied`.

- **Bug 15 (medium-high)** — `pending_responses` RAII cleanup.  `crates/aaos-runtime/src/services.rs` registered a oneshot sender before `route()` and did not clean it up on route error or timeout.  Every timed-out or routed-to-dead-agent `send_and_wait` permanently leaked a `DashMap` entry; `pending_count()` grew monotonically.  Fixed: added `MessageRouter::cancel_pending` + a RAII `PendingGuard` inside `send_and_wait` that removes the entry on any early return.  New regression test: `send_and_wait_timeout_cleans_up_pending` asserts `pending_count() == 0` after a timeout.

- **Bug 16 (medium)** — `SqliteMemoryStore::store` explicit transaction.  `crates/aaos-memory/src/sqlite.rs` ran DELETE then INSERT as separate auto-commits.  A failed INSERT left the old record permanently deleted.  Fixed: wrapped both statements in `conn.transaction()` + `tx.commit()`.  Existing `replaces_is_atomic` test continues to pass.

- **Bug 10 (high)** — `max_invocations` now enforced at the `ToolInvocation` layer.  `crates/aaos-tools/src/invocation.rs` called `permits()` (read-only) but never `authorize_and_record()`.  Capability `max_invocations` constraints were dead code — an agent could invoke any tool unlimited times regardless.  Fixed: replaced `any()` scan with `find()` to retain the matching handle, then calls `authorize_and_record` after a successful tool execution.  If the token is revoked or expired in the window between the two calls, a warning is logged and the already-completed invocation is not failed (can't undo).  New test: `max_invocations_enforced_through_invoke`.

- **Bug 11 (narrowed, not closed)** — Revoked and expired tokens filtered before forwarding to workers.  `crates/aaos-core/src/capability_registry.rs::resolve_tokens` previously forwarded all tokens regardless of revocation status; workers received and honoured revoked tokens in their per-call registry.  Fixed: filter out `is_revoked() || is_expired()` tokens in `resolve_tokens` so workers only receive currently-valid tokens at dispatch time.  **Residual race:** a token revoked *after* `resolve_tokens` runs but *before* the worker invokes the tool is still honoured by the in-flight call.  Closing this fully requires a push-revocation protocol (Option A) — queued for v0.2.x.  New test: `resolve_tokens_filters_revoked`.

### Test count

625 (v0.1.0) → 631 (+6 new regression tests across `aaos-core`, `aaos-ipc`/`aaos-runtime`, and `aaos-tools`).

---

## [0.1.0] — 2026-04-24

Architectural release.  Unifies both orchestration paths (plan/decompose and persistent/direct) through the PlanExecutor.  Each subtask now runs as a full multi-turn agent with a role-configurable iteration budget.  Bug 9 (hallucinated fallback reports) is closed by deleting the fallback path.

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.1.0> — `aaos_0.1.0-1_amd64.deb`.

### Added

- **`role.orchestration.max_iterations`** — optional `orchestration:` block in role YAML sets the per-subtask multi-turn iteration budget.  Default 50 if absent.  Replaces the old `retry.max_attempts + 10` formula (floor 10).  Bundled role values: `fetcher` 10, `writer` 30, `analyzer` 30, `generalist` 50, `builder` 50.
- **`role.require_declared_output`** — optional boolean (default `false`).  When `true`, a subtask that finishes without writing its declared `file_write` output is a hard failure, not an advisory.  `fetcher` sets this to `true`.
- **`SubtaskOutputStatus` enum** — `check_declared_outputs_exist` now returns `Present`, `MissingAdvisory(String)`, or `MissingFatal(String)`.  Advisory path emits `AuditEventKind::SubtaskOutputMissing` and continues as success; fatal propagates as a subtask failure.
- **`AuditEventKind::SubtaskOutputMissing { subtask_id, declared_path }`** — advisory audit event emitted when a subtask's declared output file is absent and `require_declared_output: false`.
- **`PlanExecutor::run_with_plan(initial_plan, goal, run_id)`** — new method that starts from a pre-built `Plan` and skips the Planner call entirely.  Used by the Direct path.
- **`inline_direct_plan(goal, run_id)`** — server-side function that builds a 1-node generalist `Plan` for the Direct orchestration path.

### Changed

- **Both orchestration modes now route through PlanExecutor.**  `plan` (now `decompose`) calls `PlanExecutor::run()` as before.  `persistent` (now `direct`) calls `PlanExecutor::run_with_plan()` with a 1-node inline plan — the Bootstrap persistent agent is no longer used for per-submit work.
- **Classifier output changed from `plan`/`persistent` to `decompose`/`direct`.**  New `DecompositionMode` enum in `orchestration_classifier.rs`.  Classifier prompt updated: asks whether the goal has independent parallelisable subtasks.  Fallback on LLM error changed from `direct` (was `plan`).  Wire API (`--orchestration plan|persistent`) preserved; `plan → Decompose`, `persistent → Direct`.
- **Subtask iteration budget now reads from `role.orchestration.max_iterations`** instead of `retry.max_attempts + 10`.  Old default was ~12; new default is 50.  Open-ended goals benefit most: a single-subtask direct run now has 50 turns instead of 12.
- **`NoopOrchestrationClassifier` now returns `Direct`** (was `Plan`).  When no LLM client is configured, the daemon routes all submissions to the generalist single-agent path rather than attempting a Planner call that would immediately fail.
- **Architecture docs updated** — "Orchestration modes" section rewritten to reflect the unified PlanExecutor path, new role YAML fields, `SubtaskOutputStatus`, and `fallback_generalist_plan` removal.

### Removed

- **`fallback_generalist_plan`** function in `executor.rs` — closes Bug 9.  A malformed Planner response now propagates as `ExecutorError::Correctable`; the replan loop handles retries; after `max_replans` the run fails cleanly with no hallucinated report.  The `PlannerError::Malformed → fallback_generalist_plan` arm in `PlanExecutor::run()` is gone.
- **Bootstrap streaming path in `server.rs`** — `handle_submit_streaming` no longer has a Bootstrap arm.  `ensure_bootstrap_running`, `route_goal_to`, `event_in_subtree` helper methods deleted.  `submit_streaming_writes_events_then_end_frame` integration test (Bootstrap-specific) deleted; replaced by the routing tests added in v0.0.5.
- **Bug 9 from the Known issues list** — the fallback-generalist hallucination path is structurally impossible in v0.1.0.  See `docs/reflection/2026-04-24-v0.0.3-self-reflection.md` for the closure write-up.

### Fixed

- Test count: 613 → 625 workspace-wide.  Net gain despite deleting the Bootstrap streaming integration test (`submit_streaming_writes_events_then_end_frame`) — that test was replaced by more precise unit tests for the new routing logic, plus new tests for `SubtaskOutputStatus`, `RoleOrchestration`, `run_with_plan`, and classifier behaviour under `decompose`/`direct` labels.

---

## [0.0.5] — 2026-04-24

Third same-day release.  Adds per-submit orchestration routing with LLM-driven auto-detection as the default — `agentd submit` no longer forces every goal through the Planner + PlanExecutor DAG path.  Structured goals still take the DAG path; open-ended exploration / investigation goals route to a persistent Bootstrap agent that manages its own multi-turn context.

Surfaced as a direct response to the v0.0.3 and v0.0.4 self-reflection droplet runs, which exposed the computed-orchestration path as architecturally unsuited to bug-hunting-class goals (per-subtask LLMs are single-shot with capped iteration budgets; they exhaust the budget exploring and never commit).  The Bootstrap persistent path still existed in the codebase but was only reachable by deleting the role catalog, an all-or-nothing switch.  v0.0.5 makes it a per-submit choice, default auto-detected.

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.0.5> — `aaos_0.0.5-1_amd64.deb`.

### Added

- **Auto-routing: `agentd submit` now classifies the goal and picks `plan` or `persistent` automatically.** A cheap single-shot LLM call (~50 input / 1 output token, routes through the configured provider — DeepSeek `deepseek-chat` or Anthropic) inspects the goal text before any agent work begins and routes accordingly.  Classifier prompt is terse and asks for a single-word response; response parsing is forgiving (substring match on `plan` / `persistent`).
  - `plan` — Planner + PlanExecutor DAG.  Best for structured goals with declared outputs per subtask (fetch, analyse, write).  Requires a loaded role catalog; returns a clear error if the catalog is absent.
  - `persistent` — Bootstrap persistent agent.  Best for open-ended, exploratory, or long-context goals where a single multi-turn agent manages its own context and spawns children as needed.
  - Classifier falls back to `plan` on any LLM error or unparseable response.  When no LLM client is configured, auto-routes to `plan` immediately (no network call, no hanging).
  - **Override available**: `agentd submit --orchestration [plan|persistent] "<goal>"` bypasses the classifier.  Explicit wins.
  - **Audit visible**: an `OrchestrationSelected { mode, source }` audit event fires on every submit so operators can see which path was chosen and why (`source: "explicit"` or `"auto"`).  A `tracing::info!` log line `orchestration mode selected mode=<Plan|Persistent> source=<auto|explicit>` also lands in journald.
  - **JSON-RPC surface**: the `agent.submit_streaming` method accepts an optional `"orchestration"` field in its params.  Present → explicit; absent → classified.  Clients built against older servers that always defaulted to plan continue to work (they just don't get classification).
- **Per-submit routing gate in `server.rs`** replaces the startup-time `if let Some(executor)` all-or-nothing gate.  Plan mode errors cleanly when no role catalog is loaded instead of silently falling through to Bootstrap.

Commits: `1beaf22` (CLI flag), `a9bbfe2` (routing gate), `976aa95` (initial docs), `5dc20fd` (classifier module + tests), `4ddc959` (classifier wiring), `e1c3d73` (auto-detect docs).

### Changed

- Test count: 592 → 613 across the workspace.  +21 net: 12 new classifier unit tests, 4 new CLI tests, 5 new / updated server routing tests.

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

[Unreleased]: https://github.com/Joncik91/aaOS/compare/v0.1.5...HEAD
[0.1.5]: https://github.com/Joncik91/aaOS/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/Joncik91/aaOS/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/Joncik91/aaOS/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/Joncik91/aaOS/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/Joncik91/aaOS/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Joncik91/aaOS/releases/tag/v0.1.0
[0.0.5]: https://github.com/Joncik91/aaOS/releases/tag/v0.0.5
[0.0.4]: https://github.com/Joncik91/aaOS/releases/tag/v0.0.4
[0.0.3]: https://github.com/Joncik91/aaOS/releases/tag/v0.0.3
[0.0.2]: https://github.com/Joncik91/aaOS/releases/tag/v0.0.2
[0.0.1]: https://github.com/Joncik91/aaOS/releases/tag/v0.0.1
[0.0.0]: https://github.com/Joncik91/aaOS/commits/779dd62
