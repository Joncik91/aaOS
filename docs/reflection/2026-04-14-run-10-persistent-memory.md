# Run 10 — persistent memory, same goal as Run 9 *(2026-04-14)*

**Integration commits:** None yet (pending fix decision). Built on `f567d7a` with all Run 9 Fixes 1–7 + correction commits live. Fresh `--no-cache` rebuild verified by extracting `session_store_error` and `select_summarization_boundary` strings from the `agentd` binary before launch.

## Setup
- Memory state: **persistent memory enabled**, carrying Run 9's memory dir forward (`./memory/memories.db` intact, SQLite WAL present).
- Goal: adversarial bug-hunt prompt, expanded with "do not re-report bugs already fixed in recent commits (check git log and memory) — find something new."
- Hypothesis: does persistent memory let the system avoid re-finding bugs Run 9 fixed, and push deeper into new territory?

## What Worked
- **Memory was actually queried.** Bootstrap fired `memory_queried` twice at startup — first time in the log's history that cross-run retrieval actually happened. Infrastructure works end-to-end.
- **Chain compressed.** Three agents (bootstrap → code-reader → bug-reporter) vs Run 9's four. Shorter decomposition; lower total tokens (1.11M vs Run 9's 1.2M).
- **One real finding underneath a mis-framed one.** The child agent's final bug report was titled "Race Condition in Agent Registry Spawn" pointing at `registry.rs`. On the surface, that's Run 9's Fix 1 (already shipped). But the report specifically named `spawn_with_tokens` (line 315-377), which is a *different* function from `spawn_internal` and was *not* patched by Fix 1. Verified against source: `spawn_with_tokens` still has the pre-Fix-1 pattern — `self.agents.insert(id, process)` at line 365 before `router.register()` at line 367-373. Same invariant bug, different function. The system found a gap in its own prior fix.

## What the Run Exposed
- **Memory-based fix tracking isn't granular enough to shield against duplicate findings.** Bootstrap queried memory at startup and retrieved the Run 9 summary, but the summary was goal-level prose ("found and fixed error-handling issues") rather than a structured list of `file:line → fixed` pairs. Two children then re-raised the registry race condition and the path canonicalization issue — both closed in Run 9. Without granular tracking, memory doesn't protect future runs from redundant work.
- **"Check git log" was an unenforceable instruction.** The prompt told agents to check git log for recently-fixed bugs; agents have no `git_log` tool and no shell capability. They ignored the clause. Either add a minimal git-log capability to the tool registry or remove the instruction from prompts.
- **Child 1 produced 15 candidate findings; Bootstrap's final synthesis kept only one.** The first child's output was Run-9-style (many candidates, high recall, low precision). Bootstrap (reasoner) filtered hard — but filtered *toward the most familiar shape* (registry race) rather than toward the most novel. Memory didn't help Bootstrap recognize that the familiar shape was already fixed.
- **Persistent memory's `memory_stored` count this run: zero.** Bootstrap queried memory but did not store a new summary at end-of-run. Possible regression in the manifest's end-of-run memory protocol, or the reasoner decided the run didn't warrant a new summary. Worth investigating — if memory isn't growing, the cross-run infrastructure isn't paying its keep.

## What Shipped
- No commits yet; the `spawn_with_tokens` fix is pending decision on whether to batch with manifest/memory changes or ship standalone.
- Artifact (`race_condition_bug_report.md`) exported to `/tmp/run10-artifacts/`. Workspace files stay local per the `/output/` gitignore policy.

## Cost
- Run 10 spend per dashboard: **~$0.07** (cumulative moved from $0.93 → $1.00). Consistent with Run 9 despite persistent-memory overhead and the tighter 3-agent chain.
- Cumulative per dashboard: **$1.00** all DeepSeek runs to date (~$1.16 all-in including earlier Anthropic runs). **Milestone: crossed $1.00 lifetime across ten self-reflection runs.**

## Design / Review Notes
- The **$1.00 milestone** is worth naming: ten self-reflection runs producing real fixes + external peer review, for the cost of a coffee. Comparable human code-review work would be orders of magnitude more expensive.
- **Open questions for Run 11:**
  1. Do we extend memory to carry structured "fixes-shipped" metadata so future runs can filter duplicates? Or accept that duplicates are cheap and the signal comes from filtering?
  2. Is the `spawn_with_tokens` gap a one-off we fix directly, or a signal that Fix 1 needed a broader grep for the pattern?
  3. Should the prompt stop saying "check git log" (agents can't), or should we add the capability?
