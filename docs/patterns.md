# Patterns

Cross-cutting lessons distilled from the aaOS build history and the self-reflection log. Each one comes from observed failure or success, not speculation. Kept short — anything longer belongs in [`retrospective.md`](retrospective.md) (build history) or [`reflection/`](reflection/README.md) (per-run detail).

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

## Prompts persuade; only the kernel enforces

Between Runs 5 and 6 the Bootstrap manifest gained an explicit rule: *"Do NOT grant children `tool: memory_store`."* Run 6 confirmed Bootstrap granted it anyway — twice. Four orphaned memory writes landed in the store under ephemeral child IDs no future run could query. Run 5 had the same rule in commentary form; only Run 6 exposed it as unenforceable because the spawn path accepted whatever capabilities the LLM listed.

**If an invariant matters, the kernel has to be the one that says no.** Manifest prose is a teaching aid, not an access control layer. The fix from Run 6 (`505f559`) moved the rule from prose to a `CapabilityDenied` in `SpawnAgentTool` + a defense-in-depth check in `spawn_with_tokens`, with a runtime-owned `persistent_identity` flag so the invariant generalizes beyond Bootstrap.

The same lesson applies to any future constraint: path whitelists, budget caps, retry limits. If the manifest is the only thing stopping bad behavior, the LLM will eventually route around it.

## Fewer orchestration turns usually beats smarter orchestration

Run 7b used a 4-child chain (code-reader → analyzer → analyzer-with-source → writer). Profiling showed the chain itself — Bootstrap's digest/spawn turns between children, plus an unscoped proposer producing 5 intermediate documents — consumed ~8-10 minutes of a ~29-minute run. Copilot's Round-1 pushback on the Phase 1 speed plan: "In systems like this, fewer orchestration turns usually beats smarter orchestration." Shipped in `5be74ac` as a manifest-level chain trim (default 2 children now) plus output-scoping instructions.

The natural counter-move when a system feels slow is to parallelize the work; the better counter-move is often to remove steps that didn't need to exist. `spawn_agent`'s round-trip cost (child spawn + context growth + Bootstrap digest of the reply) is real and mostly invisible in the per-child cost numbers — it only shows up when you profile the whole chain.

**Rule of thumb:** before adding parallelism, auditing an orchestration chain by asking "what work would be lost if this step didn't exist" should be the default. In Run 7b, analyzer-#1 (option-ranking) and writer (final synthesis) both collapsed into Bootstrap's own turns without quality loss.

## Batch tools beat generic parallelism at the executor level

Copilot's Round-1 review of the Phase 1 speed plan flagged a broader principle: **don't change executor semantics to "parallelize any same-turn tool calls"**. Same-turn calls can be semantically dependent even when they look independent. The safer shape is a **tool-level opt-in** — ship batch tools that are known-safe (`file_read_many`, `spawn_agents`) and leave serial execution as the default for everything else. Explicit is better than speculatively-generic when the ordering contract matters for the LLM's downstream reasoning.

Shipped: `file_read_many` (Phase 1). Deferred: `spawn_agents` (needs atomic budget reservation + per-agent workspace guarantees the current registry doesn't provide).

## Audit events need structure to be useful, not just strings

The `ContextSummarizationFailed` audit variant has existed since Phase C with a single `reason: String` field. For most of that time it was also **unreachable** — `prepare_context()` caught summarization failures with `tracing::warn!` and silently fell back to uncompressed context, so the caller never got an `Err` to audit. Run 7's finding triggered the Commit B follow-up that surfaced the failure typed (`SummarizationFailureKind`: `llm_call_failed`, `empty_response`, `boundary_selection`, `reply_parse_error`) alongside the free-form reason, and wired the audit event path through the fallback branch. Operators now see `SUMM! [llm_call_failed] <message>` in the detail log instead of either nothing or an opaque string.

**Two lessons bundled here:**

1. **A log-level warn is not an audit event.** If a failure mode matters enough to have a dedicated audit variant, ensure the code path reaches it. Tracing spam is a debug aid; the audit stream is the structured record external tooling consumes. Don't swallow audit-worthy events in `tracing::warn` fallbacks.
2. **Prefer typed classifications to parsed strings.** Adding a `kind` enum alongside the existing `reason: String` was 20 lines of code and makes programmatic routing (retry this category, alert on that one) possible without regex over log text. `String` fields stay for humans; enums exist for machines. Both.

## Verify the binary before trusting a run

Docker build caches silently produced a binary without Fix 1/Fix 2 even though the commits were on the branch and the build timestamp post-dated them. First Run 7 attempt looked identical to Run 6 as a result; only a host-side `strings` check on the copied binary confirmed the fix text was missing. Rebuilt with `--no-cache`, confirmed the strings, and Run 7b showed completely different behavior.

**Discipline:** after any runtime code change, rebuild with `docker build --no-cache` and grep the binary for a known unique string from the change (e.g. an error-message literal) before launching the run. One layer between "committed code" and "code the container actually runs" is enough to invalidate an entire test run.

## Structured handoff beats opaque prompts for child-to-child data

Run 6's second bug: the `proposal-writer` was spawned with a goal string and no structured access to the `code-analyzer`'s findings. It dutifully `memory_query`'d (empty), then *confabulated* a generic proposal citing non-existent files, using phrases like "hypothetical based on common patterns" and "(or similar)" — plausible on the surface, disconnected from reality.

Two shapes fix this badly and one fixes it well:

- **Bad:** paste the prior child's output into the next child's `message`. Instructions and data collapse into one stream; prompt injection in the first output becomes executable in the second.
- **Bad:** have the parent paraphrase findings in prose into the next goal. Information loss, parent becomes a bottleneck, and the LLM is not trained to compress faithfully.
- **Good:** a separate field (Run 6 shipped `prior_findings` on `spawn_agent`) that the kernel wraps with kernel-authored delimiters + a prompt-injection warning. The parent LLM can't remove the warning; the child is told explicitly to treat the block as quoted input.

Caveat: a parent can still *fabricate* content in the field. This is continuity, not cryptographic provenance. The natural next upgrade is handoff-handles: pointers into the audit log that the child can verify, so a parent cannot forge findings from a child that never spoke. Noted as TODO; not built until a run actually needs it.

## Prompt shape determines the signal

Run 8 and Run 9 ran against the same codebase within the same week, both as self-reflection. Run 8 used *"What am I? What should I become? Build it."* and produced a roadmap-shaped proposal that mostly rediscovered `docs/ideas.md`. Run 9 used an *adversarial* prompt — "find a concrete bug or security issue, file:line report, no items already in docs/ideas.md or docs/roadmap.md" — and found seven real bugs, including a security fix that extended a Phase-A finding.

Same system, same code, same LLM, same capability model. The only thing that changed was the prompt. The philosophical prompt biases toward synthesis of existing documentation; the adversarial prompt biases toward independent code reading.

**Consequence:** treat prompt shape as a signal-selection knob. Keep both shapes in the rotation — roadmap-exploration runs for forward-looking ideas, adversarial runs for bug-finding. Don't expect one prompt to do both jobs.

## LLM-proposed fixes need review even when the findings are real

Run 9 found seven real bugs — all verified against source. But when the system proposed fixes, a second peer review (Copilot/GPT-5.4) pushed back on five of the seven. Pushbacks included: non-atomic clear+append still drops data on partial write; `catch_unwind` would hide bugs that should fail loud; silent `.min()` clamp papers over broken invariants; `Vec::remove(0)` is O(n); canonicalize caching stale metadata is the wrong failure mode for a security boundary.

None of these were "the finding is wrong." All were "the proposed fix has a subtle issue." Without the second review, we would have shipped working-but-subtly-regressive code for five of the seven bugs.

**Consequence:** self-reflection produces findings; external review is still needed to produce safe fixes. The two roles are not interchangeable, and the review is cheap compared to debugging a regression later.

## Public claims should match the code the reader finds

Docs can overclaim without anyone noticing — the README said "unforgeable tokens replace permissions" but `CapabilityToken` is a plain `#[derive(Serialize, Deserialize)]` Rust struct. Technically "only the runtime issues them," but *unforgeable* is a specific term in capability-security literature that implies cryptographic or type-system guarantees aaOS does not provide. A security-focused reader reading the code after reading the README would lose trust in the whole project.

The audit surfaced seven overclaims: "unforgeable tokens," "runtime guarantee" of filesystem isolation, "zero-permission default" (agents vs the runtime itself), "typed MCP messages" (schema validation is partial), "every action logged as a runtime guarantee" (durability depends on backend), "inference is a schedulable resource" (concurrency-limited, not scheduled), and "self-designing capability" (drafting, not designing).

Rule: **claims in README.md and architecture.md must be defensible when a reader does `grep` against the source the next minute**. Not aspirational. Aspirational claims belong in `roadmap.md` or `ideas.md` with the concrete gap named. If the code gets stronger later, upgrade the wording; don't pre-upgrade it and hope.

Derived from a post-Run-10 audit (commit after 2026-04-14) that replaced the seven claims above with honest phrasing and added the gaps to `ideas.md` as deferred hardening items.

**Follow-up — the upgrade cycle worked.** Within two days of the audit, one of the deferred gaps (handle-based capability tokens) was closed (commits `14a8eae` + `18d14f0`). The README's capability-security paragraph was rewritten from *"runtime-issued, narrowed-only tokens"* to *"runtime-issued, handle-opaque tokens... a forged handle either resolves to nothing or to a token owned by a different agent"* — a strictly stronger claim that the code now backs. The HMAC-signing piece is still deferred (it's for cross-process transport, not in-process forgery) and stays named as such in `ideas.md`. That's the pattern in action: audit honestly → document the gap → close it when it matters → upgrade the wording. Don't pre-upgrade.

## Runtime admission control needs more than one review round

Features that change how agents are admitted or cleaned up (registry reservation, batch spawn, atomic counters) consistently need **three peer-review rounds** to stabilize. Each round catches a different class of issue:

- Round 1: semantics and threat model. Does "all-or-nothing" actually mean that? Is this cap a real cap or documentation?
- Round 2: concurrency primitives. Are the compare-and-swap loops correct? Is `contains_key + insert` a race? Who decrements the counter on error paths?
- Round 3: implementation-layer mistakes. Can this tool actually compile where you put it? Does the cleanup path you described exist?

Single-round review misses at least one of these tiers. The shape of the mistake changes after each round of fixes: round-2 fixes create round-3 surface for critique. Cost is ~1-2 minutes per round of Copilot time, plus the wall-clock of the author reading the review. Worth it every time — each caught issue is a real bug that would have shipped.

Derived from Run 11 prep (commits `73b3653`, `04dc0c7`): three rounds to land `spawn_agents`, each round caught a distinct class of problem.

## Self-reflection is an adversarial reviewer, not a bug-finder

Across ten runs, the system has shipped ~14 fixes. Honest audit of what they actually were:

- **~2 fixes** addressed bugs a user would have noticed in production (Run 6's writer confabulating with no `prior_findings`; Run 7's silent audit-event drop on summarization failure).
- **~12 fixes** addressed *invariants* — code patterns that were correct today but fragile: ordering bugs that hadn't yet tripped because the fallible step didn't exist yet, security gaps that weren't exploited because no agent had tried, hardening around paths not currently exercised.

This is a useful role, but it's a narrower claim than "autonomous bug-finder." What the system is *doing* is static code review against a threat model and a set of design invariants. What it can *not* do: find bugs that require running the code, performance issues, UX friction, integration problems with real providers, or anything where the symptom is observable rather than structural.

**The honest framing:** self-reflection produces a cheap adversarial reviewer for security and invariant code. Not a fuzzer, not an integration-test suite, not a substitute for production telemetry. Worth the ~$0.07/run cost for a capability-based runtime where invariants matter before they fire — but don't overclaim it as general bug discovery.

## Security fixes have threat-model shelf lives

Phase A's path-traversal fix blocked `..` traversal via lexical normalization. Correct for the threat model as written at the time. Run 9 showed the threat model was incomplete — symlinks are another way to redirect a path, and the lexical fix doesn't touch them.

The fix wasn't buggy; the *specification* was incomplete. A fresh adversarial reviewer with full code access catches this kind of gap better than the original author, whose mental model is anchored to the original threat statement.

**Consequence:** re-audit security-critical code *periodically* against a fresh threat model, not just against the original spec. The cost is small; the payoff is catching class-of-bug extensions before they're exploited.

## Prove substrate-agnostic abstractions with a second implementation

`AgentServices` was documented as substrate-agnostic from early in the
project. With exactly one implementation (`InProcessAgentServices`), that
claim was unfalsifiable — the trait could have any number of hidden
assumptions about in-process semantics and no one would know. The same
was true of `CapabilityToken` as "an opaque handle model" when nothing
in the code forced opacity.

Both were proven only when a genuinely different second implementation
forced the trait to bend:
- The handle-based token migration (commits `14a8eae`, `18d14f0`) made
  `CapabilityToken` mutable state registry-owned instead of agent-owned.
  Until a caller actually held `Vec<CapabilityHandle>` and had to ask the
  registry for permits, the token model was theory.
- The `NamespacedBackend` scaffolding (commits `a84cd98`, `a73e062`)
  made `AgentServices` bend to a real cross-process boundary with its
  own threat model, readiness semantics, and failure modes. Each piece
  that had to stretch — opaque handle data, serializable launch spec,
  self-applied sandbox inside the worker — was a place the original
  trait had quietly assumed the in-process case.

Rule: before committing to a distribution or product shape that relies
on an abstraction being substrate-agnostic, ship a second backend. It
doesn't have to be production-ready — the scaffolding alone reveals
whether the trait was honest. Peer review of the design catches the
architectural mistakes; the second implementation catches the silent
assumptions no review can see.
