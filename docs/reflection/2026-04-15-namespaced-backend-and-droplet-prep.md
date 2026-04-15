# Namespaced backend scaffolding + isolated dev VM prep *(2026-04-15)*

Not a reflection run — two in-progress workstreams worth preserving for continuity: the handle-token migration that landed in the runtime, and the namespaced-backend scaffolding that sits behind Phase F's `.deb` gate.

## Handle-token migration

**Integration commits:** `14a8eae`, `18d14f0`, `3c82f6e`.

A three-commit flow spread across roughly two Copilot review rounds, with mid-flow implementation done by a sub-agent handoff. Commit 1 went through cleanly. Commit 2 stalled mid-implementation when the sub-agent's free-tier access expired; a coordination session recovered the work-in-progress from the plan and shipped it. Commit 3 followed.

**Outcome:** `CapabilityToken` is now **handle-opaque**. Agents and tool code hold `CapabilityHandle` values only; the registry owns all mutable token state. Mutating paths (revoke, record_use, constraint checks) go through the registry rather than through cloned token state. The previous shape — tokens as plain Clone structs carrying their own state — was footgun-shaped for future concurrent access; the handle form removes that surface entirely.

## Namespaced-backend scaffolding

**Integration commits:** `a84cd98`, `a73e062`, `8a70a1a`.

A three-commit flow, **four Copilot review rounds** (plan v1 → v4) before implementation started. Each round concentrated on a different failure mode:

- Opaque handles (consistent with the handle-token work above) instead of direct token passing across the broker boundary.
- Corrected seccomp policy — the first draft was too permissive and the second too restrictive for the broker syscalls actually used.
- Dropped `CLONE_NEWPID` from the clone flag set. PID namespacing requires a helper for signal and reaping semantics that the scaffolding isn't ready to own; kept out of scope.
- Two-phase readiness handshake: the child emits `ready` after setup and `sandboxed-ready` after applying its own restrictions, so the broker can distinguish "spawned" from "confined."
- Peer-creds session binding via `SO_PEERCRED` on the broker's AF_UNIX socket, so session identity is kernel-attested rather than claimed in-band.
- Self-applied Landlock + seccomp inside the worker (not applied by the parent before exec), because the worker needs pre-restriction syscalls to set itself up.
- Hard failure on uid/gid mapping: if the `uid_map` / `gid_map` write fails, the worker exits non-zero rather than continuing in an unexpected identity.
- Fail-closed on missing Landlock: if the ABI check returns "no Landlock available," the backend refuses to start rather than downgrading to seccomp-only silently.

Sequential sub-agents implemented the three commits under a locked plan (no parallel dispatches — the Qwen free-tier expiry on the previous workstream was a clear signal that mid-flow handoffs need a written plan to survive).

**Commit 2 landed as PARTIAL.** The honest disclosure in the commit message and in code:

- **Works today:** the crate, the broker protocol, the Landlock + seccomp compilers, the worker binary, the two-phase handshake, peer-creds session management, the `AAOS_DEFAULT_BACKEND=namespaced` feature flag. 19 new tests landed, full suite sits at 340 passing.
- **Stubbed:** the actual `clone() + uid_map + pivot_root + exec` dance inside `clone_and_launch_worker`. The function is behind a `BackendError::CloneFailed` return and a unit test pinning the "no silent success" contract — so a future completion can't land silently without the test being updated.

**Three security-relevant deviations, documented in code:**

1. **AF_UNIX / CLONE_THREAD argument filtering dropped** — the plan explicitly allowed this as a seccomp-simplification trade-off; noted in the policy source.
2. **Landlock ABI check deferred to `restrict_self()` rather than `new()`** — a crate API limitation. Fail-closed behavior still holds (if restriction fails, the worker exits), but the detection happens one step later than the plan preferred.
3. **`AAOS_DEFAULT_BACKEND=namespaced` falls back to `InProcessBackend`** with a loud error log if the namespaced constructor fails. Operator intent is respected (they asked for namespaced, they got the closest working substrate); no *silent* degradation — the log line is clearly scoped and the audit trail records it.

## Isolated dev VM for finishing the stub

The clone+exec work involves `pivot_root`, namespace creation, and mount operations that are not appropriate to run on an interactive daily-driver machine. Provisioned an isolated cloud VM for this workstream: Debian 13, kernel 6.12.43, `CONFIG_SECURITY_LANDLOCK=y`, unprivileged user namespaces enabled, 4 GB RAM, 2 vCPU. Billed hourly at roughly $18/mo — a week of work on this costs ~$4. (Vendor and host address kept in local notes.)

Rust 1.94.1 installed, repo cloned, full suite (340) passes, `aaos-backend-linux` builds cleanly. Environment is ready for the stub-finish work.

## What's queued next

A tight plan (~150 lines) for the `clone_and_launch_worker` implementation:

- Clone flags: `CLONE_NEWUSER` + `CLONE_NEWNS` + `CLONE_NEWIPC`. No `CLONE_NEWPID`, no `CLONE_NEWNET` (network isolation is a separate concern with its own policy surface).
- uid/gid mapping via the pipe handshake from `user_namespaces(7)` — parent writes the maps after the child signals it's in the new namespace.
- Bind-mount plan for the worker's mount namespace: scratch tmpfs, shared libs, broker socket, worker binary.
- `pivot_root` to the scratch root, `execve` into the worker.

Peer review with Copilot (one round expected given the narrow scope), then sub-agent implementation on the isolated dev VM. Un-ignore the four integration tests in `tests/namespaced_backend.rs` as each piece lands. Manual verification: `/proc/<pid>/status` shows `Seccomp: 2` and `NoNewPrivs: 1`; Landlock applied.

## Process notes worth preserving

- **Three Copilot rounds is the empirical number for runtime-admission-control features.** Two rounds feels adequate after the fact and isn't — each round's fixes create new surface for the next round's critique. The Run 11 Prep Part B entry already named this; the namespaced-backend four-round cycle reinforces it.
- **Sub-agent coordination works when the plan is locked.** Each agent gets a self-contained prompt with file paths, cascade order, and explicit STOP conditions. Mid-flow failures are recoverable as long as the plan is written down: the handle-token workstream's recovery from a free-tier expiry was only possible because commit 2's plan was on disk.
- **PARTIAL commits are correct when honest.** The scaffolding's value (broker protocol, policy compilers, handshake, peer-creds, feature flag, 19 tests) is independent of the kernel-mechanics completion. Conflating them would have delayed shipping the reviewable parts without buying anything. A `CloneFailed` stub plus a unit test pinning the "no silent success" contract is a cheap, verifiable commitment that the gap will be closed before it can be accidentally ignored.
