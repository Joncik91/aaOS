# Junior ships `git_commit` tool — run 12 *(2026-04-17)*

**Integration commit:** `ad87de6` "feat: git_commit tool — narrow `git add` + `git commit` under capability scope"

First self-build run against the bumped 60-iteration budget + plan-complete checklist that shipped with run 11. Scope was a full new Tool: capability variant, two parser sites, 250-LoC tool file with tests, three registration sites, role grant, README + architecture docs. Run-11-shape or slightly bigger.

## What shipped

- `Capability::GitCommit { workspace }` variant + `permits()` arm + unit test.
- `"git_commit: {path}"` grant-syntax parser in both `aaos-runtime/src/registry.rs` and `agentd/src/spawn_tool.rs`.
- `aaos_tools::GitCommitTool` — subcommand allowlist `{add, commit}`, message-starts-with-dash rejection, 60 s timeout, 2 KB output cap, returns new commit SHA via `git rev-parse HEAD`. "Nothing to commit" is a success with a flag, not an error. Only `-m` is ever passed to git.
- Registered in all three server tool-registry setup sites.
- Builder role granted `"tool: git_commit"` + `"git_commit: {workspace}"`.
- Tool count 15 → 16 across README + architecture.md.
- 4 new unit tests (1 capability + 3 tool-level, of which 1 runs without git, 3 are `#[ignore]`-gated on a host git binary). Workspace count 484 passed / 0 failed / 15 ignored.

## Junior run

272 s wall clock for the builder subtask; 45 tool calls landed. Discovery phase used `grep` to find `GrepTool` registration sites in `server.rs` (3 hits at 565/714/846) and `cargo_run:` parser sites (2 files). File edits landed first-try on every anchor the plan named. The ambiguous-edit recovery that runs 8 and 11 surfaced fired once here too — on the three `GrepTool` registration lines — and the agent dropped to a single-site `old_string` with `replace_all: true`, same pattern as before.

The agent went beyond the plan in one spot: added a 4th test (`rejects_workspace_without_capability`) that the plan didn't ask for. Small initiative, correct test, accepted.

## Senior touch-ups

Two small edits:

1. **`docs/architecture.md`** — the agent was mid-edit on the tool list + description paragraph when its subtask budget ran out. Completed both edits on pull-back.
2. **One stale docstring** on `MAX_INLINE_OUTPUT` referenced a `stdout_path` parameter the tool doesn't expose (copy-paste residue from `cargo_run.rs`). Rewrote to match the tool's actual behavior.

## What the new budget + checklist did and didn't do

**The 60-iteration cap helped.** Run 11 stopped at ~35 calls mid-implementation with the tests + polish missing; run 12 landed 45 calls with everything except the last architecture.md edit + report. Roughly 25 % more work per run. No budget-exhaustion log line this time.

**The plan-complete checklist did not fire.** The prompt now contains an explicit three-step finishing protocol ("run every cargo_run the plan names, write the report to {report}, only then respond with the report path"). The agent said `complete` mid-work — with the report unwritten and one doc edit still pending — as though the implementation phase alone was the whole task. Three possible explanations: the checklist text is too far from the decision point in the prompt; the subtask completion is a policy the executor enforces based on LLM stop signals that the prompt can't reach; or the agent treated the bootstrap's overall goal ("return only the report path") as permission to stop once it had something plausible to say. None of these is diagnosed yet. The checklist may still be helping in ways we can't measure from one run — but on its own it did not close the loop.

## What this proves

- **The budget knob is the right first lever, and 60 is a realistic ceiling for this scope.** Run 11 clipped at 40; run 12 came in under 60 with margin. Small plans (runs 7, 9, 10) fit in 15-25; medium plans (runs 11, 12) need 45-55. A 60-cap covers everything we've attempted with ~10 turns of headroom.
- **The ambiguous-edit recovery pattern is now a stable behavior.** Three runs (8, 11, 12) have hit it; three runs have recovered the same way (`replace_all: true` on a shortened anchor). Treat it as a feature of the tool, not a finding.
- **Plan shape matters more than budget.** The plan gave exact `old_string` / `new_string` pairs for the capability variant and for each parser site. Every one landed first-try. The one piece that didn't land — the architecture.md description — was the one piece the plan did *not* give as a pair (it said "add a one-sentence rationale" in prose). Rule: if the plan can specify the exact text, specify it.

## What this didn't prove

- **The checklist is unproven.** We haven't yet seen a run where the agent visibly consults the checklist and writes the report as a result. Next step is either to strengthen the prompt's anchor (move the checklist to a closing system message, or repeat it in the user-visible message template) or to back it with an executor-level check that refuses to mark a subtask "complete" when `{report}` is absent. The latter is the stronger fix.
- **Subtask isolation handles zombies cleanly.** A failed first dispatch left an orphan agent (`bea8fa34`) in a scratch workspace that ran in parallel with the good run. It burned its own budget pretending to build scaffolded placeholder files, but never interfered with `7db9d46a` — different subtask, different capabilities, different iteration cap. Good: the architecture tolerates zombies. Less good: the first dispatch's failure (capability-denied loop on the Planner-assigned scratch dir) should have been noticed and cancelled by the operator CLI, not left to idle.

## Cost

~5 minutes DeepSeek for the productive subtask; an extra ~3 minutes burned on the zombie. Senior review + touch-ups: ~8 minutes. Droplet hour: negligible (reused existing droplet). End-to-end: ~20 minutes from plan dispatch to commit, same bracket as run 11.

## Takeaway worth lifting

**The coding-agent primitive surface is now five tools: `file_read(offset, limit)`, `file_edit`, `file_list`, `grep`, and `git_commit`** (plus `cargo_run` for build/test, not strictly a coding primitive). That matches the working subset of the Claude Code / Cursor / OpenCode tool set for code editing. The remaining gap versus that reference set is not more tools but more autonomy: the junior still cannot produce a plan from a higher-level goal, it only applies a plan a senior wrote. Closing that gap is not a tools problem.
