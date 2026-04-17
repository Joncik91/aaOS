# Self-Reflection Log

A chronological record of aaOS reading its own code, finding bugs, proposing features, and having those results reviewed and sometimes shipped. Each entry is verified against git commits and observed behavior; where a number was estimated at the time and later corrected, both the original and the correction are recorded.

Each run or prep entry lives in its own file, dated `YYYY-MM-DD-<slug>.md`. New entries are added as new files, not appended to a monolith. For the build history that preceded this log, see [`../retrospective.md`](../retrospective.md). For cross-cutting lessons, see [`../patterns.md`](../patterns.md).

On cost figures: see [`cost-bookkeeping.md`](cost-bookkeeping.md) for the authoritative framing (dashboard > token math) and the running cumulative total.

## Entries

Chronological, oldest first:

- [`2026-04-13-run-1-security-self-audit.md`](2026-04-13-run-1-security-self-audit.md) — Run 1: first time the runtime read its own source; found 4 real vulnerabilities including a Phase-A path traversal bug. Integration `82d19e9`.
- [`2026-04-13-run-2-capability-revocation.md`](2026-04-13-run-2-capability-revocation.md) — Run 2: system proposed and drafted capability revocation; shipped as `f1732d9`.
- [`2026-04-13-run-3-constraint-enforcement.md`](2026-04-13-run-3-constraint-enforcement.md) — Run 3: `max_invocations` was decorative; the system noticed and we shipped enforcement in `f106d97`.
- [`2026-04-13-interlude-skill-loading-bug.md`](2026-04-13-interlude-skill-loading-bug.md) — Skill-loading bug observed in runs 2-3: Bootstrap was naming children after skills without calling `skill_read`. Manifest-only fix in `66542bf`.
- [`2026-04-14-run-4-meta-cognitive-coordinator.md`](2026-04-14-run-4-meta-cognitive-coordinator.md) — Run 4: first skill-driven run; proposed a Meta-Cognitive Coordinator. Shipped as a minimal version (`file_list` + stable Bootstrap ID + opt-in persistent memory + manifest protocol) after peer review.
- [`2026-04-14-run-5-first-persistent-memory.md`](2026-04-14-run-5-first-persistent-memory.md) — Run 5: first end-to-end persistent-memory run. Three manifest-only tuning fixes; exposed child-memory-orphaning and the JS+Python over-build.
- [`2026-04-14-run-6-kernel-gated-handoff.md`](2026-04-14-run-6-kernel-gated-handoff.md) — Run 6: two kernel-level gaps surfaced and fixed — stable-identity gate on private memory (`505f559`) and structured `prior_findings` handoff (`5feedbe`).
- [`2026-04-14-run-7-kernel-fixes-validated.md`](2026-04-14-run-7-kernel-fixes-validated.md) — Run 7/7b: validated Run 6's kernel fixes under live traffic; no new code shipped. Process lessons on `--no-cache` builds and workspace export before `docker rm`.
- [`2026-04-14-run-7-followup-error-handling.md`](2026-04-14-run-7-followup-error-handling.md) — Run 7 follow-up: acted minimally on the error-handling proposal. `MemoryResult2` renamed; `ContextSummarizationFailed` audit event actually fires now (`ba0904a`, `51db7b5`).
- [`2026-04-14-run-7-followup-phase1-speed.md`](2026-04-14-run-7-followup-phase1-speed.md) — Run 7 follow-up: Phase 1 speed work — `file_read_many` batch tool, chain trim, output scoping. Shipped as `5be74ac`.
- [`2026-04-14-run-8-phase1-measured.md`](2026-04-14-run-8-phase1-measured.md) — Run 8: measured Phase 1 speed work — ~50% reduction, beating the 35-45% target. Peer-review emergence pattern first observed.
- [`2026-04-14-run-9-adversarial-bug-hunt.md`](2026-04-14-run-9-adversarial-bug-hunt.md) — Run 9: adversarial bug-hunt prompt found seven real bugs including the symlink bypass of the Phase-A path-traversal fix. Eight commits, five Copilot-pushback revisions.
- [`2026-04-14-run-10-persistent-memory.md`](2026-04-14-run-10-persistent-memory.md) — Run 10: persistent memory carried forward from Run 9; found a `spawn_with_tokens` gap in Run 9's Fix 1. Crossed $1.00 cumulative dashboard spend across ten runs.
- [`2026-04-14-run-11-prep-docs-masking-parallelism.md`](2026-04-14-run-11-prep-docs-masking-parallelism.md) — Run 11 prep: docs masking at container launch + `spawn_agents` batch tool (three Copilot review rounds, best-effort semantics). Commits `73b3653`, `04dc0c7`.
- [`2026-04-15-namespaced-backend-and-droplet-prep.md`](2026-04-15-namespaced-backend-and-droplet-prep.md) — Handle-token migration (`14a8eae`/`18d14f0`/`3c82f6e`) and namespaced-backend scaffolding + finish (`a84cd98`/`a73e062`/`8a70a1a` scaffold, `1d6ec97`/`67c7fc3` kernel launch mechanics). Four Copilot rounds on the plan; isolated cloud dev VM for the clone+exec bring-up. End-to-end confirmed on Debian 13 / kernel 6.12.43: `Seccomp: 2` + `NoNewPrivs: 1` in `/proc/<pid>/status`; 4 integration tests green under `--ignored`.
- [`2026-04-15-phase-f-a-deb-package.md`](2026-04-15-phase-f-a-deb-package.md) — Phase F-a shipped: `agentd` as a Debian `.deb`, built via `cargo-deb` (metadata on the `agentd` crate, no hand-maintained `debian/` tree). One Copilot review round caught six substantive items (socket path under `RuntimeDirectory=`, dropped `curl/jq` deps, `ProtectSystem=full` not `strict`, `postinst` → `StateDirectory=` only, `postrm` no silent errors, Debian 13 build container). Release-build breakage surfaced: `CapabilityRegistry::inspect` was `cfg(debug_assertions)`-only and two production callers depended on it — fixed with `token_id_of()`. Commits `5717906` + `8d45691`. End-to-end verified on a Debian 13 cloud VM: install → start → socket serves JSON-RPC → purge cleans state + user.
- [`2026-04-16-agentd-cli-shipped.md`](2026-04-16-agentd-cli-shipped.md) — Operator CLI shipped: five subcommands (`submit`, `list`, `status`, `stop`, `logs`) + server-side NDJSON streaming (`agent.submit_streaming`, `agent.logs_streaming`) + `BroadcastAuditLog` + explicit `aaos` system group + `agentd(1)` man page. 18-task plan, subagent-driven implementation. End-to-end verified on a Debian 13 cloud VM with a non-root operator in the `aaos` group running two real DeepSeek-backed goals (first spawned Bootstrap in 5s, second reused it in 3s). Caught a socket-permissions bug that only surfaces under non-root group members — `UnixListener::bind` inherits the process umask, so an explicit `chmod 0660` is needed after bind. Commits `58dd1bb` through `5e01acc`.
- [`2026-04-16-computed-orchestration-shipped.md`](2026-04-16-computed-orchestration-shipped.md) — Computed orchestration shipped: two-phase boot replacing Bootstrap-as-LLM-orchestrator. A cheap-LLM Planner emits a structured JSON Plan; a deterministic Rust `PlanExecutor` walks the subtask DAG via role-based scaffold spawns, running independent subtasks in parallel batches via `futures::try_join_all`. 17 commits (`9b001cb` through `cbd3dc7`), 126 new unit tests, subagent-driven with model-per-task-complexity (haiku/sonnet/opus). End-to-end verified with a real DeepSeek submit of "fetch HN and lobste.rs, compare top 3, write to /data/compare.md" — planner produced the expected 5-subtask DAG with 2 parallel fetchers. Three planner-prompt bugs surfaced (workspace-as-directory, operator-path routed through `{run}`, over-decomposition); none require runtime changes, all are next-commit prompt-tuning work. Architectural claim verified: orchestration is now code, not improvisation.
- [`2026-04-17-role-budget-wiring.md`](2026-04-17-role-budget-wiring.md) — Four benchmark runs across the day walked the canonical HN+lobste.rs goal from 5m30s (baseline) to **28s** (12× improvement). Root cause of the fetcher stall traced via Explore subagent: `Role::render_manifest` silently dropped `budget`; `execute_agent_for_subtask` used `ExecutorConfig::default()`. Fixed with `SubtaskExecutorOverrides` threading + tightened role prompts (`ef45e61`, `c412a14`). Writer's "ERROR: missing input <path>" contract fired correctly — the silent-quality-failure mode is closed. New failure mode surfaced: fetcher LLM emits plausible-path acks **without calling file_write**, proving prompt-only contracts can't enforce tool-call side effects. Next: deterministic fetcher scaffold skipping the LLM entirely for mechanical I/O.
- [`2026-04-17-replan-on-subtask-failure.md`](2026-04-17-replan-on-subtask-failure.md) — First observe→replan primitive. `PlanExecutor::execute_plan` now drains the batch with `join_all`, audits `SubtaskCompleted{success:true|false}` per subtask, and returns `Correctable("subtask '<id>' (role '<role>') failed: <err>")` on the first failure — the outer `run()` loop replans with that context. Replan prompt tightened to instruct diagnosis and non-repetition. Verified in Docker: failure-dominated goal bounds cleanly at `max_replans=3` (26s); recoverable goal pivots the fetcher URL on the first replan and completes in 82s with real HN content. Commit `54cf501`; 4 new executor tests, 138 runtime tests green.
- [`2026-04-17-namespaced-re-verification.md`](2026-04-17-namespaced-re-verification.md) — Re-verified `NamespacedBackend` on a fresh DO droplet (Debian 13, kernel 6.12.43) against current `main` (`3e1b207`). All 4 `--ignored` integration tests green; live worker's `/proc/<pid>/status` shows `NoNewPrivs:1 / Seccomp:2 / Seccomp_filters:2`. No regression across the ~20 commits since the 2026-04-15 baseline. Fixed stale `architecture.md` drift that still said "not yet functional." Phase F-b now unblocked.
- [`2026-04-17-secret-isolation.md`](2026-04-17-secret-isolation.md) — Close API-key exfiltration paths before F-b. Three changes: `/etc/default/aaos` tightened to `0600 root:root` (postinst enforces on upgrade), `agentd` scrubs the key from its own process environment at startup (libc `getenv` + byte-zero + `remove_var` — closes `/proc/<pid>/environ` read), man page documents the contract. Verified end-to-end in a fresh Docker container: `aaos`-group members can no longer read the env file; `sk-...` bytes no longer present in the daemon's `/proc/environ`; daemon still calls DeepSeek successfully. Commit `37590a0`+pending.

## Supporting

- [`cost-bookkeeping.md`](cost-bookkeeping.md) — dashboard vs token-math framing for all cost figures quoted in run entries.

## How to add a new entry

Create a new file `YYYY-MM-DD-<short-slug>.md` in this directory using the template below, then add a one-line summary to the list above.

```markdown
# Run N — <short name> *(YYYY-MM-DD)*

**Integration commits:** `<hash>` "<message>" (HH:MM), ...

## Setup
- Memory state: fresh / carried over from run N-1 / partial
- Philosophical / specific goal
- Notable config (AAOS_PERSISTENT_MEMORY, AAOS_RESET_MEMORY, etc.)

## What Worked
- ...

## What the Run Exposed
- ...

## What Shipped
- ...

## Cost
- Dashboard-authoritative figure if known, else note "[token-math estimate]"
```

New lessons that generalize across runs should be lifted into [`../patterns.md`](../patterns.md) rather than repeated in each entry.
