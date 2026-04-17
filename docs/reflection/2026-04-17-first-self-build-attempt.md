# First self-build attempt — cargo_run + builder role *(2026-04-17)*

**Integration commits:** `45ce06b` "feat: cargo_run tool + builder role for self-hosted build loops", `755149e` "docs: note cargo_run + self-hosted build loop"

## Setup

- Fresh DO droplet, Debian 13 / kernel 6.12.43 / 4 vCPU / 8 GB RAM.
- `agentd` release-built on droplet in 3m14s; binary + the full role catalog (including the new `builder.yaml`) installed under `/etc/aaos/`.
- DeepSeek API key pasted into `/etc/default/aaos` (0600 root:root).
- Plan: the wise-twirling-prism plan from 2026-04-17 targeting the fetcher/writer/analyzer bugs, copied to `/root/aaOS/plan.md`.
- Goal: `"Apply the plan at /root/aaOS/plan.md to the pre-existing Rust workspace at /root/aaOS... use the builder role..."`

Four runs total. Only run 4 was a genuine self-build attempt; runs 1–3 surfaced setup bugs.

## What the first three runs exposed

1. **Run 1 (~50s): capability-glob syntax mismatch.** Initial `builder.yaml` used `"file_read: {workspace}/**"` (gitignore semantics). aaOS's capability matcher treats a trailing `*` as `starts_with(prefix)` — a single `*` is already recursive — so `/**` expanded to the literal pattern `/root/aaOS/**` which matched nothing. Agent read the plan, tried to read source files, got denied on every one, wrote an honest "SKIPPED" report. Capability denials appeared cleanly in telemetry (args_preview + result_preview from yesterday's work made the failure legible in seconds). **Fix:** single `*` in `builder.yaml`.
2. **Run 2 (~50s): Planner substituted operator-stated paths with per-run scratch dir.** The freeform goal `"apply plan to workspace at /root/aaOS"` caused the Planner to treat `/root/aaOS` as a subtask workspace and rewrote it as `/var/lib/aaos/workspace/<run-id>/builder_workspace`. The Planner prompt already documents an "operator-stated absolute paths stay verbatim" rule — I just didn't invoke it. **Fix:** restated the goal with `"operator-stated absolute path /root/aaOS"` three times.
3. **Run 3 (~131s): iteration budget too small.** With `retry.max_attempts: 1`, the executor computed `max_iterations = (1 + 10).max(10) = 11`. Agent used all 11 turns reading files (1 plan + 4 list + 6 reads) and hit the cap right when it was about to edit. **Fix:** `retry.max_attempts: 30` → `max_iterations = 40`.

## Run 4 — the real attempt

- Runtime: 588 s (~10 min) to "complete".
- Tool-call trace (18 turns): `file_read(plan)` → `file_list` ×3 → `file_read` ×6 (executor.rs, server.rs, 4 role yamls) → `cargo check` (✅) → `cargo test` (❌) → `cargo test --nocapture` → three diagnostic reads → `cargo test -- spawn_and_list` → `file_list /etc/aaos` (denied) → `file_write report.md` → complete.
- **Diff against pre-run snapshot: zero source changes.** The agent concluded the plan was already implemented and wrote that as its report.

That conclusion is correct. The wise-twirling-prism plan was written 2026-04-17 morning; its edits landed in commits `ef45e61` + `c412a14` the same day. I submitted yesterday's plan against a codebase that already contained the fix. The agent read the plan, read the current code, compared them, and reported accurately instead of re-applying dead edits. The "silent quality failure" failure mode (fabricate to satisfy the contract) was closed: the agent declined to pretend.

## What run 4 exposed

**A real test-hygiene bug: `submit_streaming_writes_events_then_end_frame` fails on hosts where a role catalog exists at `/etc/aaos/roles/`.** The test creates a `Server::with_llm_client(...)`, which calls `load_role_catalog()` — that function reads `/etc/aaos/roles/` (or `$AAOS_ROLES_DIR`) directly, without any test-isolation hook. When the catalog loads, `Server::handle_submit_streaming` switches from the Bootstrap code path to the PlanExecutor code path, and the test's assertions about Bootstrap events fail.

Repro:
```
# On A8 (no /etc/aaos):
cargo test -p agentd --lib submit_streaming_writes_events_then_end_frame  # passes
# On the droplet (with /etc/aaos/roles/builder.yaml etc. installed):
cargo test -p agentd --lib submit_streaming_writes_events_then_end_frame  # fails
```

Fix shape: either set `AAOS_ROLES_DIR=<tempdir>` in that specific test (most surgical), or teach `load_role_catalog` to accept an override path and have the test inject an empty one. Either is a 5-line change; not shipping in this entry — it's its own small follow-up.

**The agent's tool-schema confusion at turn 15.** One call was `file_read({"path": [2310, 2320]})` — the agent tried to use `path` as a line-range tuple. The current `file_read` tool silently rejects the call with "missing 'path' parameter"; consider surfacing "got array, expected string" so an LLM can self-correct faster. Not shipping that either — one malformed call out of 18 isn't a crisis.

## What worked

- `cargo_run` tool ran real `cargo check` and `cargo test` against the aaOS workspace from inside aaOS, under capability enforcement. Subcommand allowlist refused nothing because the agent never tried `install` or `publish`.
- Telemetry-with-previews (shipped 2026-04-16) made the capability denials and the `file_read` schema error debuggable in seconds. Without the previews, this run would have been a black box.
- Goal → Planner → builder role routing worked end-to-end from a freeform English goal (once the paths were flagged as operator-stated).
- The agent's honest-on-blocked discipline held under real execution. Runs 1–4 all produced correct reports of what they could and couldn't do, with zero fabrication.

## What shipped

- `cargo_run` tool + `CargoRun { workspace }` capability + `builder` role YAML. Total ~400 LoC. Subcommand allowlist `{check, test, clippy, fmt}`. 4-minute timeout, 8 KB output cap. Commit `45ce06b`.
- Doc updates: README tool count 12→13, architecture.md tool description, roadmap.md Phase F-a iteration note, ideas.md "self-hosted build loop" entry (marked as surface-shipped, gated on a successful run for further expansion). Commit `755149e`.

## What didn't ship

- No source code changes from the run itself. That's the correct outcome — the plan was already implemented — and a meaningful first data point for any future self-build loop: **an honest "already done" is a success case**, not a failure.
- No fix for the test-hygiene bug surfaced at turn 14. It's queued as a small follow-up.

## Cost

~10 minutes of DeepSeek API use for runs 1–4. Dashboard not checked yet; token-math estimate is **[token-math estimate] under $0.05** across all four runs combined based on observed traffic (18 turns of mostly small reads + one cargo test payload). Droplet cost ~$0.01 for the hour.

## Takeaways worth lifting

- **"Already implemented" is a valid builder outcome.** A self-build loop that can recognize a no-op is cheaper and safer than one that always re-applies edits.
- **Glob syntax is a convention**, not a universal. aaOS uses single-`*` prefix semantics; gitignore uses `**`. The builder role YAML is now an example of the right shape.
- **Planner paths need the right words.** "operator-stated absolute path" is a literal keyword the Planner prompt recognizes. Freeform goals will get routed into `{run}`-scoped workspaces unless steered.
- **Test isolation from host config matters.** `load_role_catalog` reads `/etc/aaos/roles/` directly; any test that constructs a `Server` picks up the host's catalog. Either stub the path or stop reading from `/etc/` at test time.
