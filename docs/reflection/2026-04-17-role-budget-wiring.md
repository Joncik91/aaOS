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

## Addendum — scaffold verified *(same day)*

Scaffold commit: `2b8ed6d` — `RoleScaffold { kind }` on the `Role` struct, `ScaffoldRunner` closure type parallel to `SubtaskRunner`, `PlanExecutor::set_scaffold_runner`, branching in `spawn_subtask`. The fetcher role YAML gets a `scaffold: { kind: fetcher }` header. When the executor encounters it, it dispatches to `Server::scaffold_fetcher` which does `web_fetch` → `file_write` directly through the capability-checked `ToolInvocation` in Rust — no LLM call.

v5 run (commit `2b8ed6d`, same container recipe, canonical goal):

| Phase | Timing | Evidence |
|-------|-------:|----------|
| Both fetchers spawn + web_fetch + file_write | **1s each** | `spawned fetcher` → `tool: web_fetch` → `tool: file_write` back-to-back in the event stream at `04:37:02–03` for both subtasks. |
| Analyzer | ~48s | LLM-shaped work, not a bug. |
| Writer | ~65s | Reads the real HTML files, emits real prose. |
| Total wall-clock | **2m9s** | Dominated by analyzer+writer LLM loops on DeepSeek chat. |

Files on disk after the run:
- `hn.html` 34 KB — contains "Claude Opus 4.7" as #1 story (post-training-cutoff event from 2026-04-15).
- `lobsters.html` 50 KB — contains "IPv6 traffic crosses the 50% mark".
- `/data/compare.md` 6.3 KB — cites both titles above with correct vote counts and timestamps.

Zero `capability denied` events. Writer did NOT emit "ERROR: missing input" — because the inputs actually exist now.

**What this closes.** The fetcher-fabricates-path-ack bug (v4) — the LLM satisfying "respond with path" without calling `file_write`. Deterministic Rust code calls `file_write` before returning; there is no possible state where the response exists but the file doesn't. The fabrication-from-training-data mode (v3) was already closed by the writer prompt contract in `c412a14` — v5 confirms it stays closed when the inputs are real.

**What v5 did not improve.** Total wall-clock went from 28s (v4) to 2m9s (v5). Not a regression — v4 short-circuited because its writer errored out on the missing inputs; v5 actually produced the output, so it paid the real analyzer+writer LLM-loop cost. That cost is a separate fitness-of-prompt question (the writer does two rounds of `file_read` then a long `file_write`; could probably be cut in half), deferred.

**Shipped.** `2b8ed6d` plus this addendum. Fetcher is now a first-class scaffold; the pattern (`scaffold_runner` closure, `Role::scaffold` field, kind-dispatched runtime implementation) is in place for future scaffold-shaped roles — `file_sync`, `archive_extract`, `db_dump`, whatever shows up. The LLM path is unchanged for roles that need it.

## Addendum 2 — adversarial e2e sweep *(same day)*

After the scaffold verification, ran five adversarial cases against the same running daemon to probe for edge cases. Three of the five surfaced real bugs.

| # | Case | Outcome | Bug |
|---|------|---------|-----|
| 1 | `fetch https://example.com/does-not-exist-404-test-page` (200 from example.com + a real `/status/404`) | Completed; empty 0-byte file was written, writer honestly said "no data captured" | #1: fetcher scaffold treats all HTTP bodies equal — a 404 / empty body silently succeeds |
| 2 | `fetch HN front page, summarize top 5` | Clean 2-subtask plan, 32s, real content | none |
| 3 | `fetch HN + lobste.rs + slashdot, top 2 each` | All 3 fetchers spawned same second; Slashdot blocked (Cloudflare), writer reported "unavailable" honestly | same class as #1 (fetcher does not surface protocol errors) |
| 4 | `fetch example.com, write summary to /etc/cannot-write-here.md` | Writer emitted `ERROR: missing input <workspace-path>` — but the input was there; `file_write` to `/etc/` was what failed | #2: writer prompt contract covers only `file_read` failures; `file_write` failure falls through to the nearest-error template and lies about the cause |
| 5 | `write a Merkle-tree explainer to /data/merkle.md` (no fetch) | Planner picked `generalist` + `writer`, but generalist has no workspace/output-path parameter, so writer's input path never materialized — `ERROR: missing input` | #3: planner composes generalist→writer plans that cannot resolve (no declared handoff path) |

**Orthogonal:** the `submit` CLI's `tool: <name>` line is emitted from `ToolInvoked` only. The audit schema has a separate `ToolResult { tool, success: bool }` kind (`crates/aaos-core/src/audit.rs:65`) which the operator view doesn't surface. Failed tool calls look identical to successful ones in the event stream — part of why #2 was confusing to diagnose.

**What this means.**

- The scaffold fix is correct for the LLM-can-skip-side-effects class of bug it targets. It does not fix HTTP-semantics bugs (empty/404 body written to disk silently) — that's a separate "fetcher should treat non-2xx as an error" follow-up, bounded (~1 hour: add a status-code check in `scaffold_fetcher`, return `CoreError` on 4xx/5xx).
- The writer contract needs a second error shape for `file_write` failures — otherwise when the output path is un-writable, the operator sees a misleading error. Bounded fix: add a second line to the prompt, *"If file_write fails, respond with exactly `ERROR: cannot write <output>: <reason>` — do not retry, do not invent a missing input."*
- The planner needs a rule that roles used in a handoff chain must have a declared output-path parameter, OR generalist needs an output-path parameter added to its schema. The cleaner fix is to teach the planner: *"If a downstream subtask depends on this one, the upstream role MUST have a workspace/output parameter."* — bounded ~30 minutes in the Planner prompt.
- The `tool:` event should distinguish success vs. failure. Either add a `tool ✗` / `tool ✓` variant to the operator-visible events, or fold `ToolResult` into `ToolInvoked` at audit-emit time. Small but high-signal improvement.

**Ship-vs-defer call.** None of the four are blocking for the current scaffold milestone. They're edge cases operators would hit once they started using the daemon for varied goals. Worth filing as separate short tasks — probably one commit per issue. The fetcher scaffold itself is correct and stays shipped.

No code changes in this addendum — just the e2e record and the diagnosis.

## Addendum 3 — edge-case fixes shipped and verified *(same day)*

All four issues from Addendum 2 fixed in four focused commits:

- `3699ce1` — `fix(fetcher): scaffold errors on non-2xx or empty body`. Scaffold now inspects the `status` field from `web_fetch`; returns `CoreError` outside 200..300 or on empty body. 404 and blocked responses fail cleanly instead of silently writing error pages.
- `6c10516` — `fix(writer): explicit contract for file_write failure`. Writer prompt gains a second error shape `ERROR: cannot write <output>: <reason>` for `file_write` failures, separate from the `ERROR: missing input <path>` shape for `file_read` failures.
- `bf88510` — `fix(plan): handoff rule + generalist workspace param`. Generalist gets an optional `workspace: path` parameter; planner prompt gains a handoff rule requiring upstream roles to declare an output path when downstream subtasks read from them.
- `20da490` — `fix(cli): surface failed tool results in operator view`. `ToolResult { success: false }` now operator-visible as `tool FAILED: <name>` (red in TTY). Successes stay hidden to avoid doubling the stream.

Re-verified in a fresh `debian:13` container with the rebuilt `.deb`:

| Bug | Pre-fix behavior | Post-fix observed |
|-----|------------------|-------------------|
| #1  | `/status/404` wrote 0 bytes, summary said "no data captured", pipeline reported success | `bootstrap failed (0k in / 0k out, 6s)` — clean failure, no downstream subtask spawned |
| #2  | Writer emitted misleading `ERROR: missing input <workspace>` when `file_write` to `/etc/` failed | `ERROR: cannot write /etc/cannot-write-here.md: Permission denied (os error 13)` — correct root cause |
| #3  | `generalist→writer` plan had no declared handoff path; writer failed with "missing input" | Planner now generates `fetcher(workspace=X) → writer(inputs=[X])` chains with correct path handoff; real content written |
| #4  | Failed `file_write` looked identical to success in the event stream | New `tool FAILED: file_write` line appears in red after the invocation line |

All unit tests still pass (44 plan/role + 59 CLI). No regressions. Scaffold path unchanged — the fetcher still fires `web_fetch` + `file_write` in 1s. The edge-case fixes ride on top of the scaffold rather than replacing any of it.
