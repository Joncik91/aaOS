# Worker-Side Tool Confinement Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When `AAOS_DEFAULT_BACKEND=namespaced`, route tool invocations over the broker stream into the worker so the tool code runs under Landlock + seccomp. Daemon keeps capability checks + audit + LLM loop + network tools. Worker runs filesystem + compute tools.

**Architecture:** Extend `broker_protocol::Request` with `InvokeTool` / `InvokeToolOk` / `InvokeToolErr`. Add `WorkerToolRegistry` constructed inside the worker after `sandboxed-ready`. Add `BrokerSession::invoke_over_worker` with a `pending: HashMap<u64, oneshot::Sender<WireResponse>>` map demuxed by the existing reader task. Add `ToolExecutionSurface` (`Daemon | Worker`) and a single routing fork in `ToolInvocation::invoke` keyed off `backend_kind` and a static `DAEMON_SIDE_TOOLS` list (`web_fetch`, `cargo_run`, `git_commit`). Emit a new `execution_surface` field on `ToolInvoked` audit events so the CLI can show `[worker]` / `[daemon]` tags.

**Tech Stack:** Rust, `serde` / `serde_json`, `tokio` (oneshot + mutex), `futures-util` (`FutureExt::catch_unwind`), no new top-level crates.

---

## File structure

**New files:**

- `crates/aaos-backend-linux/src/worker_tools.rs` — `WorkerToolRegistry` construction (allowed tools, built post-sandbox).
- `crates/aaos-tools/src/routing.rs` — `ToolExecutionSurface` enum, `DAEMON_SIDE_TOOLS` constant, `route_for()` helper.

**Modified files:**

- `crates/aaos-backend-linux/src/broker_protocol.rs` — `Request::InvokeTool { tool_name, input, request_id }`; two new worker→broker variants `InvokeToolOk` / `InvokeToolErr`. Kebab-case method names. Roundtrip tests.
- `crates/aaos-backend-linux/src/broker_session.rs` — `pending: Mutex<HashMap<u64, oneshot::Sender<WireResponse>>>`; `invoke_over_worker()`; reader task extended to demux `InvokeToolOk` / `InvokeToolErr` via `pending`.
- `crates/aaos-backend-linux/src/worker.rs` — `agent_loop` dispatches `InvokeTool` via `WorkerToolRegistry` with `catch_unwind` + 60s timeout.
- `crates/aaos-backend-linux/src/lib.rs` — re-export `worker_tools`; pass `WorkerToolRegistry` constructor into `worker::run`.
- `crates/aaos-core/src/audit.rs` — extend `AuditEventKind::ToolInvoked` with `execution_surface: ToolExecutionSurface`.
- `crates/aaos-tools/src/invocation.rs` — accept optional `Arc<dyn WorkerHandle>` trait object; fork execution tail on `ToolExecutionSurface` returned from `route_for`; emit surface in audit event.
- `crates/aaos-tools/src/lib.rs` — re-export `routing`.
- `crates/agentd/src/server.rs` — when the backend is `NamespacedBackend`, construct a `WorkerHandle` adapter over the `BrokerSession`, pass it into `ToolInvocation::new_with_worker_handle`.
- `crates/agentd/src/cli/output.rs` — include `[worker]` / `[daemon]` tag in `ToolInvoked` formatter.
- `docs/architecture.md` + `docs/roadmap.md` — mark Gap 3 shipped with scope notes.
- `docs/reflection/README.md` + new `docs/reflection/YYYY-MM-DD-f-b3-e2e-qa.md` — reflection after fresh-droplet run.

---

## Task 1: `InvokeTool` wire protocol

**Files:**
- Modify: `crates/aaos-backend-linux/src/broker_protocol.rs`

- [ ] **Step 1: Extend `Request` enum + add method constants**

```rust
pub mod method {
    pub const READY: &str = "ready";
    pub const SANDBOXED_READY: &str = "sandboxed-ready";
    pub const POKE: &str = "poke";
    pub const INVOKE_TOOL: &str = "invoke-tool";
    pub const INVOKE_TOOL_OK: &str = "invoke-tool-ok";
    pub const INVOKE_TOOL_ERR: &str = "invoke-tool-err";
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "method", content = "params", rename_all = "kebab-case")]
pub enum Request {
    Ready { pid: u32 },
    SandboxedReady,
    Ping { nonce: u64 },
    Poke { op: PokeOp },
    /// Broker→worker. Carries a tool call to execute in the worker's
    /// confined address space. Response is correlated via `request_id`
    /// which is carried back in the `result` of the matching
    /// `WireResponse`.
    InvokeTool {
        tool_name: String,
        input: serde_json::Value,
        request_id: u64,
    },
}
```

Successful and error replies use the existing `WireResponse` with a small payload contract: `result = { "kind": "invoke-tool-ok", "value": <json> }` on success, `error = WireError { code, message }` on failure. Machine-readable reason codes defined below:

```rust
pub mod invoke_tool_error_code {
    /// Tool not registered in the worker (wrong DAEMON_SIDE_TOOLS config).
    pub const TOOL_NOT_AVAILABLE: i64 = -32100;
    /// Tool panicked.
    pub const TOOL_PANICKED: i64 = -32101;
    /// Tool exceeded 60s timeout.
    pub const TOOL_TIMEOUT: i64 = -32102;
    /// Tool returned a structured denial (e.g. Landlock EACCES).
    pub const TOOL_DENIED: i64 = -32103;
    /// Tool returned a generic runtime error.
    pub const TOOL_RUNTIME: i64 = -32104;
}
```

- [ ] **Step 2: Roundtrip test**

Append to the `#[cfg(test)] mod tests` block:

```rust
#[test]
fn invoke_tool_roundtrip() {
    let req = WireRequest::new(
        99,
        Request::InvokeTool {
            tool_name: "file_write".into(),
            input: serde_json::json!({ "path": "/tmp/x", "content": "hi" }),
            request_id: 99,
        },
    );
    let s = serde_json::to_string(&req).unwrap();
    assert!(s.contains("invoke-tool"));
    let back: WireRequest = serde_json::from_str(&s).unwrap();
    match back.request {
        Request::InvokeTool { tool_name, request_id, .. } => {
            assert_eq!(tool_name, "file_write");
            assert_eq!(request_id, 99);
        }
        other => panic!("wrong variant: {:?}", other),
    }
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p aaos-backend-linux --lib broker_protocol::tests::invoke_tool_roundtrip
```

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/aaos-backend-linux/src/broker_protocol.rs
git commit -m "feat(backend-linux): Request::InvokeTool + error code constants"
```

---

## Task 2: `ToolExecutionSurface` + routing helper

**Files:**
- Create: `crates/aaos-tools/src/routing.rs`
- Modify: `crates/aaos-tools/src/lib.rs`

- [ ] **Step 1: Write the new module**

```rust
//! Per-tool-call execution surface (daemon vs worker) and the static
//! list of tools that must always run daemon-side because the worker
//! sandbox cannot host them (no network, no subprocess execution).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionSurface {
    Daemon,
    Worker,
}

impl ToolExecutionSurface {
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolExecutionSurface::Daemon => "daemon",
            ToolExecutionSurface::Worker => "worker",
        }
    }
}

/// Tools that must always execute daemon-side, regardless of backend.
///
/// - `web_fetch`: seccomp allowlist has no socket/connect syscalls.
/// - `cargo_run`, `git_commit`: seccomp kill-filter denies execve.
pub const DAEMON_SIDE_TOOLS: &[&str] = &["web_fetch", "cargo_run", "git_commit"];

/// Return the intended execution surface for a tool call given the
/// active backend kind (as reported by `AgentLaunchHandle::backend_kind`).
pub fn route_for(tool_name: &str, backend_kind: &str) -> ToolExecutionSurface {
    if backend_kind != "namespaced" || DAEMON_SIDE_TOOLS.contains(&tool_name) {
        ToolExecutionSurface::Daemon
    } else {
        ToolExecutionSurface::Worker
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_process_always_daemon() {
        assert_eq!(route_for("file_write", "in_process"), ToolExecutionSurface::Daemon);
        assert_eq!(route_for("web_fetch", "in_process"), ToolExecutionSurface::Daemon);
    }

    #[test]
    fn namespaced_routes_most_to_worker() {
        assert_eq!(route_for("file_write", "namespaced"), ToolExecutionSurface::Worker);
        assert_eq!(route_for("grep", "namespaced"), ToolExecutionSurface::Worker);
    }

    #[test]
    fn namespaced_keeps_daemon_side_list_on_daemon() {
        for t in DAEMON_SIDE_TOOLS {
            assert_eq!(route_for(t, "namespaced"), ToolExecutionSurface::Daemon, "{t}");
        }
    }
}
```

- [ ] **Step 2: Re-export**

Add to `crates/aaos-tools/src/lib.rs`:

```rust
pub mod routing;
pub use routing::{route_for, ToolExecutionSurface, DAEMON_SIDE_TOOLS};
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p aaos-tools --lib routing::
```

Expected: 3 tests PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/aaos-tools/src/routing.rs crates/aaos-tools/src/lib.rs
git commit -m "feat(tools): ToolExecutionSurface + route_for + DAEMON_SIDE_TOOLS"
```

---

## Task 3: `execution_surface` on `ToolInvoked` audit event

**Files:**
- Modify: `crates/aaos-core/src/audit.rs`

- [ ] **Step 1: Extend `ToolInvoked` variant**

Find the `ToolInvoked` variant of `AuditEventKind`. Add the field:

```rust
ToolInvoked {
    tool: String,
    args_preview: String,
    input_hash: String,
    result: ToolInvocationResult,
    /// Which surface actually ran the tool — daemon-side (today's code
    /// path, and network/subprocess tools) or worker-side (Phase F-b/3).
    execution_surface: aaos_tools::ToolExecutionSurface,
},
```

If `aaos-core` cannot depend on `aaos-tools` (circular), define a local `ToolExecutionSurface` mirror in `aaos-core::audit` and have `aaos-tools::routing::ToolExecutionSurface` be a newtype around it — OR move the enum to `aaos-core`. Check the dep direction with `cargo tree -p aaos-core`; if `aaos-core` is below `aaos-tools`, move the enum down to `aaos-core::tool_surface` and re-export from `aaos-tools`.

- [ ] **Step 2: Fix all existing `ToolInvoked` construction sites**

```bash
grep -rn "ToolInvoked {" crates/ | cut -d: -f1 | sort -u
```

For each site, add `execution_surface: ToolExecutionSurface::Daemon`. There are a handful — all in `crates/aaos-tools/src/invocation.rs` and test sites.

- [ ] **Step 3: Run tests**

```bash
cargo check --workspace --all-features
cargo test --workspace --lib audit
```

Expected: compile clean, audit tests PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/aaos-core/src/audit.rs crates/aaos-tools/src/
git commit -m "feat(core): ToolInvoked gains execution_surface field"
```

---

## Task 4: Broker-side request correlation + `invoke_over_worker`

**Files:**
- Modify: `crates/aaos-backend-linux/src/broker_session.rs`

- [ ] **Step 1: Add pending-request map to `BrokerSession`**

```rust
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use tokio::sync::oneshot;

pub struct BrokerSession {
    // ...existing fields...
    /// Next `request_id` allocated for an InvokeTool call.
    next_request_id: std::sync::atomic::AtomicU64,
    /// Senders keyed by request_id; reader task resolves these when
    /// InvokeToolOk / InvokeToolErr responses arrive.
    pending: StdMutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, InvokeToolError>>>>,
}

#[derive(Debug, thiserror::Error)]
pub enum InvokeToolError {
    #[error("tool {0} not available in worker")]
    NotAvailable(String),
    #[error("tool {0} panicked: {1}")]
    Panicked(String, String),
    #[error("tool {0} exceeded 60s timeout")]
    Timeout(String),
    #[error("tool denied: {0}")]
    Denied(String),
    #[error("tool error: {0}")]
    Runtime(String),
    #[error("worker lost mid-invoke")]
    WorkerLost,
}
```

- [ ] **Step 2: Implement `invoke_over_worker`**

```rust
impl BrokerSession {
    pub async fn invoke_over_worker(
        &self,
        tool_name: &str,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, InvokeToolError> {
        let request_id = self
            .next_request_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().expect("pending mutex poisoned");
            pending.insert(request_id, tx);
        }
        let req = crate::broker_protocol::Request::InvokeTool {
            tool_name: tool_name.to_string(),
            input,
            request_id,
        };
        if let Err(e) = self.send_request(request_id, req).await {
            self.pending
                .lock()
                .expect("pending mutex poisoned")
                .remove(&request_id);
            return Err(InvokeToolError::Runtime(format!("send failed: {e}")));
        }
        rx.await.unwrap_or(Err(InvokeToolError::WorkerLost))
    }
}
```

- [ ] **Step 3: Extend the reader task to demux responses**

In `install_post_handshake_stream`'s reader loop, when a `WireResponse` arrives:
- If there's a pending sender keyed by `response.id`, send the result (success → extract `value`; error → convert code to `InvokeToolError` variant).
- Otherwise fall through to existing routing (Ping / Poke replies).

```rust
if let Some(tx) = session
    .pending
    .lock()
    .expect("pending mutex poisoned")
    .remove(&resp.id)
{
    let payload = if let Some(err) = resp.error {
        use crate::broker_protocol::invoke_tool_error_code::*;
        Err(match err.code {
            TOOL_NOT_AVAILABLE => InvokeToolError::NotAvailable(err.message),
            TOOL_PANICKED => InvokeToolError::Panicked(String::new(), err.message),
            TOOL_TIMEOUT => InvokeToolError::Timeout(err.message),
            TOOL_DENIED => InvokeToolError::Denied(err.message),
            _ => InvokeToolError::Runtime(err.message),
        })
    } else {
        Ok(resp.result.unwrap_or(serde_json::Value::Null))
    };
    let _ = tx.send(payload);
    continue;
}
```

On EOF / read error in the reader loop: drain `pending` and send `Err(InvokeToolError::WorkerLost)` to every pending sender so callers unblock cleanly.

- [ ] **Step 4: Test concurrent invokes in isolation**

Inline unit test using a pair of connected UnixStreams:

```rust
#[tokio::test]
async fn pending_map_demuxes_concurrent_invokes() {
    // Set up a BrokerSession with a fake peer that acks every
    // InvokeTool with echo { "value": input }. Fire 4 invokes
    // concurrently with tokio::join!; assert all 4 resolve with
    // the right values.
    // (Harness is identical to existing sandboxed_ready_fires_once.)
}
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p aaos-backend-linux --lib broker_session::tests
```

Expected: PASS (including the new test).

- [ ] **Step 6: Commit**

```bash
git add crates/aaos-backend-linux/src/broker_session.rs
git commit -m "feat(backend-linux): invoke_over_worker + pending-request demux"
```

---

## Task 5: `WorkerToolRegistry` — whitelist of worker-side tools

**Files:**
- Create: `crates/aaos-backend-linux/src/worker_tools.rs`
- Modify: `crates/aaos-backend-linux/src/lib.rs`

- [ ] **Step 1: Write the module**

```rust
//! Tools that execute inside the confined worker. Constructed after
//! `sandboxed-ready` fires — all registered tools therefore run with
//! Landlock + seccomp already applied.

use std::sync::Arc;

use aaos_tools::registry::ToolRegistry;

/// Explicit whitelist. Fail-closed: if a tool isn't here, `InvokeTool`
/// returns `TOOL_NOT_AVAILABLE` rather than falling back silently.
pub const WORKER_SIDE_TOOLS: &[&str] = &[
    "file_read",
    "file_write",
    "file_edit",
    "file_list",
    "file_read_many",
    "grep",
    "skill_read",
    "memory_query",
    "memory_store",
    "memory_delete",
    "context",
];

/// Build a registry containing only the worker-safe tools.
///
/// Note: `memory_*` tools require a memory store; pass `None` and they
/// return structured errors ("memory not available in worker") until a
/// follow-up sub-project wires a broker-side memory backend.
pub fn build_worker_registry() -> Arc<ToolRegistry> {
    let mut reg = ToolRegistry::new();
    reg.register_by_name(WORKER_SIDE_TOOLS).expect("known-good tool names");
    Arc::new(reg)
}
```

`ToolRegistry::register_by_name` may not exist yet — if not, add it as a thin wrapper that dispatches names to the existing per-tool constructors. Unknown names panic at startup (known-good list).

- [ ] **Step 2: Re-export from `lib.rs`**

```rust
#[cfg(target_os = "linux")]
pub mod worker_tools;
```

- [ ] **Step 3: Unit test — registry contains the whitelist**

```rust
#[test]
fn worker_registry_has_whitelist_only() {
    let reg = build_worker_registry();
    for name in WORKER_SIDE_TOOLS {
        assert!(reg.get(name).is_ok(), "missing {name}");
    }
    assert!(reg.get("web_fetch").is_err(), "web_fetch must not be worker-side");
    assert!(reg.get("cargo_run").is_err(), "cargo_run must not be worker-side");
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test -p aaos-backend-linux --lib worker_tools::
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/aaos-backend-linux/src/worker_tools.rs crates/aaos-backend-linux/src/lib.rs
git commit -m "feat(backend-linux): WorkerToolRegistry whitelist"
```

---

## Task 6: Worker `agent_loop` dispatches `InvokeTool`

**Files:**
- Modify: `crates/aaos-backend-linux/src/worker.rs`

- [ ] **Step 1: Extend `agent_loop` to handle `InvokeTool`**

```rust
use futures_util::future::FutureExt;
use std::panic::AssertUnwindSafe;
use tokio::time::{timeout, Duration};

async fn agent_loop(
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    mut write_half: tokio::net::unix::OwnedWriteHalf,
) -> Result<(), WorkerError> {
    use crate::broker_protocol::{invoke_tool_error_code::*, Request, WireResponse};
    let worker_registry = crate::worker_tools::build_worker_registry();
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(());
        }
        let req: WireRequest = match serde_json::from_str(line.trim_end()) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error=%e, "worker: malformed broker message");
                continue;
            }
        };
        let resp = match req.request {
            Request::Ping { nonce } => {
                WireResponse::success(req.id, serde_json::json!({ "nonce": nonce }))
            }
            Request::Poke { op } => handle_poke_with_id(req.id, op),
            Request::InvokeTool { tool_name, input, request_id: _ } => {
                let tool = match worker_registry.get(&tool_name) {
                    Ok(t) => t,
                    Err(_) => {
                        WireResponse::error(req.id, TOOL_NOT_AVAILABLE, format!("tool {tool_name} not available in worker"));
                        continue;
                    }
                };
                let fut = AssertUnwindSafe(tool.invoke(input)).catch_unwind();
                match timeout(Duration::from_secs(60), fut).await {
                    Ok(Ok(Ok(value))) => WireResponse::success(req.id, value),
                    Ok(Ok(Err(e))) => WireResponse::error(req.id, TOOL_RUNTIME, e.to_string()),
                    Ok(Err(panic_payload)) => {
                        let msg = panic_payload
                            .downcast_ref::<&'static str>().map(|s| s.to_string())
                            .or_else(|| panic_payload.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "<panic payload>".into());
                        WireResponse::error(req.id, TOOL_PANICKED, msg)
                    }
                    Err(_) => WireResponse::error(req.id, TOOL_TIMEOUT, format!("tool {tool_name} exceeded 60s timeout")),
                }
            }
            _ => WireResponse::error(req.id, -32601, "worker: unsupported request"),
        };
        let mut buf = serde_json::to_vec(&resp).map_err(|e| WorkerError::InvalidResponse(e.to_string()))?;
        buf.push(b'\n');
        write_half.write_all(&buf).await?;
        write_half.flush().await?;
    }
}
```

- [ ] **Step 2: Add `futures-util` to `aaos-backend-linux` Cargo.toml if not present**

```toml
futures-util = "0.3"
```

- [ ] **Step 3: Run unit tests**

```bash
cargo test -p aaos-backend-linux --lib
```

Expected: existing tests PASS; no new failures.

- [ ] **Step 4: Commit**

```bash
git add crates/aaos-backend-linux/src/worker.rs crates/aaos-backend-linux/Cargo.toml
git commit -m "feat(backend-linux): worker agent_loop dispatches InvokeTool"
```

---

## Task 7: `WorkerHandle` trait + `ToolInvocation` fork on surface

**Files:**
- Modify: `crates/aaos-tools/src/invocation.rs`

- [ ] **Step 1: Define a thin trait for broker dispatch**

```rust
#[async_trait::async_trait]
pub trait WorkerHandle: Send + Sync {
    /// Return the backend kind for routing (`"namespaced"` or `"in_process"`).
    fn backend_kind(&self) -> &'static str;
    /// Forward a tool invocation across the broker.
    async fn invoke_over_worker(
        &self,
        agent_id: aaos_core::AgentId,
        tool_name: &str,
        input: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, String>;
}
```

- [ ] **Step 2: Extend `ToolInvocation` with an optional handle**

```rust
pub struct ToolInvocation {
    registry: Arc<ToolRegistry>,
    audit_log: Arc<dyn AuditLog>,
    capability_registry: Arc<CapabilityRegistry>,
    repeat_counts: Mutex<HashMap<(AgentId, String, u64), u32>>,
    worker_handle: Option<Arc<dyn WorkerHandle>>,
}

impl ToolInvocation {
    pub fn new(...) -> Self { /* as today; worker_handle = None */ }
    pub fn new_with_worker_handle(
        registry: Arc<ToolRegistry>,
        audit_log: Arc<dyn AuditLog>,
        capability_registry: Arc<CapabilityRegistry>,
        worker_handle: Arc<dyn WorkerHandle>,
    ) -> Self { /* worker_handle = Some(...) */ }
}
```

- [ ] **Step 3: Fork the execution tail**

In the existing `invoke()` method, after the capability check + pre-execution audit, before calling `tool.invoke(input)`:

```rust
let surface = self
    .worker_handle
    .as_ref()
    .map(|h| crate::routing::route_for(tool_name, h.backend_kind()))
    .unwrap_or(crate::routing::ToolExecutionSurface::Daemon);

let result: Result<Value> = match surface {
    ToolExecutionSurface::Worker => {
        let handle = self.worker_handle.as_ref().expect("surface=Worker implies handle");
        handle
            .invoke_over_worker(agent_id, tool_name, input.clone())
            .await
            .map_err(|reason| CoreError::ServiceBackend(reason))
    }
    ToolExecutionSurface::Daemon => {
        let tool = self.registry.get(tool_name)?;
        tool.invoke(input.clone()).await
    }
};
```

Then emit the post-invocation audit event with `execution_surface: surface`.

- [ ] **Step 4: Test — surface routes correctly**

Add an inline mock `WorkerHandle` that records `invoke_over_worker` calls. Unit test:

```rust
#[tokio::test]
async fn namespaced_routes_file_write_to_worker() {
    // Mock handle that reports backend_kind = "namespaced" and
    // echoes any invoke_over_worker call into a Vec.
    // Invoke file_write. Assert the mock received the call and
    // the daemon-side registry was NOT asked for file_write.
}

#[tokio::test]
async fn namespaced_keeps_web_fetch_on_daemon() {
    // Same harness; invoke web_fetch. Assert the mock was NOT called.
}
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p aaos-tools --lib invocation::tests
```

Expected: 2 new tests PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/aaos-tools/src/invocation.rs
git commit -m "feat(tools): ToolInvocation forks on ToolExecutionSurface"
```

---

## Task 8: `agentd` wires `BrokerSession` as the `WorkerHandle`

**Files:**
- Modify: `crates/agentd/src/server.rs`

- [ ] **Step 1: Adapter struct**

```rust
struct BrokerWorkerHandle {
    backend: Arc<aaos_backend_linux::NamespacedBackend>,
}

#[async_trait::async_trait]
impl aaos_tools::WorkerHandle for BrokerWorkerHandle {
    fn backend_kind(&self) -> &'static str { "namespaced" }

    async fn invoke_over_worker(
        &self,
        agent_id: AgentId,
        tool_name: &str,
        input: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, String> {
        let session = self
            .backend
            .session(&agent_id)
            .ok_or_else(|| "no broker session for agent".to_string())?;
        session
            .invoke_over_worker(tool_name, input)
            .await
            .map_err(|e| e.to_string())
    }
}
```

- [ ] **Step 2: Conditional `ToolInvocation` construction**

Where `ToolInvocation::new(...)` is called in `Server::new_*` and `spawn_tool`:

```rust
let tool_invocation = match &*backend {
    b if b.kind() == "namespaced" => Arc::new(ToolInvocation::new_with_worker_handle(
        registry.clone(),
        audit_log.clone(),
        cap_registry.clone(),
        Arc::new(BrokerWorkerHandle { backend: namespaced_backend_arc.clone() }),
    )),
    _ => Arc::new(ToolInvocation::new(registry, audit_log, cap_registry)),
};
```

`AgentBackend` needs a `kind()` method if it doesn't have one — if absent, add it as a thin `fn kind(&self) -> &'static str { "in_process" }` default with an override on `NamespacedBackend`.

- [ ] **Step 3: Run workspace tests**

```bash
cargo test --workspace --all-features
```

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/agentd/src/server.rs crates/aaos-core/src/backend.rs
git commit -m "feat(agentd): wire BrokerSession as WorkerHandle for namespaced backend"
```

---

## Task 9: Integration test — end-to-end `file_write` over broker

**Files:**
- Modify: `crates/aaos-backend-linux/tests/namespaced_backend_e2e.rs` (or the existing integration test file)

- [ ] **Step 1: Extend the existing `namespaced_backend_end_to_end` test**

After handshake + `sandboxed-ready`, drive an `InvokeTool` for `file_write` with a path inside the worker's scratch directory:

```rust
let result = session
    .invoke_over_worker(
        "file_write",
        serde_json::json!({
            "path": scratch_dir.join("hello.txt"),
            "content": "hi from worker\n"
        }),
    )
    .await
    .expect("file_write should succeed inside scratch");
assert!(result.is_object() || result.is_string());
assert!(scratch_dir.join("hello.txt").exists());
```

Skip gate: if `probe_mount_capable()` is false (Azure/restricted), skip this test — same as the existing path.

- [ ] **Step 2: Run the integration test**

```bash
cargo test -p aaos-backend-linux --test namespaced_backend_e2e -- --nocapture
```

Expected: PASS on a host with user namespaces; SKIP on AppArmor-restricted.

- [ ] **Step 3: Commit**

```bash
git add crates/aaos-backend-linux/tests/
git commit -m "test(backend-linux): file_write roundtrips over broker InvokeTool"
```

---

## Task 10: Integration test — Landlock denial surfaces as structured error

**Files:**
- Modify: `crates/aaos-backend-linux/tests/namespaced_backend_e2e.rs`

- [ ] **Step 1: Add a negative-path test**

Invoke `file_read` on `/etc/shadow` (or any path not in the worker's Landlock policy):

```rust
let result = session
    .invoke_over_worker(
        "file_read",
        serde_json::json!({ "path": "/etc/shadow" }),
    )
    .await;
let err = result.expect_err("reading /etc/shadow must fail");
let msg = err.to_string();
assert!(
    msg.contains("denied") || msg.contains("EACCES") || msg.contains("Permission"),
    "error must surface landlock denial, got: {msg}",
);
```

- [ ] **Step 2: Run the test**

```bash
cargo test -p aaos-backend-linux --test namespaced_backend_e2e -- landlock_denies
```

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/aaos-backend-linux/tests/
git commit -m "test(backend-linux): landlock denial surfaces as structured InvokeToolError"
```

---

## Task 11: Operator-visible `[worker]` / `[daemon]` tag

**Files:**
- Modify: `crates/agentd/src/cli/output.rs`

- [ ] **Step 1: Include surface in the `ToolInvoked` formatter**

Find the existing arm that formats `AuditEventKind::ToolInvoked`:

```rust
AuditEventKind::ToolInvoked { tool, execution_surface, .. } => {
    let surface_tag = match execution_surface {
        ToolExecutionSurface::Daemon => "daemon",
        ToolExecutionSurface::Worker => "worker",
    };
    write!(out, "tool: {tool} [{surface_tag}]\n").ok();
}
```

Colorize: worker tag in green (confinement active), daemon tag in dim white (not confined). Use the existing `Color` helper already in the file.

- [ ] **Step 2: Unit test the formatter**

```rust
#[test]
fn tool_invoked_formatter_shows_worker_tag() {
    let ev = AuditEvent::new(AgentId::new(), AuditEventKind::ToolInvoked {
        tool: "file_write".into(),
        args_preview: String::new(),
        input_hash: String::new(),
        result: ToolInvocationResult::Success,
        execution_surface: ToolExecutionSurface::Worker,
    });
    let out = format_event(&ev);
    assert!(out.contains("file_write"));
    assert!(out.contains("[worker]"));
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p agentd --lib cli::output
```

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/agentd/src/cli/output.rs
git commit -m "feat(agentd): operator-visible [worker]/[daemon] surface tag"
```

---

## Task 12: Guardrail — feature-off stays on daemon path

**Files:**
- Modify: `crates/aaos-tools/src/invocation.rs` (tests block)

- [ ] **Step 1: Add a unit test — no `worker_handle`, all tools daemon-side**

```rust
#[tokio::test]
async fn no_worker_handle_stays_on_daemon() {
    // Construct ToolInvocation via ::new (no handle).
    // Invoke file_write. Assert:
    //   - the registry's file_write tool was called (daemon path)
    //   - the audit event has execution_surface = Daemon
}
```

- [ ] **Step 2: Add a unit test — in-process backend with handle returns `"in_process"` stays daemon-side**

```rust
#[tokio::test]
async fn in_process_handle_routes_everything_daemon_side() {
    // Handle returning backend_kind = "in_process".
    // Invoke file_write. Assert it did NOT go through invoke_over_worker.
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p aaos-tools --lib invocation::tests
```

Expected: 2 tests PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/aaos-tools/src/invocation.rs
git commit -m "test(tools): guardrail — feature-off stays on daemon path"
```

---

## Task 13: Docs — architecture + roadmap + scope notes

**Files:**
- Modify: `docs/architecture.md`
- Modify: `docs/roadmap.md`

- [ ] **Step 1: Architecture — mark Gap 3 shipped**

Under the NamespacedBackend section, add:

```markdown
**Gap 3 shipped (Phase F-b/3):** agent tool invocations now execute inside
the worker when `AAOS_DEFAULT_BACKEND=namespaced`. Filesystem and compute
tools (file_*, grep, skill_read, memory_*, context) run under Landlock +
seccomp. Network-touching tools (`web_fetch`) and subprocess-spawning tools
(`cargo_run`, `git_commit`) stay daemon-side — seccomp's kill-filter denies
`execve` and the allowlist omits `socket`/`connect`, so those tools can't
run in the worker today. Operator CLI shows `[worker]` / `[daemon]` tag
per tool line so confinement is visible in-situ.
```

- [ ] **Step 2: Roadmap — Phase F-b Gap 3 done**

In the Phase F-b section of `docs/roadmap.md`, mark Gap 3 ✅ with the scope notes from the architecture doc:

```markdown
- ✅ Gap 3 — worker-side tool confinement. Filesystem + compute tools run
  in the sandbox; network and subprocess tools remain daemon-side (their
  own sub-projects). Shipped YYYY-MM-DD via commits <lo>..<hi>.
```

- [ ] **Step 3: Commit**

```bash
git add docs/architecture.md docs/roadmap.md
git commit -m "docs: Phase F-b sub-project 3 — worker-side tool confinement shipped"
```

---

## Task 14: Fresh-droplet end-to-end verification (Definition of Done)

This task is not a code change. It is the DoD: the 12 or so commits from tasks 1-13 must hold up on a clean Debian 13 host with a real DeepSeek key and the canonical goal.

**Runbook:**

- [ ] **Step 1: Operator creates a fresh droplet**

User provisions `debian-s-4vcpu-8gb-fra1` and pastes the IP.

- [ ] **Step 2: Clone + build with `--features namespaced-agents`**

```bash
ssh root@<ip> 'apt-get update && apt-get install -y build-essential curl pkg-config libssl-dev rsync'
scp -r . root@<ip>:/root/aaos/
ssh root@<ip> '
  cd /root/aaos
  curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain stable
  . ~/.cargo/env
  cargo install cargo-deb
  ./packaging/build-deb.sh --features mcp,namespaced-agents --no-default-features
  apt install -y target/debian/aaos_0.0.0-1_amd64.deb
'
```

- [ ] **Step 3: Install DeepSeek key at `/etc/default/aaos` mode 0600**

Operator pastes the key; `chmod 600 /etc/default/aaos`.

- [ ] **Step 4: Enable namespaced backend + restart daemon**

```bash
ssh root@<ip> "echo AAOS_DEFAULT_BACKEND=namespaced >> /etc/default/aaos && systemctl restart agentd"
```

- [ ] **Step 5: Run the canonical goal**

```bash
ssh root@<ip> 'agentd submit "fetch HN and lobste.rs, compare the top 3 stories on each, write a detailed 800-word comparison" 2>&1 | tee /tmp/submit.log'
```

- [ ] **Step 6: Verify success criteria**

All six must hold:

1. Goal completes end-to-end (workspace files exist; comparison written to `/data/compare.md`).
2. `grep -c 'tool: .* \[worker\]' /tmp/submit.log` > 0 — at least one worker-side tool call.
3. `grep -c 'tool: web_fetch \[daemon\]' /tmp/submit.log` > 0 — web_fetch ran daemon-side.
4. `grep -c 'tool: file_write \[worker\]' /tmp/submit.log` > 0 — file_write ran worker-side.
5. `journalctl -u agentd | grep -iE 'panic|backtrace'` — empty.
6. Landlock-denial probe: try `agentd submit "read /etc/shadow"` with an agent that has the capability. Confirm the error surfaces as `ToolDenied` with "landlock" or "EACCES" in the reason.

- [ ] **Step 7: Shred the key and tear down**

```bash
ssh root@<ip> 'shred -u /etc/default/aaos'
# Operator destroys the droplet on DO dashboard; rotates DeepSeek key.
```

- [ ] **Step 8: Write the reflection entry**

Create `docs/reflection/YYYY-MM-DD-f-b3-e2e-qa.md` using the template in `docs/reflection/README.md`. Fields: integration commits, setup, what worked, what the run exposed, what shipped, cost.

Update `docs/reflection/README.md` index with a one-line summary.

- [ ] **Step 9: Commit the reflection**

```bash
git add docs/reflection/
git commit -m "docs: reflection — F-b/3 worker-side tool confinement e2e on fresh droplet"
```

---

## Commit shape (target: ~12-14 commits)

1. `feat(backend-linux): Request::InvokeTool + error code constants`
2. `feat(tools): ToolExecutionSurface + route_for + DAEMON_SIDE_TOOLS`
3. `feat(core): ToolInvoked gains execution_surface field`
4. `feat(backend-linux): invoke_over_worker + pending-request demux`
5. `feat(backend-linux): WorkerToolRegistry whitelist`
6. `feat(backend-linux): worker agent_loop dispatches InvokeTool`
7. `feat(tools): ToolInvocation forks on ToolExecutionSurface`
8. `feat(agentd): wire BrokerSession as WorkerHandle for namespaced backend`
9. `test(backend-linux): file_write roundtrips over broker InvokeTool`
10. `test(backend-linux): landlock denial surfaces as structured InvokeToolError`
11. `feat(agentd): operator-visible [worker]/[daemon] surface tag`
12. `test(tools): guardrail — feature-off stays on daemon path`
13. `docs: Phase F-b sub-project 3 — worker-side tool confinement shipped`
14. `docs: reflection — F-b/3 worker-side tool confinement e2e on fresh droplet`

---

## Risks and how each is bounded by the plan

- **Request correlation is real plumbing (biggest risk).** Bounded by T4 — concurrent-invoke unit test must pass before any worker-side code lands.
- **Circular dep between `aaos-core::audit` and `aaos-tools::routing`.** Bounded by T3 step 1 — resolve dep direction before touching construction sites.
- **`ToolRegistry::register_by_name` may not exist.** Bounded by T5 — add as thin dispatch wrapper if absent; fail fast on unknown names.
- **Azure AppArmor blocks user namespaces.** Already handled — `probe_mount_capable()` skips on restricted hosts (carried over from sub-project 0).
- **Worker-side tool panics must not kill the worker.** Bounded by T6 — `catch_unwind` wrapper + explicit test (follow-up: can add a `panic_tool` poke op that always panics, verify worker survives).

---

## Non-goals for this plan (say this up front, not later)

- **No network tool confinement.** `web_fetch` stays daemon-side. That's its own sub-project (requires broker-mediated HTTP egress or sandbox relaxation).
- **No subprocess tool confinement.** `cargo_run`, `git_commit` stay daemon-side. Requires scoped `execve` + fs-exec Landlock — its own sub-project.
- **No LLM loop confinement.** Daemon keeps provider clients and API keys. Its own phase, probably G.
- **No per-tool Landlock scoping.** One ruleset per worker; tool calls share it. Dynamic ruleset mutation is research, not v1.
- **No change to the default `--features mcp` (in-process) build.** Path is unchanged; `route_for` returns `Daemon` for every call.
