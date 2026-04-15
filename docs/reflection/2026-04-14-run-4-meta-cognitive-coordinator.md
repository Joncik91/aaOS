# Run 4 — First Skill-Driven Run; Meta-Cognitive Coordinator Proposal *(2026-04-14)*

**Followed by (response) commits:**
- `c2f8a49` "docs: rewrite retrospective" (09:27) — unrelated cleanup, not part of run 4 itself
- `46fd8ca` "feat(tools): add file_list tool to fix agent path-guessing" (10:02)
- `0c58052` "feat(bootstrap): minimal cross-run learning — stable ID, memory volume, query/store protocol" (10:32)
- `705ff62` "docs: update README + architecture + roadmap + retrospective for run 4" (10:50)

Run 4 was the first where the Bootstrap Agent actually used skills as executable knowledge — the `66542bf` fix from the night before finally took effect.

## Setup

Fresh container. No persistent memory (still opt-in, wasn't enabled). Same philosophical goal.

## What the Runtime Did

Loaded `idea-refine` first (matching the philosophical goal), then `planning-and-task-breakdown`. Followed the skills' workflows: map current state, propose directions, evaluate. Spawned 3 children. Produced 4 workspace files totaling ~30 KB:

- `idea.md` — Bootstrap's own reflection: 5 possible evolution directions
- `analysis.md` — "Who am I" analysis (child 1)
- `evolution-design.md` — MVP design picking the Meta-Cognitive Coordination Layer direction, from the architect child
- `technical-analysis.md` — Phase 1 implementation design with real Rust types, new `aaos-reflection` crate proposal, an 8-week implementation plan

## The 8-Week Plan Problem

The implementation design used human-software-project timeframes: "Phase 1a: Foundation (Week 1-2), Phase 1b: Integration (Week 3-4)", etc. These are **pattern-matched LLM outputs, not real estimates** — the agent had no access to its own runtime cost or to the operator's available hours. It produced the shape of a planning document because planning documents look that way.

For calibration: the minimal version of that 8-week plan (stable Bootstrap ID + memory volume + manifest protocol + guardrails + tests) shipped in roughly **30-45 minutes of Claude Opus work** the next morning (`0c58052`). The "weeks" language in agent-proposed plans should be read as placeholder structure, not a time estimate.

## Observed Failure: Path Guessing

12 of 50 `file_read` attempts failed. Two patterns:

- 4 cases: agent called `file_read` on a directory (e.g. `/src/`, `/src/crates/`) to explore — `file_read` needs a file, fails.
- 4 cases: agent guessed filenames that don't exist (`bootstrap_manifest.yaml`, `spawn_agent.rs` — actual names were different).

Same root cause: no way to list directory contents. Agents pattern-matched on Rust conventions and invented paths.

## What Was Shipped After Review

Two independent reviews of `evolution-design.md` + `technical-analysis.md` (Claude Opus 4.6 inline, then Copilot CLI GPT-5.4 with codebase access) concluded the direction was right but the design was premature.

Technical objections confirmed by both reviewers:
- New `aaos-reflection` crate was unjustified — existing `SqliteMemoryStore` + `memory_store` + `memory_query` can handle it.
- `impl Tool for PatternTool` used the wrong trait signature — would not have compiled against `aaos-tools::Tool`.
- `CoordinationPattern { success_rate, usage_count, last_used }` was a Phase 2/3 data model pretending to be MVP.
- The proposal ignored the real blocker: Bootstrap gets a fresh `AgentId` every boot, so persistent memory is orphaned between runs.

Shipped instead (a minimal empirical version):

1. **`file_list` tool** (commit `46fd8ca`). Directory listing (or file metadata), capability-gated by `FileRead` (same glob, same path normalization). 5 new unit tests. Fixed the path-guessing problem.
2. **Stable Bootstrap ID** (commit `0c58052`). `AgentId::from_uuid()` kernel-only constructor + `AgentRegistry::spawn_with_id()` + `AAOS_BOOTSTRAP_ID` env / `/var/lib/aaos/bootstrap_id` file. Makes cross-run memory meaningful for Bootstrap specifically; other agents' IDs remain fresh per-spawn. 1 new test.
3. **Persistent memory, opt-in** (same commit). `AAOS_PERSISTENT_MEMORY=1` bind-mounts host memory dir. `AAOS_RESET_MEMORY=1` wipes DB + ID file on boot.
4. **Memory protocol in manifest** (same commit). Bootstrap told to `memory_query` before decomposing a goal, `memory_store` a compact run summary after completion, with explicit guidance on what NOT to persist.

**Cost recorded at the time:** ~$0.48 `[token-math estimate; not reproducible from dashboard — DeepSeek caching discount applies]`.
