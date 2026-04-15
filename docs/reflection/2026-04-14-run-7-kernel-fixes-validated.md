# Run 7 / 7b — kernel fixes in action *(2026-04-14)*

**Integration commits:** No new code from this run — it validated the Run-6-triggered fixes (`505f559`, `5feedbe`) against real behavior. The output artifacts (workspace docs) were lost at container removal; the lesson (export before `docker rm`) is noted under Process Lessons below.

## Setup
- Memory state: fresh (host `memory/` empty; no `AAOS_PERSISTENT_MEMORY`).
- Goal: same as Runs 5 and 6 — *"Read your own source code at /src/, find something meaningful to improve, and produce a concrete proposal with implementation."*
- Container built at commit `4bd8cff` (observability redesign). Docker cache hit on the first build silently produced a stale binary with no Fix 1/Fix 2 in it — discovered only after the Run 7a code-reader was granted `tool: memory_store` and stored 3 orphaned memories. Cancelled 7a, rebuilt with `--no-cache`, verified Fix 1 + Fix 2 strings present in the fresh binary via host-side `strings`, relaunched as 7b.
- Two monitor streams active this time: significant audit events (spawn/stop/denied/memory/complete) and a 30-second heartbeat summarizing recent activity. The heartbeat was essential — several ~2-minute silent windows occurred because of slow DeepSeek responses, not because the run had died.

## What Worked
- **Fix 1 held.** On every child spawn after the first, Bootstrap omitted `tool: memory_store` from the child manifest. Zero `capability_denied` events fired — meaning Bootstrap's *prompt-side* understanding stayed aligned because the kernel rule was now credible. The teaching prose in the updated manifest worked *because* it referenced a kernel rule that actually existed.
- **Fix 2 used correctly four times.** Every child spawn used the structured `prior_findings` field to pass the previous child's output forward. The parent→child handoff path from Run 6 that caused proposal-writer confabulation was closed — the child saw its goal in `message` and the previous output in a kernel-framed BEGIN/END block with the injection warning.
- **Four-agent chain with narrowed scope per child.** Bootstrap decomposed into: `code-reader` (source scan + analysis.md), `analyzer` #1 (evaluate options, pick one, evaluation.md), `analyzer` #2 (given source access to produce implementation proposal, proposal.md + implementation_plan.md + sample_implementation.md + migration_guide.md), `writer` (synthesize summary.md, workspace-only capabilities). Each child had exactly the caps it needed — `/src` read only for scanning and implementation-design stages, workspace-only for evaluation and synthesis. The capability system narrowed as the task concentrated.
- **Grounded findings.** The code-reader caught a real naming drift (`MemoryResult2` vs `MemoryResult`) and real architectural points (sync SQLite in async context, scattered config). None of it was confabulated — each claim had a specific file path and line reference.
- **Bootstrap's own memory store fired exactly once,** at the end, with category `decision`. That's the run-summary we designed for cross-run persistence — if `AAOS_PERSISTENT_MEMORY=1` had been set, this summary would have been the first real candidate for next-run `memory_query` retrieval.
- **Observability rewrite held up under live use.** The dashboard showed 4 agents with one-line activity each, the significant-events band surfaced the spawn/stop/memory events we cared about without being drowned in tool_invoke noise. The detail log format (`HH:MM:SS  agent  VERB  body`) was readable top-to-bottom during the run.

## What the Run Exposed
- **Docker build cache silently hid the fix.** The first build after the Fix 1/2 commits produced a binary without them despite timestamps suggesting otherwise. Strings-check on the binary was the only way to confirm the fix was live. **New process rule:** after any runtime code change, rebuild with `--no-cache` and grep the binary for a known unique string from the change *before* launching a run. Added to the `patterns.md` entry below.
- **`analyzer` #1 tried to read `/src/`** without that capability — denied (correctly), tool_result returned `success=false`. The analyzer was a pure-evaluation role (should work from prior_findings only), so Bootstrap was right not to grant source access. The denial demonstrates the runtime catching a cross-role capability mismatch in real time.
- **Artifacts were lost on `docker rm`.** Workspace files (`/data/workspace/<uuid>/*`) were not exported before container teardown. Only the significant-events monitor stream + a handful of `head` captures during the run survived. **New process rule:** before stopping the container at run end, `docker cp` the workspace to `output/run-N-artifacts/`. Before — the bind-mount on `/output/` covered any file the agent wrote to `/output/`, but workspace files live elsewhere.
- **LLM calendar estimates are back.** The writer's `prior_findings` section mentioned a "6-week" migration plan for `AaosError`. Same old pattern — the actual work, done with a peer-reviewed plan and focused implementation, would be 1-2 hours. Keep noting this; don't treat it as a new finding.
- **DeepSeek latency was spiky.** Several ~2-minute waits on single LLM calls, once ~3 minutes. Connection state via `/proc/1/net/tcp` confirmed retransmit-timer growth but no dead connection. The system recovered each time without manual intervention — but the heartbeat monitor was essential to distinguish "slow call" from "stuck container."

## What Shipped
- **Nothing this run, by design.** Run 7b was a validation run: it exercised Fix 1 and Fix 2 end-to-end against real behavior. Both held. The proposal produced (error-handling unification across all 7 crates) is worth reviewing as a future implementation candidate but was not shipped — it's the system's recommendation, not our decision.
- **Process lessons** for Runs 8+: `--no-cache` on first build after runtime changes, binary-string verification before launch, `docker cp` workspace export before `docker rm`, heartbeat-style monitoring for DeepSeek hangs.

## Cost
Cumulative DeepSeek dashboard figure after Runs 7/7b: **~$0.76** (up from ~$0.60 at end of Run 6). Run 7 (cancelled early) + Run 7b combined ≈ **$0.16**. Roughly 2× Run 6 — consistent with a 4-child chain vs 2-child, and Run 7a's partial spawn/spend before cancellation.

## Design / Review Notes

No peer review for this run — it was a validation run for already-reviewed fixes. The peer-review pattern will kick back in when we have new code changes to review (next candidate: either the `AaosError` unification the system proposed, or whatever Run 8 surfaces).
