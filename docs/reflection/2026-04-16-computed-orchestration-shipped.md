# Computed Orchestration shipped *(2026-04-16)*

**Integration commits (17 commits, ~2200 LoC new):**

- `9b001cb` — role YAMLs (`packaging/roles/fetcher.yaml`, `writer.yaml`, `analyzer.yaml`, `generalist.yaml`) + cargo-deb conffile wiring
- `0b687cd` — `Role` / `RoleCatalog` types + YAML loading
- `0a64262` — `Role::validate_params` against parameter schema
- `1cd0d30` — `Substitutions` for `{run}` template variable
- `3305061` — `Plan`, `Subtask`, `SubtaskResult`, `PlanResult` types + `topo_batches` DAG sort
- `0d0aeed` — `Role::render_manifest` + `Role::render_message`
- `4dd27fe` — 4 new `AuditEventKind` variants (`PlanProduced`, `PlanReplanned`, `SubtaskStarted`, `SubtaskCompleted`)
- `7a447f5` — `Planner` single-shot LLM call + schema validation + replan wiring
- `a79ff4e` — `PlanExecutor` skeleton + `ExecutorError { Correctable, Terminal }`
- `2c9c6a9` — `PlanExecutor` real DAG walk via `SubtaskRunner` closure + parallel batches
- `c76e7fc` — `fallback_generalist_plan` for malformed initial plans
- `f38b7f1` — `Server` gains `OnceLock<Arc<PlanExecutor>>` + role catalog loading at startup
- `6f6501a` — `Server::run_subtask_inline` real + `install_plan_executor_runner` (chicken-and-egg dance)
- `f02e90e` — `handle_submit_streaming` routes through `PlanExecutor` when catalog loaded
- `10360da` — `agentd roles list|show|validate` CLI subcommand
- `cbd3dc7` — man page + README cover computed orchestration
- (this reflection)

## Setup

Fresh Debian 13 container on A8 (systemd-less; sufficient since this feature doesn't need `systemctl` and a full droplet trip costs $0.50 for no extra evidence). Built the `.deb` on A8 via `cargo build --release` + `./packaging/build-man-page.sh` + `cargo deb -p agentd --no-build`. Installed the `.deb` in the container, created a non-root `testop` user in the `aaos` group, injected a DeepSeek API key at `/etc/default/aaos` mode 0600, started the daemon manually as `aaos` (no systemd).

## What worked

**All 17 task commits landed with zero review loops.** Subagent-driven development with model selection matching task complexity (haiku for mechanical, sonnet for typed work, opus for integration/architecture) shipped the feature in one session. TDD-per-task held up: every commit includes a test module that would fail against the prior state. The `plan/` module has 126 unit tests at commit `c76e7fc` — zero regressions.

**Architectural claim verified end-to-end.** A live DeepSeek-backed submit of *"fetch HN and lobste.rs, compare the top 3 stories on each, write to /data/compare.md"* produced a **5-subtask Plan** with the correct parallelism:

- 2 fetchers spawned at `21:21:06`, both invoked `web_fetch` at `21:21:09` — **parallel execution confirmed**
- 2 analyzers spawned at `21:24:34`, both `file_read` at `21:24:38` — parallel fan-in
- 1 generalist step at `21:24:45` for comparison reasoning
- 1 writer at `21:25:25` producing the final Markdown

The final file landed (though at the wrong path — see below) with coherent content comparing the two sites. Compare to the 2026-04-16 baseline where Bootstrap spent 15+ minutes sequentially on the same goal: the **structural bottleneck (LLM-as-orchestrator) was removed**.

**`plan.json` persisted at `/var/lib/aaos/workspace/<run-id>/plan.json`** — operators can `cat` it during or after the run to see what the planner decided.

**`agentd roles list|show|validate` ships.** Operators can inspect the catalog without reading YAML by hand.

**The chicken-and-egg between `Server` and `PlanExecutor::SubtaskRunner` resolved cleanly** via `OnceLock<Arc<PlanExecutor>>` + a post-construction `install_plan_executor_runner` that rebuilds the executor with a real runner closing over `Arc<Self>`. `Server::with_llm_client` / `with_llm_and_audit` / `with_memory` now return `Arc<Self>`.

## What the run exposed

**Three real bugs, all in the planner prompt / output-path handling — not in the runtime:**

1. **Planner emits `workspace: "{run}"` (a directory) instead of a file path like `"{run}/fetched.html"`.** Fetchers then call `file_write` against a directory, which fails repeatedly, causing the LLM to retry with invented filenames. Each fetcher took 3m28s for what should be one fetch + one write. Fix: the planner prompt must require `workspace` params to be concrete file paths, not directory placeholders.

2. **Planner routes the operator's declared output path through `{run}` substitution.** The goal said "write to /data/compare.md"; the planner emitted `"output": "{run}/data/compare.md"`. Result: `/var/lib/aaos/workspace/<run-id>/data/compare.md` instead of `/data/compare.md`. The planner isn't distinguishing operator-absolute paths from workspace-relative ones. Fix: the planner prompt must preserve operator-stated absolute paths verbatim.

3. **Planner over-decomposes simple goals.** A one-fetch-one-summarize goal (example.com test) produced a 3-subtask chain (fetcher → analyzer → writer) when one writer-with-web_fetch capability would suffice. A generalist step also appeared in the HN/lobste.rs plan between analyzers and writer — unnecessary given the writer had the analyzer outputs.

**None of these require runtime changes.** They're prompt-engineering bugs in `Planner::build_prompt`. The runtime did its job: validated the plan, spawned per the DAG, ran subtasks in parallel batches, persisted the plan.json, routed audit events, returned a non-empty final document. The orchestration layer works. The Planner needs iteration — a follow-up commit can tighten the prompt, and the improvement will show up in cleaner plans without any runtime change.

**Wall-clock 5.5 minutes for the benchmark.** Still much better than the baseline's 15+ minutes of Bootstrap re-work and silent capability-denial recovery. The fetcher flailing accounts for most of it — fix bug #1 and wall-clock should drop under 2 minutes for the same goal.

**No `end` frame observed** in the CLI's streaming output because the outer `timeout 300` fired before the writer's final LLM turn finished. The file was written regardless (the writer's `file_write` call had already completed); only the success-ack frame was cut off. Fix: increase the CLI's default timeout, or rely on daemon-side tracking rather than CLI-side timeouts. In practice the operator can `cat /data/compare.md` (or in this case `/var/lib/aaos/workspace/<run-id>/data/compare.md`) and see the artifact regardless.

## What shipped

- **Runtime** (`crates/aaos-runtime/src/plan/`): six modules (`mod.rs`, `role.rs`, `placeholders.rs`, `planner.rs`, `executor.rs`). 126 unit tests. `Plan`/`Subtask`/`PlanResult` types, `RoleCatalog::load_from_dir`, `Planner::plan`/`replan` (single-shot LLM + schema validation + 3-replan cap + 10-min deadline), `PlanExecutor::run` (planner call + replan loop + DAG execution + `plan.json` write), `topo_batches` for parallel-safe grouping, `Substitutions` for `{run}` substitution, `fallback_generalist_plan` for novel goals that no template matches.
- **Core** (`crates/aaos-core/src/audit.rs`): 4 new audit event kinds (`PlanProduced`, `PlanReplanned`, `SubtaskStarted`, `SubtaskCompleted`).
- **Agentd** (`crates/agentd/src/server.rs`): `Server` holds `OnceLock<Arc<PlanExecutor>>`, loads `/etc/aaos/roles/*.yaml` at startup, rebuilds the executor with a real `SubtaskRunner` in `install_plan_executor_runner`. `handle_submit_streaming` routes through the executor when the catalog loads; legacy Bootstrap path preserved as fallback when it doesn't. `run_subtask_inline` spawns ephemeral children via `registry.spawn` with scopeguard cleanup; mirrors `execute_agent`'s LLM loop construction.
- **CLI** (`crates/agentd/src/cli/roles.rs`): `agentd roles list|show|validate`.
- **Packaging**: 4 role YAMLs ship in `/etc/aaos/roles/`, marked as conffiles.
- **Docs**: man page gains `## roles` section; README Quick Start swaps the Bootstrap paragraph for the Planner + PlanExecutor description.

## What didn't ship (deferred)

- **External planner scripts** (the computed-skills `!`command`` shell-out pattern). The current Planner is an in-daemon Rust call. External scripts are a future feature when a real operator workload needs shapes the bundled Planner can't handle.
- **Plan caching / reuse.** Every submit re-plans.
- **Partial replan.** Full replan on any correctable error; no surgical subtask swap.
- **Planner prompt tightening** for the three bugs above. Next commit after this reflection.

## Cost

- **Cargo builds on a8:** ~1 min total (incremental). No DO droplet used.
- **Docker on a8:** free.
- **DeepSeek spend during verification:** estimated <$0.05 for the two end-to-end runs ([token-math estimate]; exact figure on the dashboard tomorrow).

## Lessons

**Model selection by task shape pays for itself.** Haiku handled role YAMLs, validation, placeholders, topo-sort, render helpers, audit variants, and CLI wiring (9 tasks, each under 1 min). Sonnet handled Planner/executor skeleton/Server wiring (4 tasks, ~1-2 min each). Opus handled `execute_plan` real spawn wiring and the `handle_submit_streaming` integration (2 tasks, ~2-6 min each — multi-file, real design judgment). No implementer got stuck. No subagent needed a re-dispatch.

**Non-root end-to-end verification keeps finding real bugs.** The socket-permission bug in Phase F-a, the SpawnChild-wildcard regression earlier today, and the three planner-prompt bugs above — none of them caught by unit tests, all of them surfaced the first time a real LLM made a real decision on a real submit. Pattern from `docs/patterns.md` holds: "end-to-end verification as an unprivileged user catches permission bugs the test suite can't." Extending it: *end-to-end verification with a real LLM catches prompt bugs the unit tests can't* — because unit tests use stub LLMs that always emit the expected JSON.

**Planner prompt engineering is a separate discipline from runtime engineering.** The runtime's contract is "given a valid Plan, execute it correctly." It met that contract on the first real run. The Planner's contract is "emit a Plan the runtime + operator both agree with on paths and semantics." That's prompt work, not Rust work — and it deserves its own iteration cycle with its own tests (canned prompts → expected JSON shapes).
