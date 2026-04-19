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
// CapabilityToken is serializable and forwarded to workers so they can
// rebuild a per-call capability registry for tool internal re-checks.

/// JSON-RPC 2.0 envelope identifier.
pub const JSONRPC_VERSION: &str = "2.0";

/// Method names. Kept as `&'static str` constants so typos surface at
/// compile time, not at runtime.
pub mod method {
    pub const READY: &str = "ready";
    pub const SANDBOXED_READY: &str = "sandboxed-ready";
    pub const POKE: &str = "poke";
    pub const INVOKE_TOOL: &str = "invoke-tool";
    pub const INVOKE_TOOL_OK: &str = "invoke-tool-ok";
    pub const INVOKE_TOOL_ERR: &str = "invoke-tool-err";
}

/// Error codes for `InvokeTool` failures. Extend JSON-RPC 2.0's reserved range
/// (-32768..-32000) with AgentSkills-specific codes.
pub mod invoke_tool_error_code {
    /// Tool is not available in the worker's capability set.
    pub const TOOL_NOT_AVAILABLE: i64 = -32100;
    /// Tool executed but panicked or returned an abort-level error.
    pub const TOOL_PANICKED: i64 = -32101;
    /// Tool did not complete before the worker's timeout.
    pub const TOOL_TIMEOUT: i64 = -32102;
    /// Landlock, seccomp, or capability check denied the tool's access.
    pub const TOOL_DENIED: i64 = -32103;
    /// Tool encountered an unrecoverable error (e.g. OOM, internal worker corruption).
    pub const TOOL_RUNTIME: i64 = -32104;
}

/// Requests that cross the broker↔worker channel.
///
/// `Ready` and `SandboxedReady` are worker→broker announcements sent during
/// the handshake (and handled inline in `run_handshake`). `Ping` and `Poke`
/// are broker→worker messages sent over the persistent post-handshake
/// stream; the worker's `agent_loop` dispatches them and replies.
// Note: PartialEq/Eq are NOT derived here because the `capability_tokens`
// field (Vec<CapabilityToken>) cannot implement Eq — CapabilityToken
// contains Capability::Custom { params: serde_json::Value } which has no Eq.
// Tests that need to inspect Request values use `matches!` instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Broker→worker liveness probe. Worker echoes the nonce back in
    /// the response's `result.nonce` field. First real round-trip on
    /// the persistent post-handshake stream; no sandbox-escape
    /// semantics, purely a transport proof of life.
    Ping { nonce: u64 },
    /// Debug / integration-test helper: instruct the worker to attempt
    /// a specific operation (e.g. try `execve`) so tests can observe
    /// the sandbox's response. Not part of production traffic.
    Poke { op: PokeOp },
    /// Broker→worker. Carries a tool call to execute in the worker's
    /// confined address space. Response is correlated via `request_id`
    /// which the broker matches to a `pending` oneshot sender.
    ///
    /// `capability_tokens` carries the full `CapabilityToken` structs
    /// (serialized from the daemon's `CapabilityRegistry`) so the worker
    /// can rebuild a minimal per-call registry and satisfy the tool's own
    /// internal `ctx.capability_registry.permits()` check. Defaults to
    /// empty so older workers that do not know about this field continue
    /// to deserialize correctly.
    InvokeTool {
        tool_name: String,
        input: serde_json::Value,
        request_id: u64,
        #[serde(default)]
        capability_tokens: Vec<aaos_core::CapabilityToken>,
    },
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
    /// Optional workspace path bind-mounted from the host into the
    /// worker at the same absolute path (preserved, so tool calls
    /// using absolute workspace paths resolve identically daemon-side
    /// vs worker-side). None = no workspace visibility (pure compute
    /// under /scratch only). Added to the Landlock rw allow-list so
    /// file_read/file_write to these paths can succeed under capability
    /// grants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,

    /// Additional read+write allowlist paths bind-mounted at the same
    /// absolute path inside the worker. Populated from the role's
    /// FileRead / FileWrite capability path_globs so the writer can
    /// reach its declared output directory (e.g. `/data/`) without
    /// having to stuff everything under `workspace`. Each entry gets
    /// a Landlock PathBeneath rule and a bind-mount from the host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_writable_roots: Vec<PathBuf>,
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
                workspace: None,
                extra_writable_roots: vec![],
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

    #[test]
    fn invoke_tool_roundtrip() {
        let req = WireRequest::new(
            99,
            Request::InvokeTool {
                tool_name: "file_write".into(),
                input: serde_json::json!({ "path": "/tmp/x", "content": "hi" }),
                request_id: 99,
                capability_tokens: vec![],
            },
        );
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("invoke-tool"));
        let back: WireRequest = serde_json::from_str(&s).unwrap();
        match back.request {
            Request::InvokeTool { tool_name, request_id, capability_tokens, .. } => {
                assert_eq!(tool_name, "file_write");
                assert_eq!(request_id, 99);
                assert!(capability_tokens.is_empty());
            }
            other => panic!("wrong variant: {:?}", other),
        }
    }

    /// Verify that `InvokeTool` with forwarded `CapabilityToken` structs
    /// roundtrips correctly over JSON — the full token (including capability
    /// type, constraints, timestamps) must survive a serialize/deserialize
    /// cycle unchanged.
    #[test]
    fn invoke_tool_with_tokens_roundtrips() {
        use aaos_core::{AgentId, Capability, CapabilityToken, Constraints};

        let agent_id = AgentId::new();
        let token = CapabilityToken::issue(
            agent_id,
            Capability::FileRead {
                path_glob: "/lib/x86_64-linux-gnu/*".into(),
            },
            Constraints::default(),
        );
        let token_id = token.id;

        let req = WireRequest::new(
            42,
            Request::InvokeTool {
                tool_name: "file_read".into(),
                input: serde_json::json!({ "path": "/lib/x86_64-linux-gnu/libc.so.6" }),
                request_id: 42,
                capability_tokens: vec![token],
            },
        );
        let s = serde_json::to_string(&req).unwrap();
        let back: WireRequest = serde_json::from_str(&s).unwrap();
        match back.request {
            Request::InvokeTool { capability_tokens, .. } => {
                assert_eq!(capability_tokens.len(), 1);
                let t = &capability_tokens[0];
                assert_eq!(t.id, token_id);
                assert_eq!(t.agent_id, agent_id);
                assert!(matches!(
                    &t.capability,
                    Capability::FileRead { path_glob } if path_glob == "/lib/x86_64-linux-gnu/*"
                ));
            }
            other => panic!("wrong variant: {:?}", other),
        }
    }

    /// Verify backward-compat: an `InvokeTool` message without
    /// `capability_tokens` (as an older broker would send) deserializes
    /// correctly with the field defaulting to empty.
    #[test]
    fn invoke_tool_missing_tokens_defaults_to_empty() {
        // Craft a JSON string that looks like what an older broker would
        // send — no `capability_tokens` field at all.
        let old_wire = r#"{"jsonrpc":"2.0","id":7,"method":"invoke-tool","params":{"tool_name":"echo","input":{},"request_id":7}}"#;
        let back: WireRequest = serde_json::from_str(old_wire).unwrap();
        match back.request {
            Request::InvokeTool { capability_tokens, .. } => {
                assert!(
                    capability_tokens.is_empty(),
                    "missing capability_tokens must default to empty vec"
                );
            }
            other => panic!("wrong variant: {:?}", other),
        }
    }
}
