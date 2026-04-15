# Run 3 — Constraint Enforcement *(2026-04-13)*

**Integration commit:** `f106d97` "fix: enforce max_invocations constraint — found by self-reflection v3" (22:34).

27 minutes after Run 2 wrapped. Fresh container, zero memory, updated code including Run 2's revocation feature.

## What the Runtime Did

Spawned a `capability-analyzer` child. It read constraints in the code and noticed: `max_invocations` was declared in `Constraints`, but `permits()` never checked it. The constraint was decorative, not enforced.

## What Shipped

`permits()` now checks `max_invocations` against `invocation_count`. `record_use()` increments the counter after successful operations. `is_exhausted()` helper. Tokens with exhausted invocation limits are denied.

Cost recorded at the time as **~$0.03** `[token-math estimate]`.

The three runs together (Run 1 + 2 + 3) were summarized at the time as "$0.11 for three real bugs found and fixed." `[token-math estimate — actual is likely lower due to DeepSeek caching; dashboard-authoritative cumulative for the whole day is $0.54 across everything that ran on DeepSeek]`.
