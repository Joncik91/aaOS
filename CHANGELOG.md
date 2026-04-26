# Changelog

All notable changes to aaOS.  Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); version numbers follow [Semantic Versioning](https://semver.org/).

The dpkg-format changelog at `packaging/debian/changelog` mirrors the tagged releases in short form for the `.deb` package; **this file is the authoritative human-readable record**.

Pre-v0.0.1 work (build-history #1–#13) predates the tagged-release cadence; it's captured under the `[0.0.0]` section below with ship dates and commits drawn from the roadmap's build-history section and the reflection log.

---

## [Unreleased]

Active milestone: **M1 — Debian-derivative reference image** (Packer pipeline producing a bootable ISO + cloud snapshots with the v0.2.8 `.deb` preinstalled).

---

## [0.2.8] — 2026-04-26

Round 11 self-reflection on v0.2.7 with a third prompt-shape steer: silent-failure paths (`let _ =` discards of `Result<()>` returns).  Agent classified all 47 such discards in the codebase — 44 defensible, **3 real bugs**.  All three concern persistence-layer failures the daemon throws away without an audit event or log line; under disk-full / NFS outage / read-only fs / SQLite corruption, the agent silently truncates history or accumulates orphan rows.  Reflection log: [`docs/reflection/2026-04-26-v0.2.7-round-11.md`](docs/reflection/2026-04-26-v0.2.7-round-11.md).

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.2.8> — `aaos_0.2.8-1_amd64.deb`.

### Fixed

- **Bug 41 (high — `archive_segment` failure permanently destroys conversation history).**  `persistent_agent_loop`'s summarization path discarded `archive_segment` errors with `let _ = `, then unconditionally drained the archived messages from in-memory history.  If the archive write failed (disk full, SQLite locked, ENOSPC mid-process), the original messages existed nowhere — gone from memory, never written to disk.  Daemon restart loaded only the LLM-summarized form; the agent's original tool-call arguments and intermediate reasoning were unrecoverable.  No audit event, no log line.  **Fix**: skip the summarization cycle when archive fails.  Original history stays intact for the next cycle to retry.  Audit + log on failure (with the existing `should_emit` throttle pattern from the v0.2.2 `replace` fix).

- **Bug 42 (high — `append` failure silently truncates session history).**  Same shape: every `agent.run` turn called `let _ = session_store.append(...)` and the on-disk history would be truncated at the last successful append point.  Daemon restart loaded a shorter history than the operator expected.  No audit, no log.  **Fix**: identical to Bug 41 — audit + log on failure with the throttle pattern.

- **Bug 43 (medium — silent SQLite orphan row leaks in approval store).**  Three sites in `ApprovalQueue::request` (timeout-handler, oneshot-fail, timeout-elapsed) plus the daemon's startup purge in `Server::build_approval_queue` did `let _ = store.remove(id);`.  Under read-only filesystem or a SQLite-level error the row stayed in the database while the in-memory `DashMap` entry was removed; subsequent `agent approval list` showed empty but the table grew.  After a restart the entries reloaded, retried, silently failed again, accumulating across every daemon lifetime.  **Fix**: log at `warn!` with `approval_id` + error (matching the existing `respond` site that already logs).

### Pattern reinforced

Round 11's prompt shape ("find `let _ = ` patterns that hide real bugs") found 3 real bugs in 205 s after rounds 9 + 10 had also been declared "done" with their respective shapes.  Three distinct prompt shapes, three rounds, 5 real bugs (Bugs 38, 40, 41, 42, 43).  Convention now confirmed: when self-reflection plateaus on one prompt shape, **change the shape**.  Don't declare depletion.  The v0.2.x runtime's reachable-by-source-reading bug surface is wider than any single prompt shape can find.

The agent's classification of the 44 defensible `let _ = ` discards is preserved in the reflection log — useful as a future maintenance reference (next time someone touches one of those sites and wonders if the discard is safe).

---

## [0.2.7] — 2026-04-26

Round 10 self-reflection on v0.2.6 with a deeper-investigation prompt.  v0.2.4's round 9 had declared "patch surface depleted on v0.2.x" for source-reading — the stress probes (Bugs 35, 36, 37) proved that was a depletion-of-the-prompt-shape, not a depletion-of-the-runtime.  Round 10's prompt explicitly steered toward the missed-pattern shapes (trait methods with only test callers, doc-warned defaults that production callers use anyway, lifecycle events that don't reach all subsystems).  Three findings; **two real bugs shipped, one deferred to ideas.md**.  Reflection log: [`docs/reflection/2026-04-26-v0.2.6-round-10.md`](docs/reflection/2026-04-26-v0.2.6-round-10.md).

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.2.7> — `aaos_0.2.7-1_amd64.deb`.

### Fixed

- **Bug 38 (medium — `SessionStore::clear` had no production caller).**  Same-shape as Bug 35: the trait method existed, documentation said "use this on stop," but no production caller invoked it.  Result: every persistent agent that processed messages left its session-store entry behind on stop.  In-memory store (production default) accumulated DashMap entries unbounded across spawn-stop cycles; JSONL store (no production user yet) would also accumulate zero-byte files — `JsonlSessionStore::clear` truncated rather than removed.  **Fix**: (a) wire `session_store.clear(agent_id)` into `persistent_agent_loop`'s exit path, gated on a `clear_session_on_exit` flag; (b) `AgentRegistry::start_persistent_loop` and `InProcessBackend::launch` set the flag to `!persistent_identity` so ephemeral agents clear and Bootstrap-shaped persistent-identity agents preserve history across stop+respawn; (c) fix `JsonlSessionStore::clear` to `fs::remove_file` instead of zero-truncate.  Existing test `persistent_loop_processes_message` (which asserts history outlives loop exit) preserved by passing `false` for the clear flag in tests.

- **Bug 40 (high — `agent.spawn_and_run` + `lifecycle: persistent` leaks).**  `handle_agent_spawn_and_run` calls `handle_agent_spawn` (which starts a persistent loop via `backend.launch` if the manifest requests it) then immediately runs a one-shot `execute_agent` against the same agent_id.  Two concurrent execution loops on one agent.  After the one-shot returns, the persistent loop is never told to stop — leaks one tokio task (InProcess) or one worker subprocess (namespaced) per call.  Bug 37 closed the worker-subprocess leak for `agent.stop`; this one is the same shape but on a different entry path.  **Fix**: reject persistent manifests in `handle_agent_spawn_and_run` with an explicit error directing the caller to use `agent.spawn` + `agent.run` instead.  spawn_and_run is a one-shot API; persistent agents need explicit lifecycle management.

### Deferred (filed in `docs/ideas.md`)

- **Bug 39 — `JsonlSessionStore::load_archives` / `prune_archives` scan whole data directory.**  Real structural inefficiency (every call is O(N) where N = total agents ever, not just the target agent's archives).  No production exploit because production uses `InMemorySessionStore`; `JsonlSessionStore` has no production caller today.  Reconsider when JSONL becomes the production store (durability requirement, multi-restart history needed) — the fix is structural (per-agent subdirectory layout) and not worth the cost of refactor today.

### Pattern reinforced

The deeper prompt — explicitly steering toward "trait methods with only test callers" — found Bugs 38 and 40 in 143 seconds, after round 9 with the standard prompt had reported 0/3 real findings on the same code.  Round 10's prompt structure is now the recommended one for any future v0.2.x reflection round.  Convention going forward: "no production caller for a trait method" is itself a finding shape that the loop should be primed to look for.

---

## [0.2.6] — 2026-04-26

The concurrency stress probe under InProcessBackend in v0.2.5 found Bug 35.  Re-running under `AAOS_DEFAULT_BACKEND=namespaced` exercised the broker SessionMap + worker fork/exec path that v0.2.5's run didn't touch.  Two more bugs surfaced — both real, both shipping.  Pattern: the namespaced backend's launch+stop path had been *exercised* by every droplet QA since v0.0.2, but only via the *Bootstrap-via-main.rs* entrypoint, never via the `agent.spawn` JSON-RPC path that production operators actually use.  Two probes (source + fuzz) plus the existing droplet QA had all missed bugs that surface only when an operator does `agent.spawn` with `lifecycle: persistent` under namespaced.

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.2.6> — `aaos_0.2.6-1_amd64.deb`.

### Fixed

- **Bug 36 (high — `mount("proc", ...)` fails inside unprivileged user namespace).**  v0.2.1 added Step E2 to the worker setup to mount a fresh procfs at `/proc` so the TOCTOU-safe `readlinkat("/proc/self/fd/<fd>")` could canonicalize an open fd.  The kernel refuses `mount("proc", ..., "proc", ...)` inside an unprivileged user namespace that doesn't own a PID namespace — and our worker deliberately doesn't unshare `CLONE_NEWPID` (the module doc explains why).  EPERM on every `agent.spawn` with `lifecycle: persistent` under namespaced.  The canonical fetch-HN goal didn't hit this because plan-executor subtasks spawn inline via `execute_agent_for_subtask` (not `backend.launch`); the bug was reachable only via direct JSON-RPC `agent.spawn` + persistent + namespaced — a narrow surface but a hard regression for anyone there.  **Fix**: bind-mount the host's `/proc` instead of mounting fresh procfs.  No special userns privilege required, and `/proc/self` is a magic-link resolved per-task by the kernel — the worker sees its own `/proc/<pid>/*` regardless of which procfs instance is mounted.  Trade-off: the worker can also see other host PIDs, but Landlock denies any read outside the explicit allow-list (which doesn't include `/proc/<other>`), so visibility is recoverable only as a side channel, not a direct exfiltration.  Commit `<TBD>`.

- **Bug 37 (high — `agent.stop` leaks worker subprocess under namespaced).**  `AgentRegistry::stop` ends the in-daemon persistent loop but never told the namespaced backend to terminate the worker subprocess.  `backend.stop()` was only called from tests.  Result: every `agent.spawn` under namespaced+persistent leaked one `aaos-agent-worker` process for the lifetime of the daemon.  Stress harness measured this directly: 20 spawn-stop cycles → 20 leaked workers, all in state `S` (sleeping in their broker-read loop, never told to exit).  **Fix**: new `AgentBackend::stop_by_agent_id` trait method (default no-op for backends that don't fork subprocesses; `NamespacedBackend` overrides to look up its session by agent_id and SIGTERM+waitpid-reap the worker pid).  `Server::handle_agent_stop` calls it after `registry.stop` succeeds.  Verified bounded under 1600 spawn-stop cycles × 3 passes on the same daemon: pass-2 ΔRSS +2.6MB, pass-3 +0.6MB — memory stable, no zombies, no leaks.

### Pattern reinforced

Every droplet QA since v0.0.2 has run the canonical fetch-HN goal end-to-end.  None caught Bugs 36 or 37 because the canonical goal doesn't actually use the `agent.spawn` + namespaced + persistent path — plan-executor subtasks go through inline `execute_agent_for_subtask` (no `backend.launch`), and the daemon-CLI submit path is similar.  The convention "test the path the canonical goal actually uses" (lifted from the Phase F-b QA in 2026-04-19) doesn't help here: the canonical goal's path never touched the broken code.

Stress probes that exercise *every* JSON-RPC method (spawn, stop, list) regardless of whether the canonical goal uses it surface bugs the canonical-goal QA can't.  Worth wiring stress into the release checklist alongside the canonical run.

---

## [0.2.5] — 2026-04-26

The concurrency probe paid off on its first run.  After round 9 (0 source-reading findings) and the fuzz pass (0 panics across 137M inputs) both depleted, the natural next probe was a multi-threaded stress harness.  First serious run found Bug 35 — a real memory leak under spawn/stop churn that neither prior probe could see.  Reflection log: [`docs/reflection/2026-04-26-v0.2.4-stress-pass.md`](docs/reflection/2026-04-26-v0.2.4-stress-pass.md).

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.2.5> — `aaos_0.2.5-1_amd64.deb`.

### Fixed

- **Bug 35 (medium — `InMemoryAuditLog::new()` is unbounded under churn).**  `Server::new()` and `Server::with_llm_and_audit()` constructed the inner audit log via `InMemoryAuditLog::new()` — explicitly unbounded per its own doc-comment ("Unbounded by default; opt-in cap via `with_cap()` for long-running test harnesses where unbounded growth would OOM").  Each spawn-stop cycle emits ~10 audit events at ~120 bytes each, so a daemon under churn grew RSS at ~1.2 KB/cycle.  Stress probe measured this directly: 16k spawn-stop cycles × 3 passes on the same daemon grew RSS linearly +20MB → +18MB → +20MB (60MB total, no plateau).  **Fix**: switch both constructors to `InMemoryAuditLog::with_cap(50_000)` (~6MB cap, ~5000 spawn-stop cycles of recent history).  Override via `AAOS_AUDIT_LOG_CAP`.  Verification: same 3-pass stress on v0.2.5 grows +15.6MB on pass 1 (cap fills) then +0.1MB and +0.012MB on passes 2/3 — bounded.  Commit `<TBD>`.

### Pattern reinforced

The v0.2.x release line has now closed bugs across three independent probes:
- **Source-reading reflection** (rounds 6–8): Bugs 27–34
- **Fuzzing** randomized inputs (5 min × 3 targets, 137M total): 0 findings (input-handling robust)
- **Concurrency stress** (32 threads × 500 cycles × 3 passes): Bug 35

Each probe finds bugs the others can't.  v0.2.5 marks the first time we have evidence of bug-finding via concurrency stress that source-reading and fuzzing both missed (the audit log's unbounded `new()` was *literally documented* in its own doc-comment but the loop didn't flag it because it's not exploitable from input — it's only visible under sustained load).

---

## [0.2.4] — 2026-04-26

Round-8 self-reflection on v0.2.3.  One real bug to fix (Bug 34: seccomp socket allowlist + lying docs).  Two findings real-but-deferred — filed in `docs/ideas.md` with concrete reconsider signals rather than shipped as code, per the round-6/7/8 lesson.  This is the first round where some findings genuinely deferred rather than fixed; the v0.2.x patch surface is starting to thin.  Reflection log: [`docs/reflection/2026-04-26-v0.2.3-round-8.md`](docs/reflection/2026-04-26-v0.2.3-round-8.md).

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.2.4> — `aaos_0.2.4-1_amd64.deb`.

### Fixed

- **Bug 34 (medium — defence-in-depth hole + factually wrong docs).**  The worker's seccomp allowlist allowed `SYS_socket` and `SYS_socketpair` *unconditionally*, plus server-side primitives (`SYS_bind`, `SYS_listen`, `SYS_accept`, `SYS_accept4`).  Two doc comments downstream of the policy claimed the opposite — `tool_surface.rs:26` and `worker_tools.rs:22` both said *"`web_fetch`: seccomp allowlist has no socket/connect syscalls"*, which was factually wrong.  Round-8 self-reflection caught the contradiction.  **Fix**: argument-filter `SYS_socket` and `SYS_socketpair` via `SeccompCondition` so arg0 must equal `AF_UNIX` — `AF_INET`/`AF_INET6`/`AF_NETLINK` etc. now return EPERM.  Server-side primitives removed from the allowlist entirely (the worker is a Unix-socket *client*; it `connect()`s to the broker session socket once and reads/writes — never `bind`/`listen`/`accept`).  Module-level doc + both downstream doc comments rewritten to be honest about what's allowed.  Tests: `seccomp_drops_server_socket_primitives` (asserts the four server primitives are not in the static list) + `seccomp_socket_filter_compiles_with_af_unix_condition` (asserts the argument-filtered allowlist compiles).  Live BPF execution (does `socket(AF_INET)` actually return EPERM on the worker?) is exercised by the namespaced-agents integration tests.

### Documentation

- **`capability_registry.rs::resolve_tokens` doc** rewritten to be honest about what v0.2.0's push-revocation protocol does and doesn't close.  Push-revocation closes the *post-dispatch* race for the worker's session-level registry; it does NOT close the *wire-race* window where an `InvokeTool` and a `RevokeToken` cross on the broker stream.  Round-8 caught the previous comment ("residual race; closing it fully requires a push-revocation protocol — queued for v0.2.x as Bug 11 Option A") as misleading because v0.2.0 *did* land push-revocation but the comment still described it as the unbuilt fix.  Honest comment now points at `docs/ideas.md` for the heavier-fix reconsider signal.

### Deferred (filed in `docs/ideas.md`)

- **Token-generation counter to close the wire race.**  Reconsider when (a) two operators share a daemon and one needs sub-call-latency revocation of the other's tokens, OR (b) the broker protocol gains synchronous result-ack for other reasons (back-pressure, exactly-once delivery).
- **Replace hand-rolled `SchemaValidator` with the `jsonschema` crate.**  Reconsider when externally-authored manifests start declaring tool schemas that need pre-tool-body trust-boundary enforcement.

### Pattern reinforced

The "deferred follow-up" comment pattern (rounds 6/7/8) keeps producing.  v0.2.4's `resolve_tokens` doc rewrite explicitly cites `docs/ideas.md` instead of claiming "queued for v0.N+1" — the contract is now: comments that defer must point at an external paper trail.  In-code TODOs without a corresponding `ideas.md` entry are findings waiting to happen.

---

## [0.2.3] — 2026-04-26

Round-7 self-reflection on v0.2.2 produced three real findings — same hit rate as round 6, all confirmed against source.  The reachable bug surface in the v0.2.x line is genuinely still open: round 6 fixed three TOCTOU/atomicity bugs in v0.2.0–v0.2.1, round 7 found three more in adjacent code (concurrent budget reset, intermediate-component symlink TOCTOU that v0.2.2's leaf-only fix didn't cover, audit-log misuse leading to deadlock).  Reflection log: [`docs/reflection/2026-04-26-v0.2.2-round-7.md`](docs/reflection/2026-04-26-v0.2.2-round-7.md).

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.2.3> — `aaos_0.2.3-1_amd64.deb`.

### Fixed

- **Bug 31 (medium — `BudgetTracker` reset race).**  `maybe_reset()` did a `load` of `last_reset_check` followed by an unconditional `store(now)`.  Two threads near a period boundary could both pass the rate-limit gate, both store, and both go on to call `reset()`.  If thread A's `track()` completed between the two resets, thread B's reset clobbered A's tokens back to zero — silent over-spend across the period boundary.  **Fix**: replace load+store with a CAS loop on `last_reset_check`.  Only the thread that wins the CAS proceeds to `reset()`; the others skip.  Test `maybe_reset_races_have_at_most_one_winner` hammers `track()` with 16 threads × 100 calls and asserts cumulative `used` matches cumulative `track`.  Commit `8c06449`.

- **Bug 32 (high — intermediate-component symlink TOCTOU).**  v0.2.x's `safe_open_for_capability` used `open()` with `O_NOFOLLOW`.  `O_NOFOLLOW` only rejects a symlink at the *leaf* component — symlinks at any intermediate path component are still resolved by the kernel during traversal.  An agent with `file_write: /data/project/*` could be steered to `/etc/...` if `/data/project` was swapped for a symlink to `/etc` between the parent-dir create and the file open.  Documented in v0.2.2's code as "out of scope of this fix" — round 7 correctly classified the deferred-comment-as-bug pattern again.  **Fix**: route through `openat2(AT_FDCWD, path, OpenHow{flags, resolve: RESOLVE_NO_SYMLINKS})` which rejects symlinks at every component.  Available since Linux 5.6 (Debian 13's 6.12+ kernel has it).  Falls back to plain `open()` when `openat2` returns ENOSYS or seccomp returns EPERM, so the build still works on older kernels with the leaf-only protection.  Worker seccomp policy gains `SYS_openat2` in the allowlist.  Two API quirks worth noting in the commit: `O_NOFOLLOW` must be stripped from the flags arg (kernel returns EINVAL if both are set together with `RESOLVE_NO_SYMLINKS`), and `open_how::mode` must be 0 unless `O_CREAT` is set (also EINVAL otherwise).  Test `intermediate_component_symlink_rejected` plants a symlink as the parent dir and asserts the open is refused.  Commit `67e7d24`.

- **Bug 33 (medium — `InMemoryAuditLog::with_cap(0)` deadlock).**  The guard was `debug_assert!(max >= 1)` which compiles out in release.  With `max == 0`, `record()`'s `while events.len() >= max` loop is always true and `pop_front()` on an empty `VecDeque` is a silent no-op — infinite spin while holding the audit mutex, deadlocking every recorder thread.  **Fix**: switch to always-on `assert!`.  Misuse fails loud at construction.  Test `in_memory_audit_log_with_cap_zero_panics` asserts the panic.  Commit `627846e`.

### Pattern reinforced

The v0.2.0 → v0.2.1 → v0.2.2 → v0.2.3 sequence has now produced three rounds in a row where a v0.N comment "this is deferred follow-up" became a v0.N+1 finding.  Round 6's lesson — don't ship deferred-follow-up comments — is reinforced.  v0.2.3 deletes the `// out of scope of this fix` comment in `file_write.rs` (Bug 32 fix subsumes it).  Convention enforced: deferred-by-design goes in `docs/ideas.md` with a reconsider signal; known-issues-pending-fix go in CHANGELOG known-issues.  In-code TODOs are a finding-generator.

---

## [0.2.2] — 2026-04-26

Round-6 self-reflection on v0.2.1 source produced three findings, all real, all shipped — the highest hit rate since round 1.  The v0.2.x line just opened design ground (push-revocation, approval persistence, the path_safe TOCTOU subsystem) so the reachable bug surface temporarily widened.  Two of the three were flagged as "deferred follow-up" by the code's own comments — surfaced pattern: don't ship deferred TODOs in production code, file in `docs/ideas.md` with a reconsider signal instead.  Reflection log: [`docs/reflection/2026-04-26-v0.2.1-round-6.md`](docs/reflection/2026-04-26-v0.2.1-round-6.md).

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.2.2> — `aaos_0.2.2-1_amd64.deb`.

### Fixed

- **Bug 28 (high — `web_fetch` redirect host bypass).**  `WebFetchTool` constructed its `reqwest::Client` with `Policy::limited(5)` — the capability host check ran exactly once at the top of `invoke()` and reqwest then followed up to five HTTP redirects without re-checking the destination host against the agent's `NetworkAccess` grant.  An attacker who controlled a server in the agent's allowed host list (or compromised one) could return a 302 redirect to an attacker-controlled host; the response body returned silently as the tool result.  **Fix**: build the client with `Policy::none()`, hoist the capability check into a `check_url_permitted(ctx, url)` helper, and follow redirects manually inside `invoke` with a per-hop check.  Test `redirect_to_unpermitted_host_denied` spawns a mock 302 server and asserts `CapabilityDenied` for the redirect target.  Commit `eca9ddb`.

- **Bug 29 (medium — `file_list` residual TOCTOU on directory listing).**  v0.2.1's `file_list` opened the requested path with `O_PATH | O_NOFOLLOW` and resolved a kernel-pinned canonical for the capability check, then *immediately dropped the fd* and re-opened by canonical-path-string for the actual `metadata` and `read_dir` calls.  Between the fd drop and the second open, an attacker with write access to any path component could swap the directory for a symlink to a forbidden tree (e.g. `/etc`).  The capability check passed against the original inode but the listing was performed against the swapped target.  Code's own comment had flagged this as deferred follow-up — round 6 correctly classified it as a real bug.  **Fix**: new `AccessMode::ReadDir` in `path_safe` (`O_RDONLY|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC`); `file_list` rewrite tries `ReadDir` first and falls back to `Read` for single files; either way the fd survives and powers the actual I/O via `fstat` for files, `nix::dir::Dir::from_fd` for directories.  Side effect: directory entries report `size_bytes: 0` because per-entry `fstatat` by name would re-introduce TOCTOU.  Commit `6b24cf7`.

- **Bug 30 (high — non-atomic session-store rewrite can permanently destroy history).**  `persistent_agent_loop`'s summarization path called `session_store.clear(&agent_id)` (truncating the JSONL file) followed by `session_store.append(&agent_id, &history)` (rewriting the summarized history).  The two were not atomic — and the code's own comment again flagged it as deferred follow-up.  A daemon crash, partial write, or filesystem-full between the clear and the append left an empty on-disk file; the in-memory history was still intact, but a daemon restart loaded the empty file and the agent's session history was permanently destroyed.  The 60s throttle on `SessionStoreError` audit events meant operators saw at most one warning per minute even on persistent failures.  **Fix**: new `SessionStore::replace` trait method with a default `clear+append` fallback for in-memory stores; `JsonlSessionStore` overrides with the standard write-temp + fsync + `rename(2)` pattern (POSIX guarantees rename atomicity on the same filesystem).  `persistent_agent_loop` now calls `replace` once.  Test `jsonl_replace_is_atomic_swap` seeds 3 messages, replaces with 2, asserts no `.tmp` file leaks.  Commit `4bdfb5b`.

### Pattern lifted

- **Deferred-follow-up code comments are a finding-generator.**  Bugs 29 and 30 both came from inline `// NOTE: this is deferred follow-up` comments in v0.2.x code.  The reflection loop reads those comments and (correctly) calls them as bugs.  Convention going forward: if it's deferred-by-design, file in `docs/ideas.md` with a reconsider signal and DELETE the code comment.  If it's a known issue we couldn't fix yet, file in `CHANGELOG.md` "Known issues (fixed in X+1)" and tag the code comment with a `// SAFETY: see ideas.md#...` style reference.  No third option — comments saying "we should fix this" without an `ideas.md` entry are noise that the reflection loop will turn into work.

---

## [0.2.1] — 2026-04-26

Same-day patch closing five regressions surfaced by droplet QA of the v0.2.0 `.deb`.  v0.2.0's TOCTOU fix was correct on the host (lib tests passed) but its `/proc/self/fd/<fd>` canonicalization path was broken in three orthogonal ways once the namespaced backend was active — every `file_read` inside a worker failed, which was caught by the v0.2.0 canonical fetch-HN run on the droplet before the v0.2.0 tag was pushed.  Documented as the v0.2.0 → v0.2.1 forward-pointer per the "Known issues (fixed in X+1)" pattern.

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.2.1> — `aaos_0.2.1-1_amd64.deb`.

### Fixed

- **Worker had no `/proc` mounted.**  The worker rootfs is a tmpfs created via pivot_root with bind-mounts for the workspace, scratch tmpfs, shared libs, broker socket, and worker binary — `/proc` was never mounted inside.  v0.2.0's TOCTOU fix needs `/proc/self/fd/<fd>` to canonicalize an open fd; without it the readlinkat returns ENOENT and every `file_read` inside the namespaced backend fails.  Added Step E2 in `aaos-backend-linux/src/lib.rs`'s worker setup: `mount("proc", "/proc", "proc", MS_NOSUID|MS_NODEV|MS_NOEXEC)`.  Procfs mounted by the worker is its own instance, scoped to the worker's thread group, so this does not leak more than `/proc/<pid>/*` would already expose to the worker's own UID.  Commit `278aa52`.

- **`std::fs::read_link` calls bare `readlink`, blocked by seccomp.**  Worker seccomp policy at `aaos-backend-linux/src/seccomp_compile.rs:144` permits `readlinkat` but not the older `readlink` syscall.  Rust's `std::fs::read_link` on Linux x86_64 glibc resolves to the bare syscall, returning EPERM under seccomp.  Switched `aaos-tools::path_safe::canonical_path_for_fd` to `nix::fcntl::readlinkat(None, …)` so the call goes through the syscall the worker is permitted to make.  Commit `8d63860`.

- **Landlock policy missing `/proc` read-only rule.**  Even with `/proc` mounted and `readlinkat` allowed, the worker's Landlock ruleset has to permit reading inside `/proc` for the readlinkat to succeed.  Added a `PathBeneath(/proc, READ_ONLY)` rule in `aaos-backend-linux/src/landlock_compile.rs::build_ruleset`.  Read access is sufficient — we only call `readlinkat`.  Commit `cd8bc28`.

- **Release-mode unused-imports warning for `CapabilitySnapshot`.**  The type is used inside a `#[cfg(any(test, debug_assertions))]` method, so release builds saw the import as unused — surfaced when the v0.2.0 `.deb` build pulled rustc through release mode.  Cfg-gated the import in `crates/aaos-core/src/capability_registry.rs`.  Commit `c8737b0`.

- **Cosmetic warning on every restart: `wire_revocation_notifier: notifier already installed`.**  The LLM-aware constructors call `Server::new()` first and then rebuild `build_in_process_backend` with the LLM client — `wire_revocation_notifier` fired twice on the same registry.  The OnceLock made the second install a no-op, but the warning was noise on every restart.  Silenced because the first install already wired the SessionMapNotifier from the same SessionMap; the registry is correctly attached either way.  Commit `8f29ab7`.

### Verification

- v0.2.1 droplet QA passed: canonical fetch-HN goal completes in 12.6s with a real `/data/final-test.md` comparison file; symlink read attempt rejected with `O_NOFOLLOW (capability TOCTOU guard)`; approval-DB write-restart-clear cycle exercises the persistence path; `wire_revocation_notifier` fires cleanly with no warnings.

---

## [0.2.0] — 2026-04-26

Cleared-queue release.  v0.1.x left four architectural items deferred — push-revocation, approval persistence, the `canonical_for_match` TOCTOU, and `clone3` seccomp tightening.  v0.2.0 closes the first three; the fourth (`clone3` argument filtering) was discovered to be structurally infeasible for seccomp-BPF (the kernel takes a userspace pointer to `struct clone_args`; BPF can only read syscall registers, not pointed-to memory) and was filed as not-buildable under the current substrate.  Detail in `docs/ideas.md`.

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.2.0> — `aaos_0.2.0-1_amd64.deb`.

### Added

- **Push-revocation protocol (Bugs 11 + 18)** — `CapabilityRegistry::revoke()` now publishes the revocation to a pluggable `RevokeNotifier`; `aaos-backend-linux::SessionMapNotifier` implements it by sending a `Request::RevokeToken { token_id }` frame to every active worker session whose tokens reference the revoked id.  Worker-side: a session-level `Arc<CapabilityRegistry>` initialized at session start handles the frame by calling `registry.revoke(token_id)` — subsequent `permits()` checks see the revocation.  `revoke_all_for_agent` also fires the notifier per revoked token, so lifecycle-exit and capability-wipe paths no longer silently drop revocations on workers.  Commits `294024b`, `13d08c1`.

- **SQLite-backed approval queue persistence** — `crates/agentd/src/approval_store.rs` (new) is a single-purpose SQLite store mirroring the in-memory `ApprovalQueue` shape.  `ApprovalQueue::with_store(store)` writes through to disk on every insert/respond/timeout.  `Server::build_approval_queue` reads `AAOS_APPROVAL_DB` (default `/var/lib/aaos/approvals.db`); on startup it purges entries already past `DEFAULT_APPROVAL_TIMEOUT` and clears the rest because the agents that owned them are gone after the restart.  Falls back to in-memory on any open/load failure rather than failing daemon startup.  Tests cover round-trip persistence, replace-on-duplicate-id, age-based purge, open-and-reopen.  Commit `860491c`.

### Fixed

- **`canonical_for_match` TOCTOU (Round-4 + Round-5 Finding 1)** — file tools previously canonicalized the requested path string, glob-matched against the agent's grant, then re-opened by string for I/O.  An attacker with write access to any path component could swap a regular file for a symlink between the two operations and steer the read/write to a forbidden target.  Fixed by introducing `aaos-tools::path_safe::safe_open_for_capability(path, mode)` which opens with `O_NOFOLLOW | O_CLOEXEC`, resolves the resulting fd via `/proc/self/fd/<fd>` to a kernel-pinned canonical, and hands back both an `OwnedFd` and that canonical string.  New `Token::permits_canonical_file` / `CapabilityRegistry::permits_canonical_file` / `glob_matches_canonical` skip the second `fs::canonicalize` since the fd already pins the inode.  All six file tools (`file_read`, `file_write`, `file_edit`, `file_list`, `file_read_many`, `grep`) migrated.  `file_write` and `file_edit` perform their I/O on the same fd that powered the capability check; `file_list` and `grep` use the `O_PATH` variant since they hand off to `read_dir` / ripgrep.  Tests cover the symlink-swap race directly: open an fd, swap the path for a symlink to a forbidden target, and assert the fd still reads the original inode.  Commit `8b8f03b`.

### Deferred / not buildable

- **`clone3` seccomp argument filtering (Bug 19)** — moved from `[Unreleased]` to `docs/ideas.md` as STRUCTURALLY INFEASIBLE.  `clone3(struct clone_args *args)` takes a *pointer* to a userspace struct.  seccomp-BPF programs run before the syscall executes and have access only to the syscall register values, not the memory they point to.  The kernel deliberately does not copy `clone_args` into the BPF program — this is documented in `Documentation/userspace-api/seccomp_filter.rst`.  An attacker who can call `clone3` from a confined worker could place arbitrary flags in the struct and we have no syscall-filter-level mechanism to reject them.  Reconsider signals: (a) Linux gains a seccomp variant that exposes `clone3` flags to BPF, or (b) we move worker confinement onto a substrate that can intercept argument memory (Landlock LSM extensions, eBPF LSM hooks, microVM hypervisor traps).  Until then, the in-process seccomp policy correctly allows `clone3` unconditionally — denying it would break tokio's worker-thread spawn — and the namespace-creation defense is layered at the unprivileged-user-ns boundary instead.

- **Approval-queue full reload-and-rearm.**  v0.2.0's persistence layer flushes pending approvals to disk and surfaces them on restart, but the persistent-agent state machine doesn't yet expose a hook for re-attaching a reload-time `oneshot::Sender` to the agent that originally issued the approval request.  v0.2.0 logs the count and clears the entries; the agents the entries belonged to are gone after the restart anyway.  Reconsider when persistent-agent migration shipping makes a "resume across restart" story load-bearing.

### Known issues (fixed in 0.2.1)

- The TOCTOU fix's `/proc/self/fd/<fd>` canonicalization path was broken inside the namespaced backend in three orthogonal ways: worker rootfs had no `/proc` mounted, `std::fs::read_link` calls bare `readlink` which seccomp denies, and Landlock had no rule permitting `/proc` reads.  Every `file_read` inside a worker failed.  Fixed in v0.2.1 — see that section above.

---

## [0.1.7] — 2026-04-26

Round-5 self-reflection on v0.1.6 source on a fresh droplet.  One real new finding (Bug 27); two findings reproduced from earlier rounds and correctly skipped (already in `docs/ideas.md`).  Full reflection: [`docs/reflection/2026-04-26-v0.1.6-round-5.md`](docs/reflection/2026-04-26-v0.1.6-round-5.md).

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.1.7> — `aaos_0.1.7-1_amd64.deb`.

### Fixed

- **Bug 27 (high — capability-budget enforcement on spawn)** — `crates/agentd/src/spawn_tool.rs` issued child capability tokens via `CapabilityToken::issue(... Constraints::default())` on BOTH the first-attempt and retry paths, silently dropping parent `max_invocations`, `rate_limit`, and `expires_at`.  Phase A's run-1 finding #3 had originally fixed this; the fix regressed when the spawn paths needed to issue child tokens with a NARROWER capability than the parent (e.g., parent holds `file_read: /src/*`, child asks for `file_read: /src/crates/*`).  The existing `CapabilityToken::narrow()` only tightens constraints — can't substitute the capability identity — so the spawn code worked around it by issuing fresh tokens, bypassing constraint inheritance entirely.  Concrete impact: a parent with `WebSearch { max_invocations: Some(1) }` could spawn a child with `web_search` capability whose token had no invocation cap.  **Fix**: added `CapabilityToken::narrow_with_capability(child_agent, child_capability, additional)` that verifies the child's capability is a subset of the parent's via `capability_matches()`, clones the parent token preserving its constraints, substitutes the narrower capability, and layers any additional constraints on top.  Plus a registry wrapper.  Both spawn paths refactored to use the new method.  Surfaced by aaOS reading its own source.  Commit `c064531`.

---

## [0.1.6] — 2026-04-25

---

## [0.1.6] — 2026-04-25

Round-4 self-reflection on v0.1.5 source.  One finding shipped (Bug 26), two filed as deferred-hardening / future-architecture entries in `docs/ideas.md`.

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.1.6> — `aaos_0.1.6-1_amd64.deb`.

### Fixed

- **Bug 26 (medium — capability budget enforcement)** — `crates/aaos-tools/src/invocation.rs` charged the capability use *after* tool invocation (Bug 10's v0.1.1 fix).  If the token expired or was revoked between `permits()` and the post-invoke `authorize_and_record`, the tool had already executed with no count recorded — effectively a free invocation past the `max_invocations` budget cap.  **Fix**: charge BEFORE invoke.  `authorize_and_record` now runs after the `permits()` handle-find and BEFORE the surface-routing block.  On failure (race lost or token expired/revoked between permits and record), the tool does NOT run — fail-closed.  On success, the count stays charged regardless of whether the tool then succeeds or errors: charge-on-attempt semantics, the correct billing model for a capability budget.  Existing `max_invocations_enforced_through_invoke` test (Bug 10) still passes under the new ordering.  Commit `58f1460`.

### Documentation

- **`docs/ideas.md`** — added "Authenticated `McpMessage` sender (when a serialization boundary appears)" entry from the round-4 finding 2.  Theoretical under current architecture (no agent-controlled deserialization path); becomes real if a wire protocol is added.

### Deferred (logged in ideas.md or already there)

- **Round-4 Finding 1** — TOCTOU in `canonical_for_match` (capability bypass via symlink swap).  Real attack surface but documented technical debt at `crates/aaos-core/src/capability.rs:314-318` with an `ideas.md` entry.  Fix requires `O_NOFOLLOW` + `/proc/self/fd` re-open — Linux-specific, unsafe-FFI, separate hardening milestone.

---

## [0.1.5] — 2026-04-25

---

## [0.1.5] — 2026-04-25

Same-day patch closing two findings from the round-3 v0.1.4 self-reflection run.  Third finding deferred (FileWriteTool TOCTOU — theoretical under current threat model).

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.1.5> — `aaos_0.1.5-1_amd64.deb`.

### Fixed

- **Bug 24 (low — security doc correctness)** — `crates/aaos-backend-linux/src/broker_session.rs` module-level documentation claimed "seccomp denying `dup2`" was the mitigation against fd-handoff attacks after `SO_PEERCRED` validation.  Two factually wrong claims: (a) `seccomp_compile.rs:99` explicitly *allows* `dup3` (tokio uses it for stdio plumbing), and (b) `dup2` is not on either list — it falls through to default EPERM, not the SIGSYS the comment implied.  Corrected the doc to reflect the actual mitigations: Landlock (filesystem confinement) + user namespace (process scope) + broker session-id correlation at `register_session()` time.  Runtime behaviour is unchanged — only the documentation was misleading.  Commit `5f8b7c5`.

- **Bug 25 (low-medium — async correctness)** — `crates/aaos-runtime/src/registry.rs::stop()` held a `DashMap` shard guard across an `mpsc::send().await`.  Under heavy mpsc-buffer pressure (a slow agent loop draining commands), the await would stall until the buffer drained, blocking any other task contending on the same shard.  Fixed: clone `command_tx` before the guard's scope ends, await outside.  Standard async-Rust pattern.  Commit `5f8b7c5`.

### Investigation

- The agent's claim that the Bug 21 fix (`7d8db0f`) introduced a deadlock was **disproved** by source review: `registry.rs:252` explicitly `drop(entry)` releases the `agents`-DashMap lock before `remove_agent` is called at line 260, and `remove_agent`'s `revoke_all_capabilities` call hits `capability_registry` (a separate `Arc`) — no re-entrant lock.  Bug 21's fix is correct; no revert needed.

### Deferred

- **Finding 1 — FileWriteTool parent-dir-then-write TOCTOU.**  Real race window between `fs::create_dir_all(parent)` and `fs::write(path, content)` in `crates/aaos-tools/src/file_write.rs`, but an attacker requires both a capability token AND independent filesystem write access to the workspace.  Worker confinement (Landlock + user namespace) constrains the symlink-redirect surface.  Proper fix needs `openat`/`O_PATH` component-walk; out of scope for v0.1.x.  Logged as a future hardening item.

---

## [0.1.4] — 2026-04-25

### Known — still open (triaged 2026-04-25, none blocking)

- **Bug 14 (informational)** — `commit_nudges` mechanism added in v0.1.0 (`cba106b`).  v0.1.2 expanded its trigger condition (`commit-nudges` now fires on empty `tool_uses` with `stop_reason=ToolUse`, not just `EndTurn`); both forms remain in place as safety nets.
- **Bug 18 (theoretical, accepted-risk)** — TOCTOU in `CapabilityRegistry::narrow`.  Same race class as Bug 11; tracked under v0.2.x Option-A push-revocation protocol.
- **Bug 19 (theoretical, deferred to ideas.md)** — `clone3` seccomp allowlist gap.  Tracked in [`docs/ideas.md`](docs/ideas.md) with concrete reconsider signals.
- **Bug 20 (theoretical, accepted-risk)** — `BudgetTracker::maybe_reset` benign double-reset.
- **v0.1.4 Finding 1 (theoretical)** — claimed TOCTOU in `CapabilityToken::permits` was already closed by Bug 10's v0.1.1 fix (atomic check-and-record via DashMap exclusive shard-lock in `authorize_and_record`).  No action needed.
- **v0.1.4 Finding 3 broader scope** — approval queue has no persistence across daemon restart.  REAL-DESIGN: needs a storage-layer decision before fixing.  v0.1.4 ships the timeout sub-issue (Bug 23); the persistence question is queued.

---

## [0.1.4] — 2026-04-25

Same-day patch closing two real findings from the v0.1.2 self-reflection run that targeted the v0.1.2 source.  Both were triaged via Sonnet sub-agent against current source.

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.1.4> — `aaos_0.1.4-1_amd64.deb`.

### Fixed

- **Bug 22 (high)** — Workspace GC follows symlinks, allowing privilege escalation.  An agent with a legitimate `FileWrite: /var/lib/aaos/workspace/*` capability could plant a symlink at the workspace root pointing to `/etc/cron.d`, `/etc/sudoers.d`, or any `aaos`-uid-writable path; the next GC tick (`crates/agentd/src/workspace_gc.rs`) saw the symlink via `path.is_dir()` (which follows symlinks), passed it to `remove_dir_all` (also follows), and deleted the target under the `aaos` uid.  Real privilege boundary violation: agent capability scoped to workspace, GC elevates to arbitrary filesystem deletion.  **Fix**: use `DirEntry::file_type()` (does NOT follow symlinks, unlike `path.is_dir()`) and explicitly reject `ft.is_symlink()` before considering an entry for removal.  Commit `5ac9479`.

- **Bug 23 (medium)** — Approval queue requests have no timeout.  `crates/agentd/src/approval.rs:112` awaited the response oneshot with no upper bound; if no operator responded, the agent blocked forever and the pending `DashMap` entry leaked across daemon lifetime (resource leak on operator absence).  **Fix**: wrap `rx.await` in `tokio::time::timeout(DEFAULT_APPROVAL_TIMEOUT = 1h)`.  On timeout the entry is removed, a warn is logged, and the call returns `ApprovalResult::Denied` with a timeout reason.  Commit `5ac9479`.

---

## [0.1.3] — 2026-04-25

Same-day patch closing Bug 21, surfaced by the v0.1.2 self-reflection run that verified the Bug 13 + Bug 17 fixes.  Plus triage of Bugs 18/19/20 (all theoretical, no fixes needed) and a new `docs/ideas.md` entry for the `clone3` seccomp tightening.

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.1.3> — `aaos_0.1.3-1_amd64.deb`.

### Fixed

- **Bug 21 (medium)** — Missing `CapabilityRevoked` audit events during agent shutdown.  `crates/aaos-runtime/src/registry.rs::remove_agent` (line 138) called `capability_registry.revoke_all_for_agent(id)` directly, bypassing the public `revoke_all_capabilities()` wrapper (line 408) which is the only path that emits the `CapabilityRevoked` audit event.  Result: every agent's `CapabilityGranted` events at spawn-time had no matching `CapabilityRevoked` events at shutdown — audit trail incomplete for security forensics.  Fix: route `remove_agent` through `revoke_all_capabilities()`.  Also tightened `revoke_all_capabilities` itself: replaced the dead `for i in 0..count { let _ = i; }` placeholder loop with a single bulk audit event whose `capability` string carries the count.  Surfaced by aaOS reading its own source on v0.1.2.  Commit `7d8db0f`.

### Documentation

- **`docs/ideas.md`** — added "Tighten `clone3` seccomp filter to `CLONE_THREAD` only" with concrete reconsider signals (third-party audit recommendation, M1 Debian-derivative milestone, or a demonstrated escape).

---

## [0.1.2] — 2026-04-25

Same-day patch closing two bugs uncovered while verifying v0.1.1.  Bug 13 (agent-stop race) had been queued from yesterday's v0.1.0 run; Bug 17 (workspace path mismatch) was surfaced by the same run that verified Bug 13's fix.  Full reflection: [`docs/reflection/2026-04-25-v0.1.2-bug-13-and-17.md`](docs/reflection/2026-04-25-v0.1.2-bug-13-and-17.md).

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.1.2> — `aaos_0.1.2-1_amd64.deb`.

### Fixed

- **Bug 13 (high)** — Agent stop races with in-flight tool invocation.  When the streaming JSON-RPC client disconnected (Ctrl-C from CLI, broken pipe, broadcast channel closed), `crates/agentd/src/server.rs` immediately called `exec_task.abort()`.  Tokio cancellation propagated inward to the nearest `.await` — which is `invoke_tool(...).await` inside the executor's ToolUse arm.  The future was dropped, the scopeguard fired `stop_sync(agent)`, the in-flight `file_write`/`git_commit` side-effect was lost.  Visible failure: missing output file.  Invisible failure (more dangerous): tool with side-effects executes but agent is stopped before recording the audit event.  **Fix**: 500 ms drain window via `tokio::time::timeout(&mut exec_task)` at all four `exec_task.abort()` sites (plan + direct branches, write-failure + RecvError::Closed cases) so pending tool invocations complete before cancellation.  Also added a `tracing::warn!` to `race_deadline` in `crates/aaos-runtime/src/plan/executor.rs` so TTL-triggered drops are visible in journald (same drop-mid-tool-call mechanism, just triggered by wall-clock instead of disconnect).  Diagnosis took one Sonnet sub-agent call; verified end-to-end on a fresh-clone droplet — a 10.9 KB self-reflection report landed on disk for the first time.  Commit `34b018e`.

- **Bug 17 (medium-high)** — `inline_direct_plan` hardcoded the workspace path, ignoring operator-stated output paths.  The Direct orchestration path (`--orchestration persistent`) constructed a 1-node Plan with `workspace: "{run}/output.md"` always set.  The generalist's system_prompt at `packaging/roles/generalist.yaml` prioritises the workspace param over the goal text — so when the operator's goal said "write to /data/findings.md," the LLM dutifully wrote to the workspace path instead.  Operator never saw the file at the path they asked for.  Same silent-misdelivery class as Bug 9 was, just at a different layer.  Concretely: the v0.1.2 self-reflection run wrote a 10.9 KB findings report to `/var/lib/aaos/workspace/<run-id>/output.md` instead of `/data/findings.md`.  **Fix**: omit the workspace param entirely from `inline_direct_plan`; the generalist's "if no workspace, follow the task description" fallback path then triggers and the LLM writes to whatever path the goal text named.  Tightened the EXECUTION CONTRACT block to explicitly say "the operator-specified path."  Risk if the LLM picks a path the generalist's caps don't cover: a clean capability-denied error rather than silent misdelivery — the better failure mode.  Commit `77bbe9d`.

- **Bug 14 (escalated, narrowed)** — Empty `tool_uses` with `stop_reason=ToolUse` now counts as an `EndTurn` for commit-nudge purposes.  DeepSeek (v3/v4) emits `stop_reason=ToolUse` even when the response contains zero `tool_use` blocks (thought-only text).  The existing `EndTurn`-arm nudge never fired for these.  Fix: when `tool_uses` is empty AND commit_nudges remain, inject the same nudge user-message and loop; once nudges exhausted, accept as `Complete`.  This is what made multi-turn bug-hunt runs actually commit findings on v0.1.2 — without it, the LLM would emit thought-only text under stop_reason=ToolUse and the executor would loop until token budget exhausted with no file_write call.  Bug 14 was previously informational; this v0.1.2 fix promotes it to an active failure mode that's now closed.  Commit `5dd0e09`.

- **Default `ExecutorConfig.max_total_tokens` raised 1M → 5M.**  Multi-turn investigation agents accumulate ~50-100k tokens per turn (full message history re-sent each call).  20-iteration runs routinely hit 1M on v4-priced runs and silently returned `MaxTokens`.  5M gives ~50-turn headroom; cost is unaffected (charged per-API-call, not per-config-value).  Also added a `tracing::warn!` log for the budget-exhaustion path (was silent) and a `tracing::info!` at the loop top for diagnosing stuck runs.  Commit `5dd0e09`.

### Known issues (fixed in 0.1.2)

The v0.1.1 release shipped with Bug 13 still open; that's now closed.

---

## [0.1.1] — 2026-04-25

Patch release closing 5 production bugs surfaced by the v0.1.0 self-reflection run and a parallel senior-engineer audit.  No new features; no API or wire-protocol changes.  Full report: [`docs/reflection/2026-04-25-v0.1.0-first-real-findings.md`](docs/reflection/2026-04-25-v0.1.0-first-real-findings.md).

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.1.1> — `aaos_0.1.1-1_amd64.deb`.

### Fixed

- **Bug 12 (medium)** — `glob_matches` separator-boundary check.  `crates/aaos-core/src/capability.rs` checked `canonical.starts_with(&norm_prefix)` without verifying the following byte is a path separator.  Pattern `/data/*` incorrectly accepted `/data-foo/x` and `/data_foo/x`.  Fixed: require that the character immediately after the prefix is absent (exact-dir match) or `/`.  Two new regression tests: `glob_boundary_dash_prefix_denied` and `glob_boundary_underscore_prefix_denied`.

- **Bug 15 (medium-high)** — `pending_responses` RAII cleanup.  `crates/aaos-runtime/src/services.rs` registered a oneshot sender before `route()` and did not clean it up on route error or timeout.  Every timed-out or routed-to-dead-agent `send_and_wait` permanently leaked a `DashMap` entry; `pending_count()` grew monotonically.  Fixed: added `MessageRouter::cancel_pending` + a RAII `PendingGuard` inside `send_and_wait` that removes the entry on any early return.  New regression test: `send_and_wait_timeout_cleans_up_pending` asserts `pending_count() == 0` after a timeout.

- **Bug 16 (medium)** — `SqliteMemoryStore::store` explicit transaction.  `crates/aaos-memory/src/sqlite.rs` ran DELETE then INSERT as separate auto-commits.  A failed INSERT left the old record permanently deleted.  Fixed: wrapped both statements in `conn.transaction()` + `tx.commit()`.  Existing `replaces_is_atomic` test continues to pass.

- **Bug 10 (high)** — `max_invocations` now enforced at the `ToolInvocation` layer.  `crates/aaos-tools/src/invocation.rs` called `permits()` (read-only) but never `authorize_and_record()`.  Capability `max_invocations` constraints were dead code — an agent could invoke any tool unlimited times regardless.  Fixed: replaced `any()` scan with `find()` to retain the matching handle, then calls `authorize_and_record` after a successful tool execution.  If the token is revoked or expired in the window between the two calls, a warning is logged and the already-completed invocation is not failed (can't undo).  New test: `max_invocations_enforced_through_invoke`.

- **Bug 11 (narrowed, not closed)** — Revoked and expired tokens filtered before forwarding to workers.  `crates/aaos-core/src/capability_registry.rs::resolve_tokens` previously forwarded all tokens regardless of revocation status; workers received and honoured revoked tokens in their per-call registry.  Fixed: filter out `is_revoked() || is_expired()` tokens in `resolve_tokens` so workers only receive currently-valid tokens at dispatch time.  **Residual race:** a token revoked *after* `resolve_tokens` runs but *before* the worker invokes the tool is still honoured by the in-flight call.  Closing this fully requires a push-revocation protocol (Option A) — queued for v0.2.x.  New test: `resolve_tokens_filters_revoked`.

### Test count

625 (v0.1.0) → 631 (+6 new regression tests across `aaos-core`, `aaos-ipc`/`aaos-runtime`, and `aaos-tools`).

---

## [0.1.0] — 2026-04-24

Architectural release.  Unifies both orchestration paths (plan/decompose and persistent/direct) through the PlanExecutor.  Each subtask now runs as a full multi-turn agent with a role-configurable iteration budget.  Bug 9 (hallucinated fallback reports) is closed by deleting the fallback path.

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.1.0> — `aaos_0.1.0-1_amd64.deb`.

### Added

- **`role.orchestration.max_iterations`** — optional `orchestration:` block in role YAML sets the per-subtask multi-turn iteration budget.  Default 50 if absent.  Replaces the old `retry.max_attempts + 10` formula (floor 10).  Bundled role values: `fetcher` 10, `writer` 30, `analyzer` 30, `generalist` 50, `builder` 50.
- **`role.require_declared_output`** — optional boolean (default `false`).  When `true`, a subtask that finishes without writing its declared `file_write` output is a hard failure, not an advisory.  `fetcher` sets this to `true`.
- **`SubtaskOutputStatus` enum** — `check_declared_outputs_exist` now returns `Present`, `MissingAdvisory(String)`, or `MissingFatal(String)`.  Advisory path emits `AuditEventKind::SubtaskOutputMissing` and continues as success; fatal propagates as a subtask failure.
- **`AuditEventKind::SubtaskOutputMissing { subtask_id, declared_path }`** — advisory audit event emitted when a subtask's declared output file is absent and `require_declared_output: false`.
- **`PlanExecutor::run_with_plan(initial_plan, goal, run_id)`** — new method that starts from a pre-built `Plan` and skips the Planner call entirely.  Used by the Direct path.
- **`inline_direct_plan(goal, run_id)`** — server-side function that builds a 1-node generalist `Plan` for the Direct orchestration path.

### Changed

- **Both orchestration modes now route through PlanExecutor.**  `plan` (now `decompose`) calls `PlanExecutor::run()` as before.  `persistent` (now `direct`) calls `PlanExecutor::run_with_plan()` with a 1-node inline plan — the Bootstrap persistent agent is no longer used for per-submit work.
- **Classifier output changed from `plan`/`persistent` to `decompose`/`direct`.**  New `DecompositionMode` enum in `orchestration_classifier.rs`.  Classifier prompt updated: asks whether the goal has independent parallelisable subtasks.  Fallback on LLM error changed from `direct` (was `plan`).  Wire API (`--orchestration plan|persistent`) preserved; `plan → Decompose`, `persistent → Direct`.
- **Subtask iteration budget now reads from `role.orchestration.max_iterations`** instead of `retry.max_attempts + 10`.  Old default was ~12; new default is 50.  Open-ended goals benefit most: a single-subtask direct run now has 50 turns instead of 12.
- **`NoopOrchestrationClassifier` now returns `Direct`** (was `Plan`).  When no LLM client is configured, the daemon routes all submissions to the generalist single-agent path rather than attempting a Planner call that would immediately fail.
- **Architecture docs updated** — "Orchestration modes" section rewritten to reflect the unified PlanExecutor path, new role YAML fields, `SubtaskOutputStatus`, and `fallback_generalist_plan` removal.

### Removed

- **`fallback_generalist_plan`** function in `executor.rs` — closes Bug 9.  A malformed Planner response now propagates as `ExecutorError::Correctable`; the replan loop handles retries; after `max_replans` the run fails cleanly with no hallucinated report.  The `PlannerError::Malformed → fallback_generalist_plan` arm in `PlanExecutor::run()` is gone.
- **Bootstrap streaming path in `server.rs`** — `handle_submit_streaming` no longer has a Bootstrap arm.  `ensure_bootstrap_running`, `route_goal_to`, `event_in_subtree` helper methods deleted.  `submit_streaming_writes_events_then_end_frame` integration test (Bootstrap-specific) deleted; replaced by the routing tests added in v0.0.5.
- **Bug 9 from the Known issues list** — the fallback-generalist hallucination path is structurally impossible in v0.1.0.  See `docs/reflection/2026-04-24-v0.0.3-self-reflection.md` for the closure write-up.

### Fixed

- Test count: 613 → 625 workspace-wide.  Net gain despite deleting the Bootstrap streaming integration test (`submit_streaming_writes_events_then_end_frame`) — that test was replaced by more precise unit tests for the new routing logic, plus new tests for `SubtaskOutputStatus`, `RoleOrchestration`, `run_with_plan`, and classifier behaviour under `decompose`/`direct` labels.

---

## [0.0.5] — 2026-04-24

Third same-day release.  Adds per-submit orchestration routing with LLM-driven auto-detection as the default — `agentd submit` no longer forces every goal through the Planner + PlanExecutor DAG path.  Structured goals still take the DAG path; open-ended exploration / investigation goals route to a persistent Bootstrap agent that manages its own multi-turn context.

Surfaced as a direct response to the v0.0.3 and v0.0.4 self-reflection droplet runs, which exposed the computed-orchestration path as architecturally unsuited to bug-hunting-class goals (per-subtask LLMs are single-shot with capped iteration budgets; they exhaust the budget exploring and never commit).  The Bootstrap persistent path still existed in the codebase but was only reachable by deleting the role catalog, an all-or-nothing switch.  v0.0.5 makes it a per-submit choice, default auto-detected.

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.0.5> — `aaos_0.0.5-1_amd64.deb`.

### Added

- **Auto-routing: `agentd submit` now classifies the goal and picks `plan` or `persistent` automatically.** A cheap single-shot LLM call (~50 input / 1 output token, routes through the configured provider — DeepSeek `deepseek-chat` or Anthropic) inspects the goal text before any agent work begins and routes accordingly.  Classifier prompt is terse and asks for a single-word response; response parsing is forgiving (substring match on `plan` / `persistent`).
  - `plan` — Planner + PlanExecutor DAG.  Best for structured goals with declared outputs per subtask (fetch, analyse, write).  Requires a loaded role catalog; returns a clear error if the catalog is absent.
  - `persistent` — Bootstrap persistent agent.  Best for open-ended, exploratory, or long-context goals where a single multi-turn agent manages its own context and spawns children as needed.
  - Classifier falls back to `plan` on any LLM error or unparseable response.  When no LLM client is configured, auto-routes to `plan` immediately (no network call, no hanging).
  - **Override available**: `agentd submit --orchestration [plan|persistent] "<goal>"` bypasses the classifier.  Explicit wins.
  - **Audit visible**: an `OrchestrationSelected { mode, source }` audit event fires on every submit so operators can see which path was chosen and why (`source: "explicit"` or `"auto"`).  A `tracing::info!` log line `orchestration mode selected mode=<Plan|Persistent> source=<auto|explicit>` also lands in journald.
  - **JSON-RPC surface**: the `agent.submit_streaming` method accepts an optional `"orchestration"` field in its params.  Present → explicit; absent → classified.  Clients built against older servers that always defaulted to plan continue to work (they just don't get classification).
- **Per-submit routing gate in `server.rs`** replaces the startup-time `if let Some(executor)` all-or-nothing gate.  Plan mode errors cleanly when no role catalog is loaded instead of silently falling through to Bootstrap.

Commits: `1beaf22` (CLI flag), `a9bbfe2` (routing gate), `976aa95` (initial docs), `5dc20fd` (classifier module + tests), `4ddc959` (classifier wiring), `e1c3d73` (auto-detect docs).

### Changed

- Test count: 592 → 613 across the workspace.  +21 net: 12 new classifier unit tests, 4 new CLI tests, 5 new / updated server routing tests.

---

## [0.0.4] — 2026-04-24

Second release from the same day as v0.0.3.  The v0.0.3 self-reflection droplet run (aaOS reading its own source tree under confinement) surfaced Bug 8 within 45 seconds of investigation.  No new features; patch-level release.

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.0.4> — `aaos_0.0.4-1_amd64.deb`.

### Fixed

- **Bug 8** — `grep` tool now routes daemon-side under confinement.  `grep` shells out to `rg` (ripgrep) as a subprocess; the worker's seccomp kill-filter denies `execve`, so every grep call under the namespaced backend failed with `ipc error: failed to spawn rg: Operation not permitted (os error 1)`.  Same class as Bug 7 (routing-list drift between `WORKER_SIDE_TOOLS` and `DAEMON_SIDE_TOOLS`).  Moved `"grep"` from `aaos_backend_linux::worker_tools::WORKER_SIDE_TOOLS` to `aaos_core::tool_surface::DAEMON_SIDE_TOOLS`; dropped the `GrepTool` registration from `build_worker_registry`; flipped the routing tests.  Commit `aaf82a3`.

---

## [0.0.3] — 2026-04-24

Ships the Bug 7 fix queued from the v0.0.2 extended QA pass.  No new features; patch-level release to unblock confined agents that call memory tools.

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.0.3> — `aaos_0.0.3-1_amd64.deb`.

**Known issues (fixed in 0.0.4):** Bug 8 — `grep` tool fails with `Operation not permitted` under the namespaced backend because ripgrep subprocess spawn is blocked by seccomp.  Affects any role that uses grep while confined (including the `reflector` role for self-reflection runs).  Upgrade to v0.0.4.

### Fixed

- **Bug 7** — `memory_store`, `memory_query`, `memory_delete` now correctly route daemon-side under confinement.  Previously these tools were absent from both `WORKER_SIDE_TOOLS` and `DAEMON_SIDE_TOOLS`, causing a `tool error: tool memory_X not available in worker` failure under the namespaced backend.  Memory tools need HTTP access to the embedding endpoint that the worker sandbox can't provide, so they join `web_fetch`, `cargo_run`, and `git_commit` in `DAEMON_SIDE_TOOLS` in `aaos-core::tool_surface`.  Surfaced by the v0.0.2 extended-QA pass in [`docs/reflection/2026-04-19-v0.0.2-droplet-qa.md`](docs/reflection/2026-04-19-v0.0.2-droplet-qa.md).  Commit `03d384f`.

---

## [0.0.2] — 2026-04-19

First QA-driven patch.  Fresh-droplet soak test of the v0.0.1 `.deb` surfaced six bugs; this release fixes all of them.  See [`docs/reflection/2026-04-19-v0.0.1-droplet-qa.md`](docs/reflection/2026-04-19-v0.0.1-droplet-qa.md) for the original QA record and [`docs/reflection/2026-04-19-v0.0.2-droplet-qa.md`](docs/reflection/2026-04-19-v0.0.2-droplet-qa.md) for the v0.0.2 verification pass (all six bugs confirmed closed end-to-end; one new Bug 7 surfaced — fixed in `[0.0.3]` above).

**Known issues (fixed in 0.0.3):** Bug 7 — memory tools (`memory_store` / `memory_query` / `memory_delete`) fail under the namespaced backend with `tool error: tool memory_X not available in worker`.  Affects agents that declare memory capabilities while running confined.  No workaround in v0.0.2; upgrade to v0.0.3.

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.0.2> — `aaos_0.0.2-1_amd64.deb`, 4.29 MB.

### Fixed

- **[Critical]** `.deb` now includes the `namespaced-agents` feature.  v0.0.1 shipped with `--features mcp` only, so `AAOS_DEFAULT_BACKEND=namespaced` (whether operator-set or generated by the postinst probe) silently fell through to `InProcessBackend`.  Every tool call audit-tagged `[daemon]` regardless of env.  `packaging/build-deb.sh` default `AAOS_BUILD_FEATURES` is now `mcp,namespaced-agents`.  Commit `160861f`.
- `NamespacedBackend::stop` now reaps child processes.  Prior versions left an `[aaos-agent-worker] <defunct>` zombie in the process table per subtask run; over a long-lived daemon these would accumulate until the PID ceiling.  New flow: SIGTERM → poll WNOHANG for 500 ms → escalate to SIGKILL + blocking reap.
- `agentd submit` now renders the daemon's error message on failed runs.  A mistyped or expired API key previously produced `bootstrap failed (0k in / 0k out, 0s)` with zero context; the error field on the streaming `end` frame was discarded by the CLI.  Now prints `error: <message>` with a pointer at `journalctl -u agentd`.  Daemon also emits a structured `tracing::error!` with `run_id`.
- MCP subsystem now logs startup state.  `"MCP client: attempting to connect to N configured server(s)"`, `"MCP client: N of M server(s) registered"`, `"MCP server: starting loopback listener on <bind>"`.  No-config case prints `"no /etc/aaos/mcp-servers.yaml — MCP disabled (copy .example to enable)"`.  Prior versions were completely silent about MCP state.
- `"using NamespacedBackend"` startup log fires once per process instead of twice.  `Server::new()` + `Server::with_llm_client()` both built a backend; OnceLock guard deduplicates.
- All 11 lintian errors addressed.  `packaging/debian/copyright` references `/usr/share/common-licenses/Apache-2.0` per Debian Policy 12.5; `packaging/debian/changelog` added per Policy 12.7; `packaging/agentd.service` moves to `usr/lib/systemd/system/` per Debian Trixie merged-usr; release binaries explicitly stripped (`strip --strip-unneeded` in `build-deb.sh`); `packaging/debian/lintian-overrides` covers remaining warn-level tags with per-tag rationale.

### Changed

- `.deb` size shrinks 4.92 MB → 4.29 MB after binary stripping.
- Final lintian output: **0 errors, 0 warnings** (down from 11 errors + 9 warnings on v0.0.1).

---

## [0.0.1] — 2026-04-19

First tagged release.  The runtime, capability model, MCP integration, confinement, scheduler, routing, TTL, self-reflection loop, and operator CLI had already shipped as untagged development work (see `[0.0.0]` below); v0.0.1 wrapped them in the release-pipeline infrastructure.

Release: <https://github.com/Joncik91/aaOS/releases/tag/v0.0.1> — `aaos_0.0.1-1_amd64.deb`, 4.25 MB.

### Added

- CI-built `.deb` on `v*` tag push via `.github/workflows/release.yml`.  Builds inside a `debian:13` container so cargo-deb encodes Debian's libc/systemd minimums.  Attaches the artifact to an auto-generated GitHub Release with `contents: write` permission.  Commits `1ae9432` + `f61a967`.
- Agentic-by-default `.deb` surface (formerly roadmap milestone M1, now build-history #15).  Five `.deb`-level changes made the package useful out of the box:
    - `packaging/build-deb.sh` ships `agentd` with `--features mcp` by default (bidirectional MCP both directions on from install).  Commit `a6c993b`.
    - `/etc/aaos/mcp-servers.yaml.example` template with commented-out GitHub MCP (HTTP), filesystem MCP (stdio via npx), and git MCP (stdio via uvx) entries.  Commit `54499de`.
    - 21 AgentSkills bundled under `/usr/share/aaos/skills/` (FHS-correct vendor-supplied read-only data).  `discover_all_skills` probes `/usr/share/aaos/skills/` → `/etc/aaos/skills/` → `/var/lib/aaos/skills/`; `AAOS_SKILLS_DIR` appends last.  Commit `5c78a04`.
    - `packaging/debian/postinst` probes `/sys/kernel/security/lsm` for `landlock` + `/proc/sys/kernel/unprivileged_userns_clone`; generates `/etc/default/aaos.example` with `AAOS_DEFAULT_BACKEND=namespaced` + `AAOS_CONFINE_SUBTASKS=1` uncommented when both probes pass.  Commit `9f18848`.
    - `agentd configure` subcommand: interactive or `--key-from-env` non-interactive API-key setup that atomically writes `/etc/default/aaos` mode 0600 root:root (tempfile + fsync + rename) and restarts the daemon.  Commit `4bb5e38`.
- `namespaced-agents` feature-on compile check in the fast CI job (`check-lint`).  Previously only exercised under `--ignored` with kernel primitives; a compile regression could sneak through.  Commit `801c08d`.

### Changed

- Workspace crates bumped `0.0.0` → `0.0.1` (`aaos-mcp` stays on its own `0.1.0` cadence).  Commit `779dd62`.
- Clippy CI gate flipped from advisory (`continue-on-error: true` + `-W clippy::all`) to enforced (`-D warnings`).  Required fixing 57 latent warnings first; most auto-fixed via `cargo clippy --fix`, the remainder got targeted `#[allow(...)]` with rationale for genuinely structural items (type-complexity in MCP transport factory + invocation-test fixtures; too-many-arguments on `persistent_agent_loop` + `build_in_process_backend`; `await_holding_lock` on a sync-mutex-for-env-var test pattern).  Commit `d1c4274`.
- Release workflow `contents: write` permission added so `softprops/action-gh-release@v2` can create Releases and attach the `.deb`.  Commit `f61a967`.

### Known issues (fixed in 0.0.2)

- Confinement silently disabled despite `AAOS_DEFAULT_BACKEND=namespaced` — the release build was missing `--features namespaced-agents`.  Download the `v0.0.2` `.deb` instead.
- Zombie `aaos-agent-worker` children accumulate after each run.
- Invalid API keys fail silently with no error message.
- MCP subsystem startup state completely silent.

---

## [0.0.0] — pre-tagged development (2026-03-21 through 2026-04-19)

Before v0.0.1 there was a month of untagged development.  What existed in the tree at the moment the v0.0.1 tag was cut, collapsed into a retrospective changelog:

### Added — runtime foundation

- **Runtime prototype** (2026-03-21, commit `029d90b`).  6 Rust crates, 3,917 lines, 111 passing tests.  Capability-based security with two-level enforcement (tool access + resource path), `AgentServices` + `Tool` traits, LLM execution loop, MCP message routing, human-in-the-loop approval queue.
- **Persistent agents + request-response IPC** (2026-03-25).  `persistent_agent_loop` on a tokio task; `send_and_wait` on `AgentServices`; JSONL session store; Pause/Resume/Stop commands; 30 new tests, 141 total.
- **Managed context windows** (late March).  `ContextManager` with LLM-based summarization when usage exceeds a configurable threshold.  Older messages archived to disk; Summary messages folded into the system prompt.  25 new tests, 166 total.
- **Episodic memory store** (late March).  New `aaos-memory` crate (7th workspace member) with `MemoryStore` trait + in-memory cosine-similarity impl + Ollama/mock embedding sources.  `memory_store`/`memory_query`/`memory_delete` tools.  39 new tests, 205 total.
- **Self-bootstrapping swarm** (early April).  Docker container with `agentd` as PID 1 and a Bootstrap Agent that self-organizes child agents to accomplish goals.  Canonical run: "fetch HN top 5 and write a summary" completes in ~75 s for ~$0.03.

### Added — provider support + scheduling

- **Multi-provider LLM** (early April).  `OpenAiCompatibleClient` speaks OpenAI Chat Completions; DeepSeek / OpenRouter / any OpenAI-compatible provider works.  Daemon prefers `DEEPSEEK_API_KEY`, falls back to `ANTHROPIC_API_KEY`.
- **Inference scheduler** (early April).  `ScheduledLlmClient` wraps any `LlmClient` with a tokio semaphore (default 3 concurrent) + optional rate smoothing.  Env vars `AAOS_MAX_CONCURRENT_INFERENCE`, `AAOS_MIN_INFERENCE_DELAY_MS`.
- **Per-agent token budgets** (early April).  `BudgetTracker` with lock-free atomic CAS; manifests declare `budget_config: { max_tokens, reset_period_seconds }`; over-budget agents get `BudgetExceeded` errors.

### Added — Debian packaging

- **`agentd` as a Debian package** (2026-04-15, commits `5717906` + `8d45691`).  `.deb` buildable via `cargo deb -p agentd`; `postinst` creates the `aaos` system user + group; systemd `StateDirectory=aaos` + `RuntimeDirectory=agentd` own dir creation; socket at `/run/agentd/agentd.sock` mode 0660.  `postrm purge` cleans state + user.  Hardening: `NoNewPrivileges`, `ProtectSystem=full`, `ProtectHome`, `PrivateTmp`, `ProtectKernelTunables`, `ProtectKernelModules`, `ProtectControlGroups`.  Dependencies: `$auto, systemd, ca-certificates`.
- **Operator CLI** (2026-04-16, commits `58dd1bb`..`5e01acc`).  `agentd submit | list | status | stop | logs | roles` subcommands + server-side NDJSON streaming (`agent.submit_streaming`, `agent.logs_streaming`) + `BroadcastAuditLog` + `aaos` system group + `agentd(1)` man page.
- **Computed orchestration** (2026-04-16, commits `9b001cb`..`cbd3dc7`).  Two-phase boot replacing single-LLM orchestration: cheap-LLM Planner emits a structured `Plan { subtasks, depends_on, final_output }`; deterministic Rust `PlanExecutor` walks the DAG in dependency-ordered batches running independents concurrently via `futures::try_join_all`.  126 new runtime tests.  Role catalog at `/etc/aaos/roles/` (fetcher, writer, analyzer, generalist).
- **Computed-orchestration follow-ups** (2026-04-17).  Planner prompt rules (`dfb97f9`), `{inputs.*}` capability expansion (`6b2387e`), role-budget wiring into per-subtask `ExecutorConfig` (`ef45e61`), tightened fetcher/analyzer/writer prompts (`c412a14`).  Canonical goal timing 5m30s → 28s.
- **Deterministic scaffold roles** (2026-04-17, commit `2b8ed6d`).  Roles can declare `scaffold: kind: <name>`; `PlanExecutor` dispatches to a `ScaffoldRunner` closure instead of an LLM loop.  Fetcher ships as the first scaffold: `web_fetch → file_write → return workspace path` with HTTP-status + empty-body rejection.  Closes the fabrication bug where LLMs emitted plausible `"written to <path>"` acks without calling `file_write`.
- **`cargo_run` tool + `builder` role** (2026-04-17, commit `45ce06b`).  Executes `cargo {check,test,clippy,fmt}` in a capability-scoped workspace; subcommand allowlist + 4-minute timeout + 8 KB inline output cap.
- **Bidirectional MCP integration** (2026-04-18).  `aaos-mcp` crate.  **Client:** per-entry stdio or HTTP sessions with `initialize` + `tools/list` handshake, tools register as `mcp.<server>.<tool>`, per-session reconnect loop with exponential backoff.  **Server:** axum HTTP+SSE listener on `127.0.0.1:3781` exposing `submit_goal`, `get_agent_status`, `cancel_agent`; SSE stream at `GET /mcp/events?run_id=<id>`.

### Added — Agent-kernel primitives

- **Reasoning-slot scheduler** (2026-04-18, commits `c2b56de`..`9b8e15a`).  Runtime-owned `ReasoningScheduler` awards LLM inference slots via a `BinaryHeap<Reverse<ReasoningRequest>>` priority queue keyed on subtask wall-clock deadline.  `SchedulerView` wraps the LLM client per subtask; every subtask's `complete()` call routes through the scheduler and records elapsed time in a `LatencyTracker`.
- **Per-task TTL + latency** (2026-04-18).  `TaskTtl { max_hops, max_wall_clock }` on `Subtask`.  `spawn_subtask` refuses launch when `max_hops == 0`; `tokio::select!` race cancels the runner future on wall-clock expiry.  Emits `SubtaskTtlExpired { reason }` audit events.
- **Dynamic model routing** (2026-04-19, commits `cd55c8c`..`68c9112`).  Roles declare `model_ladder: Vec<String>` + `escalate_on: Vec<EscalationSignal>`.  `Subtask.current_model_tier` bumps on replan when a configured signal (`ReplanRetry`, `ToolRepeatGuard`, `MaxTokens`) fired during the failed attempt.  `SubtaskModelEscalated` + `ToolRepeatGuardFired` audit events operator-visible in the default `agentd submit` stream.
- **Runtime-side tool confinement** (2026-04-19, commits `0a47bb3`..`7adc147`).  When `AAOS_DEFAULT_BACKEND=namespaced`, plan-executor subtasks + `spawn_agent` children run their filesystem + compute tools inside the worker under Landlock + seccomp.  `ToolInvocation::invoke` routes via `route_for(tool_name, backend_kind)` → worker over the post-handshake broker stream, or daemon-side for tools that inherently need the daemon's authority.  Capability tokens forwarded with each `InvokeTool`.  Workspace + manifest-declared output roots bind-mounted at matching absolute paths.  Worker-side whitelist: `file_read`, `file_write`, `file_edit`, `file_list`, `file_read_many`, `grep`.  Daemon-side permanently: `web_fetch`, `cargo_run`, `git_commit`, the LLM loop.

### Added — supporting infrastructure

- **Self-reflection log** — 41 dated run entries under `docs/reflection/`; each captures setup, what worked, what the run exposed, what shipped, and cost.  Cross-cutting lessons lifted into `docs/patterns.md`.
- **AgentSkills support** — skill loader parsing upstream `SKILL.md` files; `SkillRegistry` + `skill_read` tool with path-traversal protection; skill catalog injected into agent system prompts at spawn time.  21 skills bundled from [addyosmani/agent-skills](https://github.com/addyosmani/agent-skills).
- **Capability-token forgery — threat-model split.** Four distinct threat classes enumerated in `docs/ideas.md`; in-process forgery closed (handle-opaque tokens, handle field private to `aaos-core`), worker-side forgery closed (peer-creds on broker, no handles in launch protocol), registry memory tampering named as open (needs external key storage), cross-process transport named as N/A-until-Phase-G.

### Changed

- `agentd` 6 crates → 9 crates, ~4 k LoC → ~37 k LoC, 111 tests → 605+ passing + 19 `#[ignore]`-gated.
- Handle-based capability tokens: `aaos-tools` never sees a `CapabilityToken` struct, only opaque `CapabilityHandle` values; the handle's inner `u64` is `aaos-core`-private.

No `.deb` was attached to a `v0.0.0` tag — this release was the untagged development line.  The first installable artifact is `v0.0.1`'s `.deb` (see above).

---

[Unreleased]: https://github.com/Joncik91/aaOS/compare/v0.2.8...HEAD
[0.2.8]: https://github.com/Joncik91/aaOS/compare/v0.2.7...v0.2.8
[0.2.7]: https://github.com/Joncik91/aaOS/compare/v0.2.6...v0.2.7
[0.2.6]: https://github.com/Joncik91/aaOS/compare/v0.2.5...v0.2.6
[0.2.5]: https://github.com/Joncik91/aaOS/compare/v0.2.4...v0.2.5
[0.2.4]: https://github.com/Joncik91/aaOS/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/Joncik91/aaOS/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/Joncik91/aaOS/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/Joncik91/aaOS/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/Joncik91/aaOS/compare/v0.1.7...v0.2.0
[0.1.7]: https://github.com/Joncik91/aaOS/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/Joncik91/aaOS/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/Joncik91/aaOS/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/Joncik91/aaOS/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/Joncik91/aaOS/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/Joncik91/aaOS/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/Joncik91/aaOS/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Joncik91/aaOS/releases/tag/v0.1.0
[0.0.5]: https://github.com/Joncik91/aaOS/releases/tag/v0.0.5
[0.0.4]: https://github.com/Joncik91/aaOS/releases/tag/v0.0.4
[0.0.3]: https://github.com/Joncik91/aaOS/releases/tag/v0.0.3
[0.0.2]: https://github.com/Joncik91/aaOS/releases/tag/v0.0.2
[0.0.1]: https://github.com/Joncik91/aaOS/releases/tag/v0.0.1
[0.0.0]: https://github.com/Joncik91/aaOS/commits/779dd62
