# Run 2 — Capability Revocation *(2026-04-13)*

**Integration commit:** `f1732d9` "feat: capability revocation — proposed by the system's own self-reflection" (22:07).

Same philosophical goal as subsequent reflection runs: *"What am I? What should I become? Build it."* Fresh container, zero memory, updated code from Run 1's fixes.

## What the Runtime Did

Bootstrap read `capability.rs` and reasoned about safety: "Without revocation, self-modification is too dangerous. With revocation, I can experiment safely." Produced ~49 KB of proposed Rust code for a revocation mechanism.

## What Shipped

The revocation mechanism — `revoked_at: Option<DateTime<Utc>>` on `CapabilityToken`, a `revoke()` method, `permits()` now checks revocation, and a `CapabilityRevoked` audit event. `revoke_capability()` and `revoke_all_capabilities()` methods on the registry.

Cost recorded at the time as **~$0.03** `[token-math estimate]`.
