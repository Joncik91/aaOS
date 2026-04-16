# Role budget wiring + prompt tightening *(2026-04-17)*

**Integration commits:**

- `dfb97f9` — Planner prompt rules (path shape, operator-absolute paths, decomposition)
- `6b2387e` — `{inputs.*}` array-expansion in capability rendering
- `ef45e61` — `SubtaskExecutorOverrides` threading role budget + retry into `ExecutorConfig`
- `c412a14` — fetcher / analyzer / writer system-prompt + budget tightening

## Setup

Fresh `debian:13` Docker container on A8 (not a full droplet — this feature doesn't need systemd-as-PID-1). Built the `.deb` on A8 via `cargo build --release -p agentd` + `cargo deb --no-build`. Installed in the container, set `DEEPSEEK_API_KEY` at `/etc/default/aaos` mode 0600, spawned a `testop` user in the `aaos` group, launched the daemon, ran the canonical benchmark `"fetch HN and lobste.rs, compare the top 3 stories on each, write to /data/compare.md"`.

Four benchmark runs across the day:

| Run | Commit(s) under test                                | Wall-clock | Outcome                                                                          |
|-----|-----------------------------------------------------|-----------:|----------------------------------------------------------------------------------|
| v1  | `cbd3dc7` (computed-orchestration baseline)         | **5m30s**  | 5-step over-decomposed plan. Workspace files never written. Output was training data. |
| v2  | `dfb97f9` (Planner prompt fix)                      | **n/a**    | Plan shape correct (4 subtasks, parallel fetchers, paths include filenames, `/data/compare.md` verbatim). Fetchers 3m44s each; `{inputs.*}` capability denials on downstream children. |
| v3  | `6b2387e` (`{inputs.*}` expansion)                  | **n/a**    | Capability denials gone. Fetchers still 3m30s each. Writer fabricated from training data. |
| v4  | `ef45e61` + `c412a14` (budget + prompts)            | **28s total** | **12× faster.** Fetchers 9-10s each. Writer errored loudly: `ERROR: missing input /.../hn.html` — exactly the contract in its prompt. `/data/compare.md` was NOT written. |

## What worked

**Root cause diagnosis beat guesses.** An Explore subagent traced the data path from `Role.budget` → `Role::render_manifest` (which silently drops it) → `execute_agent_for_subtask` (which hardcodes `ExecutorConfig::default()`). Identified the exact line: `crates/agentd/src/server.rs:335` used the default 16_384-token cap instead of the role's declared 2_000. That made **the budget fix a data-plumbing bug, not a prompt bug** — which is a very different kind of fix.

**Budget-plumbing fix landed cleanly.** New `SubtaskExecutorOverrides` struct, extended `SubtaskRunner` signature by one argument, pulled `role.budget.max_output_tokens` + a conservative iteration cap in `spawn_subtask`, passed through `install_plan_executor_runner`'s closure, consumed by `run_subtask_inline` → `execute_agent_for_subtask`. 42 plan/ unit tests pass. No runtime regression.

**Analyzer + writer prompts held the contract.** The rule *"If any listed input is missing, your reply must be exactly `ERROR: missing input <path>` — do not fabricate from prior knowledge"* was followed exactly. The writer emitted that literal string instead of the coherent-training-data fabrication of runs 1-3. That's the silent-quality-failure mode closed.

**Parallel fetchers still work.** Both fetchers spawned in the same second across all four runs. The Planner's "prefer parallelism" rule survives every subsequent change.

**12× wall-clock improvement is real.** 5m30s → 28s for the same goal. The budget cap did its primary job: prevented the LLM from spending minutes trying to hold HN's HTML in its response space.

## What the run exposed

**Prompt-only fetcher fixes don't work. The fetcher now lies about writing.** The 200-token output cap was supposed to force `file_write` because the LLM "couldn't fit the body in text." In practice, the LLM instead emitted a plausible-looking ack like *"written to /var/lib/aaos/workspace/.../hn.html"* and exited — without ever calling `file_write`. No web_fetch was called twice, no capability denial, no error; just a fabricated confirmation. This is the mirror image of the v3 silent-quality-failure: same class of bug, different subtask.

The plan's Step 4 success criterion explicitly anticipated this: *"If criteria 1-2 don't hold, the prompt fix is insufficient — escalate to Option C (deterministic fetcher scaffold) which skips the LLM loop for fetcher entirely."* Criterion 1 held (fetcher <20s); criterion 2 failed (workspace files don't exist). Scaffold is the next commit.

**LLM prompt discipline can't enforce tool-call side effects.** The fetcher prompt says "call file_write, then respond with the path." The LLM can choose to do just the second half. No amount of "do NOT skip step 2" in the system prompt reliably prevents this — the LLM is optimizing for a response that completes the goal by its lights, and a path-string response completes the goal by its lights. The fix is to **not use an LLM for mechanical I/O** — have the runtime do `web_fetch` → `file_write` deterministically, and the LLM only enters the loop for tasks that actually require reasoning.

**Writer's "error loudly on missing input" pattern is generalizable.** That same contract (literal error string when prerequisites aren't met, no continuation with partial data) should apply to every role whose inputs are computed by prior subtasks. The analyzer has it too; fetcher doesn't need it because fetcher has no inputs.

## What shipped

- **`ef45e61`** — `SubtaskExecutorOverrides` + runner-signature extension + per-subtask `ExecutorConfig` construction. Real production infrastructure. Role YAML values now actually constrain the LLM call. Stays shipped even if the fetcher moves to a scaffold — the budget plumbing benefits every LLM-powered role.
- **`c412a14`** — role prompt + budget tightening. Fetcher prompts ship but will be superseded when the fetcher becomes a scaffold. Analyzer + writer prompts are correct and stay.
- **Reflection entry** (this file) — honest record of the 12× speed win + the new fabrication mode.

## What remains

Fetcher needs to become a deterministic scaffold. Shape:

1. `PlanExecutor::spawn_subtask` checks `subtask.role == "fetcher"` (or a `role.scaffold: true` flag on the role YAML).
2. If scaffold: runtime directly invokes `web_fetch(url)` → `file_write(workspace, body)` via `ToolInvocation` in Rust, no LLM call.
3. SubtaskResult returns the workspace path as its response text, with real token usage (zero, since no LLM).

Scope: ~half a day. New plan when ready. Analyzer + writer are genuinely LLM-shaped work (they read content and produce prose); they stay LLM-powered.

## Cost

- DeepSeek API spend across v1-v4 end-to-end runs: **<$0.10** estimated ([token-math estimate]; dashboard confirmation tomorrow). v4's 28s run was probably <$0.01 by itself.
- Container compute on A8: free.
- No droplet used today.

## Lessons for patterns.md

**"Stub tests don't catch prompt bugs" generalizes to "prompt contracts don't enforce tool-call side effects."** The writer prompt's "error on missing input" rule works because the LLM has nothing else to do when the prerequisite fails — the contract aligns with the LLM's optimization pressure. The fetcher prompt's "call file_write before responding" rule fails because the LLM can satisfy "respond" without satisfying "call file_write" — the contract and the LLM's optimization pressure point in different directions. When an LLM can satisfy the prompt's surface reading without performing the underlying side effect, prompts don't enforce anything. Move the side effect out of the LLM and into deterministic code.

Pattern for when to use an LLM vs. a scaffold:
- **LLM-shaped work**: pattern-match, summarize, compare, reason about content. Output is prose. Quality correlates with model capability.
- **Scaffold-shaped work**: fetch-then-write, parse-then-file, transform-then-store. Output is mechanical. Quality is binary (did it execute or not).

Fetcher is scaffold-shaped. Analyzer and writer are LLM-shaped. Mixing them into the same role-abstraction was the original sin.
