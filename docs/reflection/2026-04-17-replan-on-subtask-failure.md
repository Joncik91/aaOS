# Replan on subtask failure *(2026-04-17)*

**Integration commit:** `54cf501` — `feat(plan): replan on subtask failure instead of aborting`

## Motivation

Until this change the `PlanExecutor` only replanned on **pre-flight** failures: unknown role, malformed params, or a topo cycle. A runtime failure inside a subtask — a fetcher hitting a 404, a writer unable to open its output — turned into `ExecutorError::Terminal` via `try_join_all` and aborted the entire DAG with no retry. Every other coding-agent runtime we looked at (opencode's tool-use-with-retry; the single-shot planners in the research) has the same gap: a subtask blows up, the plan dies. Real agent systems need to **observe** the failure and **replan** with that observation as context.

The infrastructure was almost already in place: `Planner::replan(goal, catalog, previous, failure_reason)` existed, `ExecutorError::Correctable(String)` existed, `AuditEventKind::PlanReplanned { reason }` existed. The missing piece was the edge from "subtask failed at runtime" back to "emit Correctable with the failure reason."

## What shipped

`execute_plan` no longer uses `try_join_all`. It uses `join_all` so every sibling finishes, audits each outcome (`SubtaskCompleted { success: true | false }` — previously only the success branch was audited), and on the first failure in the batch returns `Correctable("subtask '<id>' (role '<role>') failed: <err>")`. The outer `run()` loop already caught `Correctable` and called `Planner::replan`; nothing there changed.

The replan prompt also got a rewrite. Previously it was `"PREVIOUS PLAN FAILED. Revise."` — three words of signal. Now it's a short paragraph instructing the planner to diagnose the failure, not re-emit the failing subtask verbatim, and produce a clean-failure plan if recovery is impossible.

Four new executor tests cover the four failure shapes: reason string includes subtask id + role + error; failure emits the new `success:false` audit event; parallel batches preserve sibling audit on one-of-two failure; outer `run()` loop successfully replans and completes with a scripted two-shot LLM. 138 runtime tests pass.

## Verified end-to-end

Rebuilt `.deb`, installed in the `aaos-scaffold` debian:13 container (reused from this morning's sweep), pointed at real DeepSeek.

**Case A — failure-dominated (goal forces the bad URL):**

```
submit "fetch https://example.com/does-not-exist-test-404 and write a summary to /data/summary.md"
```

Timeline (8-char agent-id prefix):

| Time     | Event                                        |
|----------|----------------------------------------------|
| 05:56:42 | `c9e10f47` spawned fetcher → web_fetch (404) |
| 05:56:49 | `cd7d76ee` spawned fetcher (replan #1) → 404 |
| 05:56:55 | `47650737` spawned fetcher (replan #2) → 404 |
| 05:57:02 | `6f73481f` spawned fetcher (replan #3) → 404 |
| 05:57:02 | `bootstrap failed (26s)` — `max_replans` cap |

Bounded exactly as designed. Pre-change: one fetcher fails → whole plan aborts. Post-change: three replan attempts (the default `max_replans = 3`), still failing because the operator literally pinned the URL, then a clean terminal failure. The cost is bounded, the loop terminates, the operator sees the attempt count in the stream.

**Case B — failure-then-recovery (goal leaves room):**

```
submit "fetch today's front page of Hacker News (https://example.com/this-will-404-first-try) \
        and write a one-paragraph summary to /data/hn-summary.md"
```

Timeline:

| Time     | Event                                              |
|----------|----------------------------------------------------|
| 05:58:23 | `4ad1ea7c` spawned fetcher → example.com → 404     |
| 05:58:29 | `06b799c6` spawned fetcher (replan) → HN → success |
| 05:58:30 | `06b799c6` tool: file_write                         |
| 05:58:30 | `258d3c39` spawned writer                           |
| 05:59:36 | `258d3c39` tool: file_write                         |
| 05:59:38 | `bootstrap complete (82s)`                          |

The planner read the failure reason (`subtask 'fetcher_0' (role 'fetcher') failed: HTTP 404`), noticed the goal's phrasing allowed a different target, picked `news.ycombinator.com`, and the second plan succeeded. `/data/hn-summary.md` is 6 KB of real prose citing "Claude Opus 4.7" as the #1 HN story — post-training-cutoff content, not fabricated.

## What this is, concretely

This is the first **observe→replan** primitive in the codebase. The Planner was always capable of replanning; what was missing was the feedback edge from runtime failure back to that capability. With this commit, a subtask failure is no longer a plan-terminating event — it's a signal the planner can act on. The bound (`max_replans = 3`, `total_deadline = 10min`) prevents pathological loops; the explicit `Correctable` string carries enough context for the LLM to pick a different approach.

This is also the first piece of aaOS that is **qualitatively different** from every coding-agent runtime in the comparison set. opencode retries the *tool call* and lets the LLM re-read the error; nothing in the open-source space **replans the DAG** on runtime failure.

## Cost

Case A: ~26s of DeepSeek calls (4 planner invocations + 4 failed fetches). Case B: ~82s (2 planner invocations + 1 failed fetcher scaffold + 1 successful fetcher + 1 writer LLM loop). Estimated spend well under $0.02 across both runs ([token-math estimate]).

## Not in this commit

- **Per-role retry policy.** The role YAML has a `retry.max_attempts` field but it only affects `ExecutorOverrides::max_iterations` (LLM-loop iteration cap). A real per-role replan limit (`retry.max_replans`) isn't wired. Deferred until a goal surfaces a need.
- **Failure classification.** All failures today look the same to the planner — a free-form string. If 10-20 replans surface patterns ("network errors → retry same URL", "404 errors → pick a different URL", "capability denied → drop the subtask"), a structured `FailureReason` enum becomes warranted. Not today.
- **Partial-result reuse across replans.** When subtask A succeeds and B fails, the replan starts from scratch — A runs again under the new plan even if its output is still valid. Caching-by-content-hash would let B's replan reuse A's result. Real optimization, but premature: the current cost is dominated by the LLM loops, not re-running scaffold-shaped work.

## Shipped

- `54cf501` — runtime + prompt + tests.
- This reflection entry.
