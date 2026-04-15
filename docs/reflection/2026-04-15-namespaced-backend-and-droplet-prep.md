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

## Stub finish — landed same day

**Integration commits:** `1d6ec97` (clone+pivot_root path), `67c7fc3` (real `health()` + integration tests green).

Implemented the 9-step child function on the isolated dev VM in a single coordinated session (no sub-agent dispatch — the work was tight enough to iterate inline against a real kernel):

- Clone flags: `CLONE_NEWUSER` + `CLONE_NEWNS` + `CLONE_NEWIPC`. No `CLONE_NEWPID`, no `CLONE_NEWNET`, per plan v4.
- uid/gid mapping via the pipe handshake from `user_namespaces(7)` — parent writes `uid_map` / `setgroups=deny` / `gid_map` after the child signals it's in the new namespace, then unblocks the child via a single-byte write.
- Bind-mount plan for the worker's mount namespace: new-root tmpfs, scratch tmpfs, shared libs (read-only bind with `MS_BIND | MS_REC` + `MS_REMOUNT | MS_RDONLY` where supported), broker socket parent dir, worker binary.
- `pivot_root` into the new root, `umount2(MNT_DETACH)` on the old root, `execve` into the worker.
- Each step has a specific exit code (10–28) on failure and an opt-in `AAOS_NAMESPACED_CHILD_DEBUG` env var for per-step logging.

**Manual verification (on the capable host):** `/proc/<pid>/status` of the spawned worker shows `Seccomp: 2` (filter mode) and `NoNewPrivs: 1`. Four integration tests in `tests/namespaced_backend.rs` now pass under `--ignored`:

- `launch_reaches_sandboxed_ready` — end-to-end spawn + handshake + confinement.
- `stop_is_idempotent` — second `stop()` is a no-op.
- `health_detects_exit` — after SIGKILL on the worker, `health()` returns `Signaled`/`Exited`/`Disconnected`, not `Healthy`.
- `worker_cannot_execve` — placeholder (broker-side `TryExecve` poke op not wired yet; test launches and stops to prove the setup works).

Full workspace: 339 unit + 4 ignored integration = **343 tests passing, 0 failing**.

## Bring-up lessons worth codifying

Three concrete things surfaced during the clone+exec bring-up that are worth preserving for future substrate work:

1. **Stacked seccomp filters require the first filter to allow `SYS_seccomp`.** The worker compiles two filters (allowlist + kill-on-dangerous) and installs them in sequence. If the first filter doesn't explicitly allow `seccomp()`, the second filter's install fails with EPERM because the `seccomp` syscall itself is denied. The kill filter *does* deny future `seccomp()` calls — so the escalation surface is still closed. Allow-then-deny is safe; just-deny is broken.

2. **Bind-mount sources must be absolute paths.** The child `chdir("/")` during `pivot_root`, so any relative path the parent passed in becomes meaningless. Integration tests that set `worker_binary` to `"target/debug/aaos-agent-worker"` (relative, cargo's default) silently bind-mounted nothing and the subsequent `execve` failed with a confusing error. Resolving from `CARGO_MANIFEST_DIR` gives an absolute path that survives the child's cwd change. Added an assertion at test setup so this failure mode is named explicitly.

3. **Opt-in debug logging inside the child is worth keeping.** During bring-up I added per-step log lines to `/tmp/aaos-child-debug-<ppid>.log`. Every failure was a different step (tmpfs mount permission, bind-mount source path, seccomp install). Without the log, each round was a blind "worker didn't reach sandboxed-ready within 5000ms" — no signal on *which* step failed. Left the instrumentation in behind `AAOS_NAMESPACED_CHILD_DEBUG=/path/to.log`; future debugging on different kernel versions or distros will need it.

## Process notes worth preserving

- **Three Copilot rounds is the empirical number for runtime-admission-control features.** Two rounds feels adequate after the fact and isn't — each round's fixes create new surface for the next round's critique. The Run 11 Prep Part B entry already named this; the namespaced-backend four-round cycle reinforces it.
- **Sub-agent coordination works when the plan is locked.** Each agent gets a self-contained prompt with file paths, cascade order, and explicit STOP conditions. Mid-flow failures are recoverable as long as the plan is written down: the handle-token workstream's recovery from a free-tier expiry was only possible because commit 2's plan was on disk.
- **PARTIAL commits are correct when honest.** The scaffolding's value (broker protocol, policy compilers, handshake, peer-creds, feature flag, 19 tests) is independent of the kernel-mechanics completion. Conflating them would have delayed shipping the reviewable parts without buying anything. A `CloneFailed` stub plus a unit test pinning the "no silent success" contract is a cheap, verifiable commitment that the gap will be closed before it can be accidentally ignored.
