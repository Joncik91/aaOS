# Worker-Side Tool Confinement — Runtime-Side Execution on `NamespacedBackend`

**Status:** Draft (2026-04-19). Third sub-project of Phase F-b (Standard-spec completion).
**Scope:** Gap 3 from [`roadmap.md`](roadmap.md) Phase F-b. Closes the last capability-model hole: tool code runs in the worker's sandbox, not in `agentd`'s address space.

## Goal

When `AAOS_DEFAULT_BACKEND=namespaced`, agent tool invocations execute **inside the worker**, after `PR_SET_NO_NEW_PRIVS` → Landlock → seccomp have been applied. The daemon keeps policy (capability check + audit + repeat-guard) and routes the tool call across the existing broker stream; the worker dispatches the call against a local `WorkerToolRegistry` and returns the result.

A reader of the spec's "capability tokens are the authority, the sandbox is the substrate" should find tool code that — at the moment a bug in `file_write` would matter — sits behind seccomp, not in the daemon's address space. Today the `NamespacedBackend` worker handshakes, sandboxes itself, accepts `Ping`/`Poke`, and otherwise sleeps. This sub-project wires tool dispatch over the persistent broker stream so the sandbox does its job.

## Non-goals

- **No network-tool confinement in v1.** The worker has no `socket`/`connect` in its seccomp allowlist and no outbound network in Landlock. `web_fetch` stays daemon-side, behind the same capability check, with an explicit `execution_surface: daemon` audit field so operators can see the distinction. Confining network tools requires either a broker-mediated HTTP egress proxy or relaxing the worker sandbox; both are their own sub-project.
- **No subprocess-tool confinement in v1.** `cargo_run` and `git_commit` spawn processes; seccomp's kill-filter denies `execve` (that's what the `TryExecve` poke test verifies). They stay daemon-side in v1 alongside `web_fetch`.
- **No per-tool Landlock scoping.** All worker-side tools share the single ruleset built at `SandboxedReady`. A capability grant to `/data` will fail at Landlock even if daemon-side policy permits it; that surfaces as `ToolDenied { reason: "landlock: write to <path> denied" }`. This is correct by design — worker Landlock is narrower than daemon capability grants on purpose — but v1 does not dynamically extend Landlock per tool call.
- **No mid-invoke cancellation.** A tool call has a timeout (default 60s); a worker stuck past the timeout is killed and re-spawned at the existing supervisor layer. Sub-project 1's TTL + sub-project 2's escalation cover recovery.
- **No LLM loop in the worker.** Daemon keeps provider clients, context manager, and budget tracking. That preserves today's shape and means `DEEPSEEK_API_KEY` never crosses the broker. A future sub-project can push the LLM loop down if there's demand; this one does not.

## Non-goals (carried forward from sub-projects 1 + 2)

- No cross-subtask state. Each tool call is independent; no per-worker tool-call memoization or caching.
- No regressions on the default `--features mcp` build. The in-process backend path is unchanged.

## Architecture

Four small pieces on top of sub-project 0 (broker stream) and the existing daemon-side `ToolInvocation`:

- **`Request::InvokeTool { tool_name, input, request_id }` + `Request::InvokeToolOk { request_id, result }` + `Request::InvokeToolErr { request_id, reason }`** in `crates/aaos-backend-linux/src/broker_protocol.rs`. The tool call and its response are first-class messages on the existing persistent stream.
- **`WorkerToolRegistry`** — a thin wrapper around `ToolRegistry::builtins_except(&[DAEMON_SIDE_TOOLS])` constructed inside the worker after `sandboxed-ready` fires. Registered tools: `file_read`, `file_write`, `file_edit`, `file_list`, `file_read_many`, `grep`, `skill_read`, `memory_query`, `memory_store`, `memory_delete`, `context` — everything that reads/writes under Landlock-scoped paths and runs as pure compute inside seccomp.
- **`BrokerSession::invoke_over_worker(tool_name, input) -> Result<Value>`** in `crates/aaos-backend-linux/src/broker_session.rs`. Allocates a request id, inserts a `oneshot::Sender` into a `pending: Mutex<HashMap<u64, oneshot::Sender<WireResponse>>>` map, sends the `InvokeTool`, awaits the receiver. The existing stream-reader task already demuxes by `id`; it now looks up the sender in `pending` and forwards the response.
- **`ToolExecutionSurface` enum (`Daemon | Worker`) + `ToolInvocation::route_for(tool_name, backend_kind)`** — single-point routing decision in `crates/aaos-tools/src/invocation.rs`. The capability check + audit prefix is unchanged; only the execution tail forks on the surface. Audit events gain a `surface: "daemon" | "worker"` field so operators can see which side ran the code.

### Split in one line

```
Daemon: capability check → audit → (fork by surface) → { in-process invoke  OR  broker.invoke_over_worker } → audit → return
Worker: agent_loop dispatch → WorkerToolRegistry.get(tool).invoke(input) → catch_unwind + timeout → respond
```

### Why the daemon still does the capability check

Three reasons:

1. `CapabilityRegistry` holds opaque `u64` handles that resolve against live `AgentId` + policy state. The worker would need the whole registry and its in-memory state to re-check — pushing that across the broker is more code and more trust than just checking daemon-side.
2. Audit is append-only on the daemon's `AuditLog`. Doing the check worker-side means the worker has to emit audit events over the broker, which adds a second stream and doubles the surface area.
3. The worker's Landlock + seccomp are **narrower** than daemon-side capability checks. If daemon-side capability denies, the worker never sees the call; if daemon-side capability permits, Landlock still gets a second vote. Defense in depth, not a single-gate replacement.

### The tool routing table

One constant in `crates/aaos-tools/src/invocation.rs`:

```rust
pub const DAEMON_SIDE_TOOLS: &[&str] = &[
    "web_fetch",      // needs outbound network — seccomp has no socket/connect
    "cargo_run",      // spawns subprocesses — seccomp kill-filter denies execve
    "git_commit",     // spawns subprocesses — same reason
];
```

Everything else is worker-side when `backend_kind == "namespaced"`. For the in-process backend, `route_for` always returns `Daemon` and today's code path runs unchanged.

## Failure modes and how each surfaces

- **Tool not registered in `WorkerToolRegistry`** → Worker replies `InvokeToolErr { reason: "tool <name> not available in worker" }`. Daemon surfaces as `CoreError::ServiceBackend` with the reason verbatim. Not a silent fall-back — routing config is wrong, and silently executing daemon-side would undo the confinement this sub-project exists to provide.
- **Tool panics inside the worker** → `FuturesExt::catch_unwind` wraps the future; on panic the worker sends `InvokeToolErr { reason: "tool <name> panicked: <unwind payload>" }`. Worker does **not** die — a tool panic is not a worker-corruption event (Rust's unwind guarantees) so the broker stream stays open.
- **Tool exceeds 60s timeout** → Worker sends `InvokeToolErr { reason: "tool <name> exceeded 60s timeout" }` and forcibly drops the tool future. If the tool is hung in a syscall that Landlock/seccomp permitted (e.g., a huge read), the drop runs the Drop impl; if that's not enough the next round-trip will fail and supervisor tears the worker down.
- **Worker dies mid-invoke** (SIGSYS from seccomp, OOM, etc.) → Broker session reader task observes EOF, flips all pending `oneshot::Sender`s to error, daemon surfaces as `CoreError::ServiceBackend("worker lost mid-invoke")`. `SubtaskFailed` fires; per-subtask TTL + escalation from sub-project 1 + 2 handle recovery.
- **Landlock denies the tool's filesystem access** → Tool's `invoke()` returns its usual `io::Error` (EACCES / EROFS). That propagates through `ToolInvocation` as today. Audit event carries the error message so operators see *landlock* in the reason.

## Operator visibility

One new audit field, one new CLI-visible line:

- `AuditEventKind::ToolInvoked` gains `execution_surface: ToolExecutionSurface` (serde-renamed `daemon` / `worker`). All existing `ToolInvoked` call sites pick up `Daemon` by default; the broker path sets `Worker`.
- CLI operator view (`agentd submit` stream): tool lines gain a short tag — `tool: file_write [worker]` vs `tool: web_fetch [daemon]`. Default-on; no flag. First-time operators see immediately that confinement is active (or not).

Rationale: sub-project 1 shipped `SubtaskTtlExpired` invisible to operators by default; re-verification caught it. Sub-project 2 made operator-visibility a first-class task (T10). Sub-project 3 follows the same pattern — you can't tell it's working unless you can see it.

## Definition of Done

Fresh DigitalOcean droplet, `--features namespaced-agents` build, `AAOS_DEFAULT_BACKEND=namespaced`:

1. Canonical "fetch HN + lobsters, write 800-word comparison" goal completes end-to-end. `web_fetch` runs daemon-side (documented); `file_write` + `file_read` for the report run worker-side.
2. `AAOS_NAMESPACED_CHILD_DEBUG=1` shows `InvokeTool` / `InvokeToolOk` round-trips on the broker stream for every worker-side tool call. Count: one `InvokeTool` per non-`DAEMON_SIDE_TOOLS` tool call the agent makes.
3. CLI operator view shows `[worker]` tag on `file_*` tool lines, `[daemon]` tag on `web_fetch` lines.
4. Guardrail test: grant an agent `file_read` for `/etc/shadow`, invoke. Daemon-side capability check **may** pass (if the grant exists); worker-side Landlock **must** deny and surface as `ToolDenied`. Proves Landlock is the second gate, not just decoration.
5. Regression: default `--features mcp` build unchanged. No new broker traffic on the in-process path.
6. No `panic|backtrace` in `journalctl -u agentd`. No daemon crash even if a worker-side tool panics (panic is caught, reported as `InvokeToolErr`).

## Risks and mitigations

- **Request correlation is real plumbing.** Today's broker stream is a FIFO of in-order round-trips. Multiple concurrent `InvokeTool` calls from parallel subtasks need a pending-request map and a reader task that routes replies by `id`. Mitigation: T4 of the plan is this and only this. Unit-test the correlation in isolation before any worker-side change lands.
- **Tool input may contain paths outside the worker's Landlock scope.** E.g., capability granted for `/data` but worker's scratch is `/var/lib/aaos/session-<id>`. Landlock denies. This is correct — worker Landlock is narrower on purpose — but the error needs to be a structured `ToolDenied { reason: "landlock: <path>" }`, not a generic tool error. Mitigation: T9 exercises this case end-to-end and pins the error shape.
- **Worker registry must not include unavailable tools.** If v1 accidentally registers `web_fetch` in the worker, the first real call will hang on `connect` until seccomp kills the worker with SIGSYS. Mitigation: `WorkerToolRegistry::new()` explicitly lists allowed tools by name (not "everything not in DAEMON_SIDE_TOOLS") — fail closed.
- **Scope honesty from sub-projects 1 + 2.** Say upfront what's confined and what's not. v1 confines filesystem and compute tools; network and subprocess tools stay daemon-side. Both the architecture doc and the operator CLI carry that distinction.

## Out of scope / carried forward

- Network-tool confinement (requires egress proxy or sandbox relaxation — own sub-project).
- Subprocess-tool confinement (requires scoped `execve` + Landlock fs-exec rules — own sub-project).
- LLM-loop confinement (requires provider clients + API-key handling across broker — biggest scope, own phase).
- Per-tool Landlock scoping (requires dynamic ruleset mutation — open research question).
