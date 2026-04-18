# Persistent broker↔worker stream + a latent bug surfaced *(2026-04-18)*

**Integration commits:** `7f7894d` (feat: persistent stream + ping round-trip), `f284cc0` (fix: seccomp allowlist for mio 1.1's actual syscall set)

Closed the "wiring partial" caveat on the capability-forgery threat-model split's worker-side class. The post-handshake broker→worker transport now carries real traffic end-to-end under the sandbox. Along the way surfaced a latent seccomp gap that had been silently killing every namespaced worker the moment it tried to do anything real after `sandboxed-ready`.

## What shipped

1. **Protocol**: `Request::Ping { nonce: u64 }`. Worker echoes the nonce in `result.nonce`; no sandbox-escape semantics — purely a transport proof of life.
2. **BrokerSession**: new `write_half: Mutex<Option<OwnedWriteHalf>>` and a pending-responses correlation map (`Mutex<HashMap<u64, oneshot::Sender<WireResponse>>>`) keyed by request id. `send_ping` and `send_poke` wrap a private `send_request` helper that serializes the write under the mutex, parks a oneshot, and awaits the matching response or a timeout.
3. **`install_post_handshake_stream`**: called by the backend after `run_handshake` succeeds. Moves the write half into the session and spawns a reader task that parses incoming `WireResponse` frames and routes them to awaiters. On worker shutdown or read error the reader drains the pending map so awaiters wake with `ResponseChannelClosed` instead of hanging.
4. **`NamespacedBackend::session(&AgentId)`**: public accessor so integration tests (and, eventually, higher-level tool-routing code) can reach the live session.
5. **Worker-side**: `Request::Ping` dispatch added. `handle_poke` renamed to `handle_poke_with_id(req_id, op)` — the old version hardcoded response `id: 0`, which was fine when nothing correlated replies; wrong once the new send_request path started matching responses by id.
6. **Tests**: two `#[ignore]`-gated integration tests added. `ping_roundtrips_over_persistent_stream` sends two pings with different nonces and asserts both round-trip; `worker_cannot_execve` was upgraded from a launch+stop scaffold (that had been carrying an explicit `#[ignore = "scaffold: needs broker-side persistent stream"]` since it shipped) to the real thing: send `PokeOp::TryExecve`, expect either an error response or a send error + dead-worker health.

## The latent bug

The new ping test failed on the first droplet run with `BrokenPipe` on the broker's very first write. Debug output showed the worker had already exited (`health: Exited(101)` — Rust panic exit code). The child's stderr was redirected to `/dev/null` by the backend's `child_fn` (with a comment calling the redirect "cosmetic"), so no panic message ever reached the logs. Temporarily rerouting stderr to a host-visible file exposed the panic:

```
thread 'main' panicked at tokio/runtime/io/driver.rs:196:23:
  unexpected error when polling the I/O driver:
  Os { code: 1, kind: PermissionDenied, message: "Operation not permitted" }
```

mio 1.1 on kernel 6.12 uses `epoll_wait` and `epoll_pwait2` in preference to `epoll_pwait`. Neither was in the seccomp allowlist. EPERM was being returned for every tokio poll, tokio panicked, and the worker died right after flushing `sandboxed-ready`. Every previous run of the namespaced backend — the 2026-04-15 and 2026-04-17 verifications — had measured `Seccomp: 2 / NoNewPrivs: 1` in `/proc/<pid>/status` correctly, but never issued a post-handshake syscall that would have tripped the missing entries. The existing `launch_reaches_sandboxed_ready` test passed because it only checked that sandboxed-ready arrived (which happens before the panic), not that the worker survived beyond it.

Fix: add `libc::SYS_epoll_wait` (x86_64) and literal syscall number `441` (`epoll_pwait2`, same on x86_64 and aarch64 since 5.11) to the allowlist. Leave `epoll_pwait` in place — mio still picks it when a signal mask is needed. Updated the stderr-redirect comment in `child_fn` from "cosmetic" to an explicit warning pointing future debuggers at the host-visible-file pattern.

## What this proves

- **"Cosmetic error output" claims warrant suspicion.** The comment explaining the `/dev/null` redirect predicted the exact situation that hid this bug — "tokio can emit a noisy panic to stderr when it polls fds after the mount namespace transition. That panic is cosmetic." It was not cosmetic; it was load-bearing. A comment that says "X is harmless" should be treated as a conjecture until a test exercises X.
- **Integration coverage matters more than structural metrics.** Three prior verification runs reported accurate sandbox metrics and still missed the bug. The missing piece wasn't more measurement — it was one syscall after the handshake. The ping test is ~10 lines and would have caught this six weeks ago if it had existed.
- **Corporate-grade threat-model splits pay off.** The "wiring partial" qualifier on the worker-side forgery claim was there because I couldn't honestly claim the transport worked end-to-end. Writing that qualifier flagged the gap, the gap motivated the test, the test caught the bug. The specific-PARTIAL-over-vague-PARTIAL move from the previous doc pass isn't just cosmetic honesty — it tells you what to build next.

## What this didn't prove

- **Tool routing still lives in `agentd`.** The transport is exercised, but the LLM loop and tool invocations for namespaced agents still execute host-side. The capability-mapping decision for routing `file_read` / `file_write` / `web_fetch` / etc. through the stream is a separate design pass tracked under "Runtime-side confinement of tool execution" in ideas.md.
- **Not every tokio syscall is covered.** We found two missing; there may be others on other kernel versions or with a future mio update. A `cargo test -- --ignored` run on each new kernel is the only real regression gate.
- **No production workload has actually exercised the namespaced backend.** The integration tests are isolated exercises of launch + stop + ping + poke. Running a real role (`builder`, `generalist`) under `AAOS_DEFAULT_BACKEND=namespaced` for a full self-build run would surface the next tier of missing coverage — capability plumbing, IPC framing overhead, agent-loop-inside-worker issues. That's the natural follow-up.

## Docs updated in the same commit as this entry

- `docs/ideas.md`: "Worker-side forgery on NamespacedBackend" — **CLOSED BY DESIGN, WIRING PARTIAL** → **CLOSED, TRANSPORT EXERCISED**.
- `docs/architecture.md` capability-model summary: same status change.
- `SECURITY.md` security-model section: same status change.
- `docs/security-gaps.md` (gitignored local ledger): same status change + a bullet recording the stderr-redirect-hid-the-panic lesson.

## Cost

~90 minutes of Opus work. One droplet hour (~$0.03 per DO dashboard equivalent; not checked today). The first droplet test run failed informatively, the second after the seccomp fix passed all five tests.
