//! JSON-RPC message types exchanged between the broker (running in
//! `agentd`'s address space) and the worker process (spawned into a
//! Linux namespace).
//!
//! Two-phase handshake (plan v4 round 3):
//!
//! 1. Worker connects, broker validates `SO_PEERCRED`, session is bound
//!    to the accepted socket fd — no per-message re-check.
//! 2. Worker sends [`Request::Ready`] with its pid.
//! 3. Broker replies [`ReadyAck`] with a **policy description** (not a
//!    serialized Landlock ruleset — rulesets are kernel objects, not
//!    JSON-serializable). Worker builds its own ruleset locally.
//! 4. Worker self-applies `PR_SET_NO_NEW_PRIVS` → Landlock → seccomp.
//! 5. Worker sends [`Request::SandboxedReady`]. Only this triggers
//!    `launch()` to return `Ok(handle)`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// JSON-RPC 2.0 envelope identifier.
pub const JSONRPC_VERSION: &str = "2.0";

/// Method names. Kept as `&'static str` constants so typos surface at
/// compile time, not at runtime.
pub mod method {
    pub const READY: &str = "ready";
    pub const SANDBOXED_READY: &str = "sandboxed-ready";
    pub const POKE: &str = "poke";
}

/// Worker → broker requests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "method", content = "params", rename_all = "kebab-case")]
pub enum Request {
    /// First message after connect. Carries the worker's own pid so
    /// the broker can sanity-check against the peer creds (the
    /// authoritative source; this field is advisory / diagnostic).
    Ready { pid: u32 },
    /// Sent after `PR_SET_NO_NEW_PRIVS` + `landlock_restrict_self` +
    /// seccomp have returned successfully. Confinement is in force
    /// before the broker observes this message.
    SandboxedReady,
    /// Debug / integration-test helper: instruct the worker to attempt
    /// a specific operation (e.g. try `execve`) so tests can observe
    /// the sandbox's response. Not part of production traffic.
    Poke { op: PokeOp },
}

/// Integration-test-only operations the worker can be asked to try
/// after confinement is in force. Each should be denied by seccomp or
/// Landlock.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PokeOp {
    /// Attempt `execve("/bin/true")`. Seccomp should kill the worker
    /// with `SIGSYS`.
    TryExecve,
    /// Attempt to open an arbitrary host path. Landlock should deny.
    TryReadHostPath { path: PathBuf },
}

/// A full JSON-RPC request as sent on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireRequest {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

impl WireRequest {
    pub fn new(id: u64, request: Request) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            request,
        }
    }
}

/// Broker → worker success response to a [`Request::Ready`].
///
/// `policy` is a *description* of what the worker should confine itself
/// to. The worker uses this to build its own Landlock ruleset and
/// seccomp filter locally. Landlock rulesets are kernel objects; we
/// cannot hand them across a process boundary as JSON.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadyAck {
    pub policy: PolicyDescription,
}

/// Paths the worker's own Landlock/seccomp setup turns into a ruleset.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyDescription {
    /// Per-agent private tmpfs. The only writable path inside the
    /// worker. Read-write including `MAKE_REG` / `MAKE_DIR`.
    pub scratch: PathBuf,
    /// Read-only paths to the dynamic loader and shared libraries.
    /// Discovered once at [`super::NamespacedBackend`] construction.
    pub shared_libs: Vec<PathBuf>,
    /// The broker socket path inside the child's mount namespace.
    /// Landlock does not restrict `connect(AF_UNIX)` — this is a hint
    /// for documentation / diagnostic logging in the worker.
    pub broker_socket: PathBuf,
}

/// Generic JSON-RPC response envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireResponse {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
}

impl WireResponse {
    pub fn success(id: u64, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: u64, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: None,
            error: Some(WireError {
                code,
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireError {
    pub code: i64,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_roundtrip() {
        let req = WireRequest::new(1, Request::Ready { pid: 4242 });
        let s = serde_json::to_string(&req).unwrap();
        let back: WireRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, 1);
        assert!(matches!(back.request, Request::Ready { pid: 4242 }));
    }

    #[test]
    fn sandboxed_ready_roundtrip() {
        let req = WireRequest::new(2, Request::SandboxedReady);
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("sandboxed-ready"));
        let back: WireRequest = serde_json::from_str(&s).unwrap();
        assert!(matches!(back.request, Request::SandboxedReady));
    }

    #[test]
    fn ready_ack_carries_policy_description_not_ruleset() {
        let ack = ReadyAck {
            policy: PolicyDescription {
                scratch: PathBuf::from("/scratch"),
                shared_libs: vec![
                    PathBuf::from("/lib/x86_64-linux-gnu"),
                    PathBuf::from("/lib64"),
                ],
                broker_socket: PathBuf::from("/var/run/aaos/sessions/x.sock"),
            },
        };
        let s = serde_json::to_string(&ack).unwrap();
        assert!(s.contains("scratch"));
        assert!(s.contains("shared_libs"));
        // Guardrail: no field that looks like a serialized BPF program
        // or Landlock ruleset descriptor. If someone adds one, this
        // test should force them to read the plan's round-3 #4.
        assert!(!s.contains("bpf"));
        assert!(!s.contains("ruleset_fd"));
    }
}
