# Plan: Handle-Based Capability Tokens

**Status:** reviewed and revised, ready to implement
**Author:** Claude Opus 4.6 (1M context)
**Peer-reviewed by:** Copilot (GPT-5.4) — two rounds; key findings incorporated, see "Peer Review Notes" at the bottom
**Target branch:** main
**Estimated scope:** ~400 LoC production + ~200 LoC test, 3 commits, ~1 day of work
**Dependencies:** none (no new crates, no key management)

---

## Why

Post-audit (commit `e4c8058`) we documented that `CapabilityToken` is a plain Rust struct with `#[derive(Serialize, Deserialize)]`. Agents can't construct tokens today because their only interface is LLM tool-calls routed through a registry that issues tokens for them. But anyone with Rust-level code execution inside `agentd` — a compromised tool implementation, a memory-corruption bug, a future third-party tool plugin — can fabricate `CapabilityToken` struct literals trivially.

This plan closes the most-likely forgery path: **third-party tool code running in the same process**. It does NOT aim for cryptographic unforgeability across process boundaries (that's HMAC-signing territory, deferred).

The strategy: make `CapabilityToken` **opaque to tool code**. Tools receive an opaque `CapabilityHandle`, not a `CapabilityToken`. The runtime resolves handle→token inside its own call boundary; tools never touch the token struct directly.

This is the minimal change that meaningfully improves the forgery story and lets us upgrade README language honestly.

---

## What changes

### New primitive: `CapabilityHandle`

A new public type in `aaos-core::capability`:

```rust
/// An opaque handle that refers to a capability token held by the runtime.
///
/// Agents and tool implementations receive handles, not tokens. Only the
/// runtime (specifically `CapabilityRegistry::resolve()`) can produce a
/// `&CapabilityToken` from a handle. A tool implementation cannot construct
/// a valid handle that maps to a useful token, because the inner u64 is an
/// index into a runtime-held table — a forged handle either resolves to
/// `None` (no such index) or to a token issued for a different agent.
///
/// Not cryptographic. Still vulnerable to attackers who can read the
/// runtime's capability table directly (e.g. /proc/<pid>/mem on the host).
/// Full HMAC-signed tokens are a separate, deferred hardening item tracked
/// in docs/ideas.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CapabilityHandle(u64);
```

### New primitive: `CapabilityRegistry` (authoritative for ALL mutable token state)

**Revision after peer review:** the registry is not just a lookup table.
It is the **source of truth** for token mutable state (invocation counts,
revocation). Tool code never clones a token; it calls `authorize()` on the
registry, which atomically performs the permit check AND records use.

Placement: this lives in `aaos-runtime`, not `aaos-core`. This lets the
registry's mutation methods be `pub(crate)` to `aaos-runtime` while the
`CapabilityHandle` type and resolution surface stay in a shared public
module. (See "Crate layout" below.)

```rust
/// Runtime-owned table of issued capability tokens. Agents and tools hold
/// `CapabilityHandle` values; the underlying `CapabilityToken` and its
/// mutable state are never exposed outside runtime code.
pub struct CapabilityRegistry {
    // Handle → (agent_id, token). The registry owns tokens; mutations
    // (record_use, revoke) happen through registry methods that acquire
    // the per-entry lock via DashMap.
    table: DashMap<CapabilityHandle, OwnedEntry>,
    next_id: AtomicU64,
}

struct OwnedEntry {
    agent_id: AgentId,
    token: CapabilityToken,  // mutable inside the entry's lock
}

impl CapabilityRegistry {
    pub fn new() -> Self { /* ... */ }

    // ------- Issuance (runtime-only; used by AgentRegistry) -------

    /// Issue a handle for a token. Called from AgentRegistry::issue_capabilities.
    pub(crate) fn insert(&self, agent_id: AgentId, token: CapabilityToken) -> CapabilityHandle {
        let h = CapabilityHandle(self.next_id.fetch_add(1, Ordering::AcqRel));
        self.table.insert(h, OwnedEntry { agent_id, token });
        h
    }

    /// Narrow: produce a new handle for a narrowed copy of the parent's token,
    /// owned by the child agent.
    pub(crate) fn narrow(
        &self,
        parent_handle: CapabilityHandle,
        parent_agent: AgentId,
        child_agent: AgentId,
        additional: Constraints,
    ) -> Option<CapabilityHandle> {
        let narrowed = {
            let entry = self.table.get(&parent_handle)?;
            if entry.agent_id != parent_agent { return None; }
            entry.token.narrow(additional)
        };
        Some(self.insert(child_agent, narrowed))
    }

    // ------- Authorization (the hot path — tools call this) -------

    /// Atomic permit-check. Does NOT count as usage; use `authorize_and_record`
    /// for the tool-invocation path. Returns whether the handle belongs to
    /// `requesting_agent` AND holds a non-revoked, non-exhausted token that
    /// permits the requested capability.
    pub fn permits(
        &self,
        handle: CapabilityHandle,
        requesting_agent: AgentId,
        requested: &Capability,
    ) -> bool {
        let Some(entry) = self.table.get(&handle) else { return false };
        if entry.agent_id != requesting_agent { return false; }
        entry.token.permits(requested)
    }

    /// Atomic permit + record-use. This is what tool implementations should
    /// call when invoking a capability — it ensures max_invocations counts
    /// are consumed exactly once per successful check. Returns `Ok(())` if
    /// allowed (and increments invocation_count), `Err(reason)` otherwise.
    pub fn authorize_and_record(
        &self,
        handle: CapabilityHandle,
        requesting_agent: AgentId,
        requested: &Capability,
    ) -> Result<(), CapabilityDenied> {
        let mut entry = self.table.get_mut(&handle).ok_or(CapabilityDenied::UnknownHandle)?;
        if entry.agent_id != requesting_agent {
            return Err(CapabilityDenied::WrongAgent);
        }
        if !entry.token.permits(requested) {
            return Err(CapabilityDenied::NotPermitted);
        }
        entry.token.record_use();
        Ok(())
    }

    // ------- Mutation (runtime-only) -------

    /// Revoke by token_id (the UUID on CapabilityToken). Matches the current
    /// AgentRegistry::revoke_capability signature.
    pub(crate) fn revoke(&self, token_id: Uuid) -> bool {
        let mut revoked = false;
        for mut entry in self.table.iter_mut() {
            if entry.token.id == token_id {
                entry.token.revoked_at = Some(Utc::now());
                revoked = true;
            }
        }
        revoked
    }

    /// Revoke every token owned by the given agent. Used on capability-wipe
    /// and (optionally) on agent removal.
    pub(crate) fn revoke_all_for_agent(&self, agent_id: AgentId) -> usize {
        let mut count = 0;
        for mut entry in self.table.iter_mut() {
            if entry.agent_id == agent_id && entry.token.revoked_at.is_none() {
                entry.token.revoked_at = Some(Utc::now());
                count += 1;
            }
        }
        count
    }

    /// Remove all handles belonging to an agent. Called from
    /// AgentRegistry::remove_agent, AFTER audit events for any revocations
    /// have been recorded.
    pub(crate) fn remove_agent(&self, agent_id: AgentId) {
        self.table.retain(|_, entry| entry.agent_id != agent_id);
    }

    /// Read-only inspection for tests and debug. Does NOT return the token
    /// in a form tool code can use — returns a snapshot of fields relevant
    /// for testing (id, agent_id, revoked_at, invocation_count). Keeps
    /// CapabilityToken out of the public API.
    #[cfg(any(test, debug_assertions))]
    pub fn inspect(&self, handle: CapabilityHandle) -> Option<CapabilitySnapshot> {
        let entry = self.table.get(&handle)?;
        Some(CapabilitySnapshot {
            token_id: entry.token.id,
            agent_id: entry.agent_id,
            revoked: entry.token.revoked_at.is_some(),
            invocations_used: entry.token.invocation_count,
        })
    }
}

/// Why an authorization failed. Included so tools can log or return a
/// specific denial reason without holding a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityDenied {
    UnknownHandle,
    WrongAgent,
    NotPermitted,
    Exhausted,  // (max_invocations reached)
    Revoked,
}
```

### Crate layout (revised)

`CapabilityHandle` lives in `aaos-core::capability` (public type,
cross-crate-friendly, derive Serialize/Deserialize). `CapabilityRegistry`
and all its mutation methods live in `aaos-runtime::capability_registry`.
Rationale: registry mutation APIs should be `pub(crate)` to `aaos-runtime`
so only runtime code can insert/narrow/revoke. `aaos-tools` takes only a
`&CapabilityRegistry` reference (via `InvocationContext`) and can call
`permits()` and `authorize_and_record()` — the read-only surface.

If later we need a third crate to own the registry (e.g. when Phase G
backends ship), move the struct; the public `CapabilityHandle` type doesn't
need to move.

### Change: `AgentProcess.capabilities` holds handles, not tokens

```rust
// BEFORE:
pub capabilities: Vec<CapabilityToken>,

// AFTER:
pub capabilities: Vec<CapabilityHandle>,
```

### Change: `InvocationContext` holds handles + a registry reference

```rust
// BEFORE:
pub struct InvocationContext {
    pub agent_id: AgentId,
    pub tokens: Vec<CapabilityToken>,
}

// AFTER:
pub struct InvocationContext {
    pub agent_id: AgentId,
    pub tokens: Vec<CapabilityHandle>,
    // Arc because Tool::invoke is async and may outlive the caller's stack;
    // the registry is runtime-lifetime anyway. Clone is cheap (atomic bump).
    pub capability_registry: Arc<CapabilityRegistry>,
}
```

### Change: tool implementations ask the registry, don't clone tokens

Every tool that today writes:

```rust
let allowed = ctx.tokens.iter().any(|t| t.permits(&requested));
```

...changes to one of two shapes:

**For check-only paths (most tools):**
```rust
let allowed = ctx.tokens.iter().any(|h| {
    ctx.capability_registry.permits(*h, ctx.agent_id, &requested)
});
```

**For tool-invocation paths where `max_invocations` should count:**
```rust
let mut authorized = false;
for h in &ctx.tokens {
    match ctx.capability_registry.authorize_and_record(*h, ctx.agent_id, &requested) {
        Ok(()) => { authorized = true; break; }
        Err(CapabilityDenied::NotPermitted) | Err(CapabilityDenied::WrongAgent)
            | Err(CapabilityDenied::UnknownHandle) => continue,
        Err(e) => {
            // Exhausted or Revoked — the handle matched but is unusable.
            // Record the specific denial and stop (no other handle will help).
            return Err(CoreError::CapabilityDenied { ... });
        }
    }
}
```

The tool never sees a `CapabilityToken`. Invocation counting happens inside
the registry's per-entry lock — no race between check and record.

### Remove `#[derive(Serialize, Deserialize)]` from `CapabilityToken`

`CapabilityToken` becomes a runtime-internal type that tool code never sees. Removing the derives prevents accidental serialization leaks (e.g. a tool that logs `ctx` accidentally dumping the whole token). It also means the struct can't trivially cross process boundaries — a non-goal today but a good discipline.

Handles stay serializable (they're just `u64`).

---

## What explicitly does NOT change

- **`Capability` enum** stays exactly as it is. It's the policy, not the token. Serialization of capabilities (for manifests) unchanged.
- **`Constraints` struct** unchanged.
- **Audit events** continue to log `CapabilityToken.id` (the UUID). Operators see what was granted, not a handle number. (Handle numbers are internal; they'd be noise in the audit trail.)
- **Manifest format** unchanged. Users still declare `"file_read: /data/*"`; the runtime turns that into a token with a handle.
- **`permits()` logic on `CapabilityToken`** unchanged — it's still where path-glob / tool-name matching lives. But tool code no longer calls it directly; the registry calls it internally inside the per-entry lock.
- **Narrowing semantics** unchanged. `CapabilityToken::narrow()` still produces a narrower token; `CapabilityRegistry::narrow()` wraps the narrowed token in a new handle owned by the child agent.
- **Revocation semantics** unchanged from the agent's perspective (revoked handle → operation denied). The mechanism moves: revocation flips the `revoked_at` field on the registry-held token, and subsequent `authorize_and_record()` calls return `CapabilityDenied::Revoked` instead of succeeding. `AgentRegistry::revoke_capability` and `revoke_all_capabilities` delegate to the registry's `revoke()` / `revoke_all_for_agent()`. Audit events stay exactly the same (`CapabilityRevoked` with the token's UUID).

---

## Commit sequence

### Commit 1 — primitives (`CapabilityHandle` + `CapabilityRegistry`)

**Files touched:**
- `crates/aaos-core/src/capability.rs` — add `CapabilityHandle` (public), `CapabilityDenied` enum, `CapabilitySnapshot` (debug/test helper).
- `crates/aaos-runtime/src/capability_registry.rs` (new) — `CapabilityRegistry` struct and methods. Internal mutation APIs `pub(crate)` to `aaos-runtime`; `permits` and `authorize_and_record` are the only `pub` methods that cross the crate boundary.
- `crates/aaos-runtime/src/lib.rs` — `pub mod capability_registry;` export.

**Tests added (~80 LoC):**
- `authorize_records_use_atomically` — N concurrent `authorize_and_record` calls against a token with `max_invocations = M` succeed exactly M times.
- `permits_does_not_record_use` — calling `permits` N times does not consume the invocation budget.
- `authorize_rejects_wrong_agent` — handle issued for agent A returns `WrongAgent` when requested by agent B.
- `authorize_rejects_unknown_handle` — fabricated handle returns `UnknownHandle`.
- `revoke_by_token_id_denies_future_authorize` — token revoked mid-flight, next call returns `Revoked`.
- `revoke_all_for_agent_affects_only_that_agent` — multi-agent test.
- `narrow_creates_distinct_handle_for_child` — narrowed handle is new, ownership is child_agent.
- `remove_agent_drops_all_its_handles` — after `remove_agent(A)`, no handle belonging to A resolves.

**No other code paths updated in this commit.** `AgentRegistry` and tools continue to use the old `Vec<CapabilityToken>` shape. Shipping the primitive in isolation scopes the blast radius of any regression in commit 2.

### Commit 2 — wire `CapabilityRegistry` through the runtime (atomic)

**Files touched:**
- `crates/aaos-core/src/capability.rs` — remove `Serialize`/`Deserialize` from `CapabilityToken`. Keep them on `Capability`, `Constraints`, `CapabilityHandle`, and `CapabilityDenied`. Any test or helper that serialized a token must either use `CapabilitySnapshot` (debug inspection) or be removed if it was only demonstrating serialization.
- `crates/aaos-runtime/src/registry.rs` — `AgentRegistry` gains `capability_registry: Arc<CapabilityRegistry>`. `issue_capabilities()` inserts each issued token and stores the returned handles in `AgentProcess.capabilities`. `remove_agent()` calls `capability_registry.remove_agent(id)` after the `AgentStopped` audit event but before the DashMap remove. `revoke_capability` and `revoke_all_capabilities` delegate to the registry's methods.
- `crates/aaos-runtime/src/process.rs` — `AgentProcess.capabilities: Vec<CapabilityHandle>`.
- `crates/aaos-tools/src/invocation.rs` — `InvocationContext` gains `capability_registry: Arc<CapabilityRegistry>`. Construction site in `ToolInvocation::invoke` threads it through from `AgentRegistry`.
- `crates/aaos-tools/src/context.rs` — same `InvocationContext` struct if that's the actual home; ensure consistency.
- Every tool implementation that reads `ctx.tokens` — switch to `registry.permits()` or `registry.authorize_and_record()` per the table below:
  - `file_read`, `file_list`, `file_read_many`, `file_write`, `skill_read` — use `permits` (these tools don't have `max_invocations` semantics at the call site; if we later want them to count, upgrade to `authorize_and_record`).
  - `web_fetch`, `memory_store_tool`, `memory_query_tool`, `memory_delete_tool` — use `permits`.
  - `echo` — no capability check today; unchanged.
  - **`file_read_many` helper `read_one`** — signature changes to `read_one(path_str, tokens: &[CapabilityHandle], registry: &CapabilityRegistry, agent_id: AgentId)`. Drop the `Arc` clone-into-closure trick since `&CapabilityRegistry` (via the `Arc`) is `Send + Sync`.
  - **`spawn_tool` (`SpawnAgentTool`)** — replaces direct `CapabilityToken::issue` + child-tokens-vec construction with `capability_registry.narrow()` calls. The parent-delegation check (`parent lacks {cap}`) still uses `permits` against parent handles. Child `AgentProcess` gets a `Vec<CapabilityHandle>` of narrowed handles, not tokens.
- `crates/agentd/src/spawn_agents_tool.rs` — no direct changes (delegates to `SpawnAgentTool`). But the preflight `reserve_slot` still works on `active_count` unchanged.
- `crates/agentd/src/server.rs` — construct `Arc<CapabilityRegistry>` in all three `new*` / `with*` paths, pass to `AgentRegistry::new_with_registry()` (new ctor variant) and inject into `ToolInvocation::new()`.

**Tests updated:** every test that constructs an `InvocationContext` by hand or builds `Vec<CapabilityToken>` directly. Strategy:
1. Add `fn test_context_with_capabilities(caps: Vec<(AgentId, Capability)>) -> (InvocationContext, Arc<CapabilityRegistry>)` helper to `aaos-tools/src/invocation.rs` test module. It builds a fresh registry, inserts tokens for each capability, returns the context + a clone of the registry so tests can inspect.
2. Migrate each tool test to use the helper.
3. For integration tests that need to also configure the `AgentRegistry` (e.g. `registry.rs` tests that call `spawn`), the registry auto-creates its own `CapabilityRegistry` internally and exposes an accessor for tests: `AgentRegistry::capability_registry() -> &Arc<CapabilityRegistry>`.

**Invariant asserted:** after this commit, `grep -r "Vec<CapabilityToken>" crates/` returns zero results outside `capability.rs` itself and the `CapabilityRegistry` internals. Tools can't reach for a token. Also `grep -r "permits(&" crates/aaos-tools/` returns zero results — the raw token method is no longer called from tool code.

**Test suite must stay green:** 306 tests before; should stay 306 or higher after (commit 1 added ~8 new tests; commit 2 should not remove any existing tests, only migrate).

### Commit 3 — docs update

**Files touched:**
- `README.md` — capability-security section upgraded:
  - "Runtime-issued tokens" → "Runtime-issued, handle-opaque tokens"
  - Add sentence: "Agents and tool implementations hold opaque `CapabilityHandle` values; the underlying `CapabilityToken` is never exposed to non-runtime code. A forged handle either resolves to nothing or to a token issued for a different agent."
  - Keep the HMAC-signing caveat for cross-process cases.
- `docs/architecture.md` — same upgrade in the "Capability System" section. `CapabilityHandle` and `CapabilityRegistry` added to the component list.
- `docs/ideas.md` — the "Cryptographically unforgeable capability tokens" entry gets a new status line: **PARTIALLY ADDRESSED** (commit `<hash>`): in-process forgery is now substantially harder; HMAC signing for cross-process transport remains open.
- `docs/patterns.md` — "Public claims should match the code the reader finds" gets a follow-up paragraph noting this is how the upgrade cycle is meant to work (audit → document honestly → close the gap → upgrade wording).

**No code changes in this commit.** Keeps the doc upgrade reviewable independently of the implementation.

---

## Test plan

### Unit tests (new)

Per the commit breakdown above, ~15 new tests covering:
- Handle lifecycle (insert, resolve, remove, narrow)
- Cross-agent leak protection (resolve returns None for wrong agent_id)
- Handle removal on `remove_agent`
- Revocation flows through handles

### Existing tests (should all pass after commit 2)

- All 306 current tests. The big risk is that one of the tool tests constructs a test `InvocationContext` with a `Vec<CapabilityToken>` and now needs the registry. Mitigation: provide a `test_context()` helper in `aaos-tools/src/invocation.rs` tests module that builds a mini registry with a pre-inserted token.

### Integration check (manual, before commit 3)

- `docker build --no-cache -t aaos-bootstrap -f Dockerfile.bootstrap .`
- Run a simple goal: `./run-aaos.sh "List files in /src/crates"`
- Verify: bootstrap spawns, scans, produces output. Dashboard shows capability_granted events. No capability_denied events that weren't there before.

---

## Rollback

Three commits, each reversible via `git revert`. The order matters: revert commit 3 (docs) first, then commit 2 (wiring), then commit 1 (primitive). Reverting commit 2 first without reverting 3 would leave the README claiming handle-opaque tokens while the code doesn't have them.

---

## Things that would make this plan wrong

- **If `InvocationContext` is called from a hot path where the extra `Arc<CapabilityRegistry>` clone per invocation matters.** It's not — tool invocations are LLM-bounded (seconds). The clone is cheap.
- **If there are tool implementations outside this repo that consume `InvocationContext`.** There aren't today — the Tool trait is stable but no external implementations exist. A future third-party plugin ecosystem would need to migrate, but that's a forcing function we want.
- **If cross-agent handle leak is a goal, not a threat.** It isn't — agents can't intercommunicate directly anyway. The resolve-with-agent-id check is conservative; removing it would be a future relaxation if we ever wanted agent-to-agent capability handoff.

---

## Questions resolved by peer review

1. **`u64` vs `Uuid` for `CapabilityHandle`:** u64. Copilot confirmed — agent-scoped resolution makes index-guessing moot.
2. **`Arc<CapabilityRegistry>` vs global:** Arc. Copilot confirmed — better than global state.
3. **Commit 2 atomic vs split:** atomic. Copilot confirmed — don't land unused scaffolding.
4. **`remove_agent` audit-logging per-handle:** skip it. `AgentStopped` already implies capability cleanup. If an operator wants per-handle revocation history mid-run they can use the existing `revoke_capability` path which does emit `CapabilityRevoked`.
5. **`CapabilityRegistry::insert` visibility:** resolved by moving the registry to `aaos-runtime`. Internal mutation methods are `pub(crate)` to that crate; only `permits` and `authorize_and_record` are `pub`.

## Peer Review Notes (Copilot/GPT-5.4)

Two-round review. Round 1 caught the two material issues the original plan handwaved; round 2 (via this plan update) ratifies the corrections.

**Round 1 — acted on:**
- **"Clone-on-resolve breaks revocation and max_invocations."** Original plan had `resolve()` return a `CapabilityToken` clone. That makes `record_use()` meaningless because each tool holds its own copy. Revised plan: registry is the source of truth for mutable token state. Tools call `permits()` for check-only paths and `authorize_and_record()` for call-site counting — both atomic, neither clones.
- **"Crate boundary is underspecified."** Original plan put `CapabilityRegistry` in `aaos-core` with `pub(crate)` mutation methods, which `aaos-runtime` can't see. Revised plan: registry lives in `aaos-runtime`. `CapabilityHandle` and `CapabilityDenied` stay in `aaos-core` (public, cross-crate-friendly). Only `permits` and `authorize_and_record` are `pub` on the registry — everything else is `pub(crate)` to `aaos-runtime`.

**Round 1 — confirmed short calls:**
- u64 handle ID: fine.
- Arc<CapabilityRegistry> in InvocationContext: fine.
- Atomic commit 2: fine.

**Round 1 — note flagged for the implementer:**
- *"Update the plan's claim about revocation carefully. It won't 'still work' automatically unless revocation moves from `AgentProcess.capabilities` to the registry as the source of truth."* — addressed explicitly in the revised revocation section above.

## Handoff to implementer

**Crate locations (precise):**
- `CapabilityHandle`, `CapabilityDenied`, `CapabilitySnapshot` → `crates/aaos-core/src/capability.rs`.
- `CapabilityRegistry` → `crates/aaos-runtime/src/capability_registry.rs` (new file).
- `AgentRegistry` integration → existing `crates/aaos-runtime/src/registry.rs`.
- `InvocationContext` change → existing `crates/aaos-tools/src/context.rs` (confirm path before editing).

**Before starting commit 1:**
1. `cd /root/apps/aaOS && cargo test` — confirm 306 tests green on the current tree.
2. Read `crates/aaos-core/src/capability.rs` end-to-end. The `permits()`, `revoke()`, `record_use()`, `narrow()` methods on `CapabilityToken` are all present and need to stay; the registry calls them internally. Do not delete these methods.
3. Read `crates/aaos-runtime/src/registry.rs` lines ~60-200 (issue_capabilities, spawn_internal, spawn_with_tokens) so you know the shape of where the registry gets wired in.
4. Read `crates/agentd/src/spawn_tool.rs` lines ~150-200 (the child-token construction block) so you know what narrow() has to replace.

**Starting commit 1:**
- Add the new module. Keep `CapabilityToken` serializable in this commit — do not remove derives until commit 2.
- Run the new unit tests. All should pass.
- Run the full suite. Nothing should change (the primitive is unused).
- Commit with message `capability: handle-based registry primitive (plan: plans/2026-04-15-handle-based-capability-tokens.md)`.

**Starting commit 2:**
- Follow the cascade order from the plan's implementer notes section: `InvocationContext` struct → tool crate helpers → each tool → `aaos-runtime` wiring → `agentd` wiring → tests. Use `cargo check` between each layer.
- Add the `test_context_with_capabilities` helper early.
- Remove `Serialize/Deserialize` from `CapabilityToken` LAST, after everything else compiles and passes.
- Full suite green before commit. Expected 306+ tests passing.
- Commit with message `capability: route tool auth through handle registry (plan: plans/2026-04-15-handle-based-capability-tokens.md)`.

**Starting commit 3:**
- Only docs edits. Do not change code.
- README: update the two capability-security sections per the plan.
- `docs/architecture.md`: same. Add the `CapabilityRegistry` to the component list.
- `docs/ideas.md`: mark "Cryptographically unforgeable capability tokens" as **PARTIALLY ADDRESSED**, reference the commit hash.
- `docs/patterns.md`: add a one-paragraph follow-up to "Public claims should match the code the reader finds".
- Commit with message `docs: handle-based tokens landed — upgrade capability-security claims (plan: plans/2026-04-15-handle-based-capability-tokens.md)`.

**After all three commits:**
- `docker build --no-cache -t aaos-bootstrap -f Dockerfile.bootstrap .`
- `./run-aaos.sh "List files in /src/crates"` — confirm a simple goal still works end-to-end.
- Push to origin/main.

**If stuck:**
- Grep for `CapabilityToken` in crates/; every hit outside `aaos-core::capability` and the registry's internals is either a test that needs migrating or a wiring gap.
- If `authorize_and_record` semantics cause a tool's existing test to fail because the test spawns multiple checks expecting no invocation counting, the fix is to use `permits()` in that tool, not `authorize_and_record()`. Only tool invocation paths where per-call counting is intended should use `authorize_and_record`.

**Explicit STOP conditions:**
- If after commit 2 more than a handful of tests fail in surprising ways (>10, or any test failure that isn't a simple shape-mismatch), stop and report back. Do not force-fix.
- If you find a code path that constructs `CapabilityToken` outside `aaos-core::capability` and `aaos-runtime::capability_registry` that the plan didn't anticipate, stop and report.
- If serialization of a token is required somewhere the plan didn't account for (e.g. persisted to disk, sent over IPC), stop and report — that's a bigger question than this plan covers.

---

## Out of scope (explicitly)

- **HMAC-signed tokens.** Different scope, different motivation (cross-process). Deferred item in `docs/ideas.md`.
- **Per-handle audit logging of resolve failures.** Could be a useful debugging signal but adds noise. Not in this plan.
- **MicroVM backend (Phase G).** Handle opacity is a defense-in-depth layer; MicroVMs are a different isolation layer. Orthogonal.
- **Third-party tool plugin framework.** Handles prepare for this but don't ship the plugin loader.
- **Changing revocation semantics.** Revocation still works the way it does today; handles are a transport layer.

---

## Implementer notes

For the implementer (Qwen or whoever):

- **Start with commit 1 exclusively.** Do NOT modify anything outside `aaos-core/src/capability.rs` in commit 1. The primitive compiles standalone and the tests run standalone.
- **For commit 2, use `cargo check` aggressively.** Changing `InvocationContext` will cascade compile errors across every tool crate. Fix them in the order: `aaos-tools/src/invocation.rs` (the struct) → all tools in `aaos-tools` → `agentd/src/spawn_tool.rs` → `agentd/src/server.rs` → tests.
- **Test helpers:** there are probably 10-20 places that do `InvocationContext { agent_id: ..., tokens: vec![...] }`. Add a `test_context_with_tokens(tokens: Vec<CapabilityToken>) -> InvocationContext` helper early and use it everywhere. Saves massive churn.
- **Do NOT remove `#[derive(Serialize, Deserialize)]` from `CapabilityToken` until the end of commit 2.** If you remove it first, tests that serialize agent state (session store, audit events with token payloads) will fail. Do it after the wiring is green.
- **Before commit 3 (docs), run the full test suite + one Docker integration check.** Docs should never ship ahead of the code that backs them.
- **Commit messages:** reference this plan's path (`plans/2026-04-15-handle-based-capability-tokens.md`) in the trailer so the reasoning is traceable.
