# Tool-repeat guard + builder budget bump — run 11 *(2026-04-17)*

**Integration commits:** `19b2104` "feat: tool-repeat guard — hint to LLM when looping on same call", plus a follow-up commit bumping `builder.retry.max_attempts` and adding a plan-complete checklist to the role prompt.

Run 11 was the first self-build to run out of iteration budget mid-implementation. It also produced the most substantively correct partial work we've seen. The two outcomes are the same phenomenon from different angles: the plan was bigger than the role's ceiling, and the agent produced exactly as much correct code as its budget allowed.

## What the feature is

Self-build runs 3, 5, and 6 all hit the same failure mode: the LLM called the same tool with the same arguments multiple times in a row, never recognizing it had already received that answer (or that same denial). Run 5 read `server.rs` — with no offset/limit — **five times consecutively** looking for "line 2229" in a blob of text. Run 3 hit ten consecutive `CapabilityDenied`s on different paths without ever re-examining its grant list.

The tool-repeat guard closes that loop. `ToolInvocation` now tracks `(agent_id, tool_name, input_hash) → attempt_count`. When the count reaches `AAOS_TOOL_REPEAT_THRESHOLD` (default 3), successful tool results get a structured `_repeat_guard` field injected — `{ attempt_count: N, hint: "You have called `<tool>` with these exact arguments N times in this subtask. The previous attempts returned the same result. Try different arguments or a different tool." }`. The audit stream also gets a `ToolRepeat` event regardless of outcome.

Counter scope is one `ToolInvocation` per `Server`, keyed on agent. Agent boundaries are respected — Agent B's first call never sees Agent A's count. Different inputs to the same tool are tracked separately via `md5_hash` of the JSON args.

The hint only goes on the Ok path (we can mutate JSON). On the Err path the existing `CoreError` surfaces as-is, because rewriting errors to include hints would require wider plumbing than one retry-loop class justifies. The `ToolRepeat` audit event fires on both paths — operators see the pattern in streaming output either way.

## Run 11 itself

**Timing:** 272 s, ~35 tool calls. Bootstrap closed out with no report written; the agent's last log line was *"I need to inject the hint after the ToolResult audit event but before returning the result. Let me modify the code to inject the hint:"* — mid-thought when the loop terminated.

**Notable features of the trace:**
- **First use of `grep` in the wild, 8 calls.** Searched for `AuditEventKind::ToolInvoked`, then `ToolResult`, then `CapabilityDenied`, then `match.*AuditEventKind`, then `AuditEventKind::.*=>`. Exactly the discovery workflow the tool was added for — navigating an unfamiliar 27k-LoC codebase to find exhaustive match sites before editing. Without `grep` the agent would have paged through every crate's source with `file_read(offset, limit)` and burned substantially more turns.
- **Recovered from a `file_edit` ambiguity.** First attempt to edit `use std::sync::Arc` found 2 matches; agent retried with `replace_all: true` on the same edit — correct call. Same recovery pattern as run 8.
- **6 edits landed correctly.** `ToolRepeat` variant in `audit.rs`, imports + struct field + constructor + counter tracking + hint injection in `invocation.rs`. Junior diff compiles, all existing tests still pass (478).
- **4 unit tests not written.** The plan asked for `first_two_calls_have_no_repeat_guard`, `third_call_injects_repeat_guard`, `different_input_hash_resets_counter`, `different_agent_resets_counter`, plus a `test_repeat_count` helper. Budget ran out before any of these landed.
- **Loose ends.** The junior left an unused `HashMap` import and a mis-indented `use std::sync::{Arc, Mutex};` line in the tests module. Both rustfmt-level noise; senior cleaned on pull-back.

**Senior handoff was ~10 minutes**: pull invocation.rs + audit.rs to A8, verify still compiles, write the 4 unit tests (they passed on first try — the logic was correct), clean the rustfmt nits, run the full workspace (482 passed, 0 failed), commit with a `Co-Authored-By: aaOS builder role (ephemeral droplet, run 79bc8ee7)` trailer.

## Root cause of the budget exhaustion

`builder.retry.max_attempts` was `30` → `max_iterations = 40`. Run 11 used ~35. The plan had 10 named steps, so average cost was 3.5 turns per step once exploratory grep/read calls and `cargo check` verification were counted in. For small plans (runs 7, 9, 10 each landed in 15-25 turns) a 40-turn ceiling works. For medium plans with multi-site discovery (run 11) it doesn't.

## Fix shipped with this reflection

Two changes to `packaging/roles/builder.yaml`:

1. **Iteration budget: `retry.max_attempts` 30 → 60** (ceiling 40 → 70). Conservative bump that gives runs of run-11-scope ~2× headroom without eating all prior runs' margins. The repeat-guard shipped alongside this change ensures a bigger budget does not mean a bigger blast radius — a runaway agent now gets a "you've done this 3 times" hint injected into its own tool results.
2. **Plan-complete checklist in the system prompt.** Adds a three-step finishing protocol: run every `cargo_run` the plan's Verification section asks for, write the report to `{report}`, then respond with only the report path. Explicit "do not stop mid-plan" and "an honest incomplete report is far more useful than a silent stop." Prompt-only, no code change. Targets the specific failure shape of run 11 — the agent reached the end of its implementation phase and began reasoning about what to do next when the loop expired, having never written a report.

Neither change is scientifically tested yet — a run-11-scope plan needs to land fully on a single pass to validate. Queued as the next measurement.

## What this proves

- **Partial self-build work is still real work.** Run 11 produced 169-line diff that compiled cleanly and did the right thing logically. The only things missing were tests and trailing polish. Contrast run 5/6 which produced nothing usable.
- **`grep` earned its keep on the first real task.** The discovery phase in run 11 would have been substantially longer (or would have failed) without it. The tool pays for itself.
- **File-edit ambiguity recovery is now the agent's default pattern.** Runs 8 and 11 both hit it; both recovered with `replace_all: true` on their own. That's a second empirical data point for the claim that the Edit-tool refuse-on-ambiguity rule actually helps the LLM rather than frustrates it.
- **The iteration-budget formula is the right knob.** Increasing `retry.max_attempts` is cheap, reversible, and scoped per role. We don't need to redesign the executor.

## What this didn't prove

- **The checklist prompt addition is untested.** It might be redundant if the larger budget alone carries the agent to the end. It might conflict with existing "respond with only the report path" rules elsewhere in the prompt. Next large-scope run will tell us.
- **The repeat-guard hasn't been observed firing in a real run.** The feature is unit-tested but we haven't yet seen a self-build agent trip it organically. That requires a deliberate stress test (or just more runs).
- **We haven't solved the class of failure where the LLM emits no tool calls at all** (run 6's silent stall). That's different in kind — the model's output token budget, not the runtime's iteration cap. Still open.

## Cost

Run 11: ~7 minutes of DeepSeek. Senior handoff: ~10 minutes of my time. Total end-to-end feature ship: ~20 minutes from plan dispatch to commit.

## Takeaway worth lifting

**"Budget exhaustion" is a legitimate outcome, not a failure.** Run 11 used every turn productively. When the agent hit the cap, it was still making progress — not spinning. The right response is to raise the cap, not to declare the run broken. The distinction is important: a spinning agent (runs 5, 6) is a bug in the tool surface or the plan; an exhausted agent (run 11) is a bug in the budget knob. The former is not fixed by larger budgets; the latter is fixed by nothing else.
