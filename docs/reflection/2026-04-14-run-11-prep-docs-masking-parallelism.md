# Run 11 Prep: docs masking + parallel spawn_agents *(2026-04-14)*

Not a reflection run — preparation work for Run 11. Two concrete changes shipped:

**Integration commits:**
- `73b3653` runtime: `AgentSlot` reservation + centralized `remove_agent` + atomic insert; `run-aaos.sh` masks `docs/` and `README.md` from the container's view of `/src`.
- `04dc0c7` agentd: `spawn_agents` batch tool — parallel child spawning.

## Part A — Docs masking

Runs 8 and 10 surfaced the same problem: when the system has access to its own roadmap and `ideas.md`, its "original" findings are often restatements of those docs. The fix: at container launch, `run-aaos.sh` mounts an empty directory over `/src/docs` and `/dev/null` over `/src/README.md`, keeping the full-repo mount so new top-level files appear automatically but forcing agents to read **code and manifests only**. No kernel change — capability-based security doing what it's designed for, enforced at the filesystem layer via mount scope. Reversal: two lines.

Trade-off acknowledged: tests (`#[cfg(test)] mod tests`) remain visible because they live inside source files. That's correct behavior — tests are part of the code and show intent better than prose.

## Part B — Parallel child spawning

**The design took three Copilot review rounds** to reach something shippable. Each round caught real issues the previous draft had hand-waved:

- **Round 1** — original plan had mount masking via allowlist (brittle when new top-level files land), promised "all-or-nothing" batch semantics that the code didn't actually implement, called the cap a "concurrency cap" when it was per-batch, and described cleanup without specifying what happens when a JoinSet task panics. Pushback: pick a semantic model and commit to it.
- **Round 2** — v2 introduced `AgentSlot` RAII and `ReservationGuard`, but still had the `contains_key + insert` race open and still promised "all-or-nothing" where post-reservation failures would produce partial state. Pushback: use `DashMap::entry` to close the race; either build real two-phase commit or downgrade to honest best-effort.
- **Round 3** — v3 committed to **best-effort semantics** with centralized `remove_agent` and `DashMap::entry`. Copilot approved the design but flagged two implementation-level blockers: (a) the new tool can't live in `aaos-tools` because that crate can't depend on `aaos-llm` / runtime; (b) the sketch of `spawn_and_wait` didn't wire per-child cleanup after `agent_run` returned. Both fixed by placing the tool in `agentd/src/spawn_agents_tool.rs` and **delegating** to `SpawnAgentTool::invoke`, which already owns the scopeguard for per-child cleanup.

**What actually shipped:**
- `AgentSlot` RAII guard on `active_count`. `reserve_agent_slot()` increments; drop-without-commit releases; `commit()` transfers ownership to the agent's presence in the registry.
- `insert_atomic()` uses `DashMap::entry` vacant-check to close the duplicate-ID race.
- `remove_agent()` is the **only** method that removes from `self.agents` — every lifecycle exit path funnels through it, so `active_count` decrements in exactly one place. Invariant `active_count == agents.len()` enforced by construction.
- `spawn_agents` tool in agentd, delegating per-child spawn to the existing `SpawnAgentTool` so cleanup logic stays in one place. `AAOS_SPAWN_AGENTS_BATCH_CAP` env var (default 3) caps per-batch.
- Bootstrap manifest updated with `tool: spawn_agents` and guidance on when to use batch vs sequential spawn.

**Test coverage:** seven drift-focused registry tests covering the failure modes that would leak reservations (duplicate-ID rejection, over-limit spawn, stop_sync, uncommitted slot drop, committed slot retention). Full suite: 306 green.

## Design / Review Notes

Three-round peer review was expensive in wall-clock but cheap in failure-mode coverage. Each round caught issues that would have caused real bugs: the first caught a misleading semantic promise, the second caught a data race, the third caught two implementation-level mistakes (wrong crate, missing cleanup). **Pattern worth naming:** when a feature touches runtime admission control, a single review round is not enough — each round's fixes create new surface for the next round's critique.

The instinct to build transactional spawn (all children succeed or none) was wrong for v1. Best-effort with explicit per-child error fields is what `file_read_many` already does, what Bootstrap can reason about, and what we actually need. Transactional multi-spawn is a separate feature if a future workload demands it.
