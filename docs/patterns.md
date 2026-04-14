# Patterns

Cross-cutting lessons distilled from the aaOS build history and the self-reflection log. Each one comes from observed failure or success, not speculation. Kept short — anything longer belongs in [`retrospective.md`](retrospective.md) (build history) or [`reflection-log.md`](reflection-log.md) (per-run detail).

---

## LLM calendar estimates are pattern-matched, not real

When a runtime agent proposes an implementation plan, it produces "Phase 1 (Weeks 1-2), Phase 2 (Weeks 3-4)" language because planning documents look like that — not because the agent has any access to wall-clock effort. Run 4's "8-week" Meta-Cognitive Coordinator plan shipped in ~30–45 minutes of Claude Opus work. Read agent-proposed timeframes as placeholder structure, not estimates.

**Corollary:** ask for the shape, do the sizing yourself.

## Cost from token math ≠ cost from dashboard

Early notes quoted per-run costs ("~$0.02", "~$0.48", "~$0.11 total") computed from `docker logs` token counts at a flat provider rate. These are unreliable for DeepSeek because context caching discounts cache-hit input tokens to ~10% of normal. A persistent Bootstrap re-sending its growing history gets massive cache hits.

**The provider dashboard is authoritative. Token math is a rough ceiling, not the actual spend.** Always note in docs when a cost figure is estimated vs verified.

## Skill adherence evolves run-to-run

Four observed postures:

- **Under-using** (runs 1-3): skill catalog in prompt, never called `skill_read`, named agents after skills.
- **Over-trusting** (run 4): loaded skills as executable knowledge, applied every step without checking fit.
- **Rigid** (run 5): followed each skill's workflow mechanically, ignored the skill's own "When NOT to use" section — doubled runtime without proportional quality gain.
- **Judgment-based** (post-run-5 manifest tuning): load, read applicability, apply or skip.

The middle path isn't in the skill — it's in how the agent is told to read skills. Put the applicability check in the manifest explicitly.

## Agent-proposed designs need external review

Self-review catches conceptual issues but misses:

- Compile errors against the real codebase (wrong trait signatures, undefined types).
- Duplication with existing code (proposed `PatternStore` duplicates `SqliteMemoryStore`).
- Architecture-level mistakes (ignoring the real blocker, like Bootstrap's ephemeral `AgentId`).

External LLM review (Copilot CLI + Qwen CLI have both proven useful) caught every class of these in runs 4 and 5. Combining *agent self-review* + *external reviewer with codebase access* + *human filter* is the cheapest path to catching mistakes before they ship. Peer-review cost is negligible compared to debugging cost.

## Runtime self-reflection works best on code, not docs

Runs 2-3 found real bugs because they read the actual `.rs` files and noticed gaps between declared constraints and enforced constraints. A parallel run tried to reason from docs alone and concluded that features didn't exist — because the architecture doc hadn't been updated for the previous phase. The runtime's self-knowledge is only as good as its documentation.

**Prefer code as the ground truth.** If docs are stale, fix them or tell the agent to ignore them.

## Persistent agents need stable identity; ephemeral ones don't

Run 5 exposed this by letting the Bootstrap Agent's memory persist across restarts and watching children orphan their writes. Children have fresh UUIDs every spawn — their `memory_store` calls are tagged with an agent_id no future query will match. Only the long-lived agent benefits from long-lived memory.

**Design consequence:** give persistent agents a persistent ID; keep ephemeral agents ephemeral; have children *report* to the persistent one instead of writing to shared state directly. That's what aaOS's manifest now enforces via prompting (children no longer get `tool: memory_store`).

## Run length trades off with quality

Run 4 (~12 min, no memory protocol, dove in): fast, strong ideas, non-compiling code artifacts.
Run 5 (~30 min, skill-driven + memory protocol, planned first): slow, grounded artifacts, better direction.

Both have their place. The manifest now explicitly tells Bootstrap to skip the planning dance for simple goals and apply it to multi-agent work. Blanket rules either way waste either quality or time.

## The capability system catches real mistakes in real time

Run 5 had the Bootstrap Agent try to spawn `pattern-implementer` with `file_write: /src/*`. The parent⊆child enforcement refused because Bootstrap itself doesn't hold `file_write: /src/*`. Bootstrap recovered with `file_write: /data/workspace/…/*`.

That's not a bug log entry — that's the capability system doing what it was built for. Each time this happens in production, it's evidence that the "you can only give what you have" rule is load-bearing.

## Over-building is the new failure mode

Early reflection runs under-built (skills as naming, no memory protocol). Later ones over-build: Run 4's 8-week plan with a new crate nobody needed, Run 5's pattern-builder child producing the same logic in JavaScript *and* Python even though neither language runs in the container.

The signal: once a runtime can reason about its own code, it can generate plausible-looking plans faster than a human can sanity-check them. The manifest fix for this is the "don't produce the same thing in multiple languages" heuristic — small symptom of a bigger pattern. The broader discipline is the same as for Phase A: **design, peer-review, then build; not build, build, build.**

## Docs drift faster than code

Multiple times this project has caught docs reporting stale numbers (crate counts, line counts, test counts, cost figures). The retrospective itself was rewritten once already to fix contradictions, then amended again to correct cost math.

**Ground truth is git + the provider dashboard.** When docs and code disagree, trust the code. When docs and dashboard disagree, trust the dashboard. Update docs when you notice drift — don't let it compound.
