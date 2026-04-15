# Run 7 Follow-up: Phase 1 speed work *(2026-04-14, same day, commit `5be74ac`)*

Run 7b took ~29 minutes and cost ~$0.16 per dashboard. Profiling the timeline showed the cost concentrated in three places: a 4-child orchestration chain (2.5 min of Bootstrap digest/spawn overhead), a ~4-minute sequential `file_read` loop inside the scanner, and an analyzer that produced 5 unscoped intermediate documents. We did a grounded research pass — surveying external work on DeerFlow 2.0's parallel-subagents pattern, Claude Code's Agent tool, the TB-CSPN deterministic-orchestration paper, and the Repository Intelligence Graph research — then sent a plan to Copilot for review. Round 1 pushback:

- Don't make executor-level parallelism generic — it's too broad, many same-turn tool calls are semantically dependent. Instead use a **tool-level opt-in** (batch tools) or a whitelist.
- The biggest wins in a system like aaOS come from **fewer orchestration turns**, not smarter orchestration. "Trim the chain" is the best single idea.
- For the 75% stretch target, honest assessment: "possible but not as a base-case expectation." Requires multiple structural wins (batched repo access + deterministic routing for common goals).

Revised plan shipped in `5be74ac`:

- **`file_read_many` batch tool** (aaos-tools): up to 16 paths per call, parallel read via `tokio::task::JoinSet`, per-file capability check, partial-failure-ok. Replaces sequential `file_read` loops in scan phases. 7 new unit tests.
- **Bootstrap manifest trim**: default chain is now 2 children (code-reader → proposer). Bootstrap synthesizes the final user reply itself — no separate `writer` child unless output genuinely spans multiple artifacts.
- **Output scoping**: proposer's message template now requires exactly one file with explicit sections (problem / solution / code sketch / risks / tests). Prohibits the Run 7b 5-document sprawl.
- Bootstrap manifest also teaches the LLM to prefer `file_read_many` when the file set is known upfront.

**Expected Run 8 saving:** ~35-45% off the ~29-minute Run 7b baseline. Primary levers: chain trim (~5-6 min), output scoping (~3-4 min), file_read_many (~2-3 min on scan phase).

**Deferred per Copilot review:**
- Generic executor-level parallelism (needs per-tool `parallel_safe` classification first)
- `spawn_agents` batch (needs atomic budget reservation + stronger per-agent workspace guarantees)
- RIG / deterministic decomposition for "scan + propose" goals (Phase 2 candidates — evaluate only after we measure Phase 1's actual effect)

**External research surveyed during planning** (ByteDance DeerFlow 2.0 multi-agent architecture, TB-CSPN deterministic orchestration, Repository Intelligence Graph studies, plus the Anthropic Claude Code Agent-tool pattern). The key takeaway: **the agentic tax concentrates at the orchestrator, and deterministic rule-based coordination can cut API calls ~67% at the cost of adding a control plane**. Phase 1 does not touch the control plane yet; Phase 2 might.
