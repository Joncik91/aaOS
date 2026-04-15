# Run 9 — adversarial bug-hunt, seven real findings *(2026-04-14)*

**Integration commits (all shipped same day):**
- `1cd0520` runtime: wire IPC channels before publishing agent in registry
- `0338545` runtime: surface session-store failures + throttle audit spam
- `ad41f92` context: debug_assert summarization-boundary invariant
- `8eae910` memory_query: list valid categories in invalid-category error
- `5d2ac7e` audit: optional cap on InMemoryAuditLog (VecDeque, O(1) rollover)
- `3081620` file_read_many: document fail-fast on task panic
- `45418cc` capability: canonicalize requested paths to block symlink bypass
- `cdb1329` docs: track TOCTOU hardening as deferred idea

## Setup
- Memory state: fresh memory dir, `AAOS_PERSISTENT_MEMORY=1` set for first time (so any stored memories would survive restart).
- Goal: *adversarial* prompt asking for "a concrete bug, design flaw, or security issue in the current implementation. Produce a specific file:line report with reproduction steps or a failing test. Do not propose items already documented in /src/docs/ideas.md or /src/docs/roadmap.md — find something new."
- Rationale for the prompt shape: Run 8's philosophical prompt ended up rediscovering `ideas.md` rather than finding new ground. The adversarial wording + explicit "not already in docs" constraint was the hypothesis: force the system past its own docs into actual code.

## What Worked
- **Seven real bugs found, one false positive correctly identified.** All verified against source at commit `b31ef39`. The system didn't hallucinate — every finding pointed to a specific file:line that matched reality.
- **First run with persistent memory enabled end-to-end.** `memory_queried` audit event fired at startup (empty result, as expected on first-ever persistent run); the infrastructure exercised cleanly.
- **Four-child peer-review chain emerged again** (`code-scanner` → `issue-validator` → `doc-checker` → Bootstrap synthesis). Same pattern as Run 8 but with a bug-hunt decomposition: scan, validate findings, check against existing docs, synthesize.
- **The adversarial prompt worked.** Seven findings, zero roadmap rehash. The explicit "don't re-propose existing ideas" constraint turned out to be load-bearing.
- **One finding extended a Phase-A fix in a non-obvious way.** The path-traversal fix from Phase A used lexical normalization. Run 9 found that the same code was bypassable with symlinks. This is the kind of finding that requires actually reading the real code, not restating docs.
- **Two-tier peer review caught real issues in the proposed fixes.** Copilot/GPT-5.4 pushed back on five of the seven proposed fixes (non-atomic rewrite in Fix 2, `catch_unwind` hiding bugs in Fix 3, silent `.min()` clamp in Fix 5, `Vec::remove(0)` O(n) in Fix 6, canonicalize cache in Fix 4). All five pushbacks incorporated before implementation. Without peer review, we would have shipped subtle regressions.

## What the Run Exposed
- **One false-positive bug report** (out of eight claimed findings): the "agent state transition not synchronized" finding missed that `AgentProcess` is stored in `DashMap`, which synchronizes `get_mut` per entry. The `&mut self` signature forces exclusive access through that per-entry lock. Manual verification against source caught it. Lesson: system confidence scores can't substitute for verification; every finding needs source review.
- **Finding quality exceeded Run 8 by a wide margin.** Run 8: zero bugs, high-level proposals. Run 9: seven bugs, concrete file:line reports. Single prompt change (philosophical → adversarial) flipped the signal.
- **The symlink bypass is the most interesting finding.** Phase A's fix was correct *as specified* (block `..` traversal); Run 9 spotted that the *threat model* was incomplete (symlinks are another way to redirect). This is the kind of finding a fresh adversarial reviewer catches better than the original author.

## What Shipped
- Seven code commits + one docs commit. See "Integration commits" above.
- New audit event kind `SessionStoreError` (22 kinds total, was 21).
- Symlink bypass closed by canonicalizing requested paths; TOCTOU gap acknowledged in `docs/ideas.md`.
- `InMemoryAuditLog::with_cap(N)` opt-in cap for long-running test harnesses.

## Cost
- Run 9 spend per dashboard: **~$0.07** (cumulative moved from $0.86 → $0.93). In line with Run 8's $0.10 despite the adversarial prompt producing seven findings — DeepSeek cache discounts on repeated `/src/` reads across the 4-child chain.
- Cumulative per dashboard: **$0.93** all DeepSeek runs to date (~$1.09 all-in including earlier Anthropic runs).

## Design / Review Notes
- Two-tier review (self-verification of findings + Copilot review of fixes) took roughly one hour of implementation time including all compile/test cycles. The fixes themselves were bounded (≤200 LoC total across 7 commits). Net: seven real bugs closed for ~$0.07 of inference + one hour of review-and-fix work.
- Peer-reviewer pushback ratio was high (5 of 7 proposed fixes revised). Indicates (a) Copilot adds real value beyond rubber-stamping, and (b) LLM-proposed fixes still need expert review even when the *findings* are real.
- The adversarial-prompt + not-already-in-docs shape is worth codifying for future bug-hunting runs. Keep the philosophical prompt for roadmap-exploration runs. **Two distinct prompt shapes for two distinct signals.**
