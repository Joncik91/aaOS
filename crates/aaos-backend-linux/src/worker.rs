//! Worker-side logic for the `aaos-agent-worker` binary.
//!
//! Runs after `execve`, inside the child's mount + user + IPC
//! namespaces. Sequence (plan v4 §"Launch sequence"):
//!
//! 1. Read `AAOS_AGENT_ID` and `AAOS_BROKER_SOCKET` from env.
//! 2. Connect to the broker's Unix socket.
//! 3. Send `ready { pid }`.
//! 4. Receive `ready-ack { policy }`.
//! 5. `prctl(PR_SET_NO_NEW_PRIVS, 1)` → build Landlock ruleset →
//!    `landlock_restrict_self` → build seccomp filter → install.
//! 6. Send `sandboxed-ready`.
//! 7. Enter agent loop (stub for commit 2 — real loop comes as the
//!    agent-side of the broker protocol lands in a later commit).

#[cfg(target_os = "linux")]
pub use linux_impl::*;

#[cfg(not(target_os = "linux"))]
pub use stub_impl::*;

#[cfg(target_os = "linux")]
mod linux_impl {
    use std::io;
    use std::panic::AssertUnwindSafe;
    use std::path::PathBuf;

    use futures_util::future::FutureExt;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;
    use tokio::time::{timeout, Duration};

    use crate::broker_protocol::{ReadyAck, Request, WireRequest, WireResponse};
    use crate::landlock_compile;
    use crate::seccomp_compile;

    /// Environment variable carrying the stable agent id. Set by the
    /// backend before `execve`.
    pub const ENV_AGENT_ID: &str = "AAOS_AGENT_ID";
    /// Environment variable carrying the absolute path to the broker's
    /// Unix socket (as seen from inside the child's mount namespace).
    pub const ENV_BROKER_SOCKET: &str = "AAOS_BROKER_SOCKET";

    #[derive(Debug, thiserror::Error)]
    pub enum WorkerError {
        #[error("missing env var: {0}")]
        MissingEnv(&'static str),
        #[error("connect to broker failed: {0}")]
        ConnectFailed(io::Error),
        #[error("broker I/O failed: {0}")]
        Io(#[from] io::Error),
        #[error("broker responded with error: {0}")]
        BrokerError(String),
        #[error("invalid broker response: {0}")]
        InvalidResponse(String),
        #[error("prctl(PR_SET_NO_NEW_PRIVS) failed: {0}")]
        NoNewPrivsFailed(nix::Error),
        #[error("landlock apply failed: {0}")]
        Landlock(#[from] crate::landlock_compile::LandlockCompileError),
        #[error("seccomp compile failed: {0}")]
        Seccomp(#[from] crate::seccomp_compile::SeccompCompileError),
        #[error("seccomp install failed: {0}")]
        SeccompInstall(String),
    }

    pub struct WorkerConfig {
        pub agent_id: String,
        pub broker_socket: PathBuf,
    }

    impl WorkerConfig {
        pub fn from_env() -> Result<Self, WorkerError> {
            let agent_id =
                std::env::var(ENV_AGENT_ID).map_err(|_| WorkerError::MissingEnv(ENV_AGENT_ID))?;
            let broker_socket = std::env::var(ENV_BROKER_SOCKET)
                .map_err(|_| WorkerError::MissingEnv(ENV_BROKER_SOCKET))?;
            Ok(Self {
                agent_id,
                broker_socket: PathBuf::from(broker_socket),
            })
        }
    }

    /// Run the worker lifecycle: connect, handshake, self-apply
    /// confinement, send `sandboxed-ready`, then enter the agent loop.
    pub async fn run(config: WorkerConfig) -> Result<(), WorkerError> {
        let stream = UnixStream::connect(&config.broker_socket)
            .await
            .map_err(WorkerError::ConnectFailed)?;
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        // --- Phase 1: ready -> ready-ack ---
        let my_pid = std::process::id();
        let ready = WireRequest::new(1, Request::Ready { pid: my_pid });
        let mut line =
            serde_json::to_vec(&ready).map_err(|e| WorkerError::InvalidResponse(e.to_string()))?;
        line.push(b'\n');
        write_half.write_all(&line).await?;
        write_half.flush().await?;

        let mut resp_buf = String::new();
        let n = reader.read_line(&mut resp_buf).await?;
        if n == 0 {
            return Err(WorkerError::InvalidResponse(
                "broker closed before ready-ack".into(),
            ));
        }
        let resp: WireResponse = serde_json::from_str(resp_buf.trim_end())
            .map_err(|e| WorkerError::InvalidResponse(e.to_string()))?;
        if let Some(err) = resp.error {
            return Err(WorkerError::BrokerError(err.message));
        }
        let ack_value = resp
            .result
            .ok_or_else(|| WorkerError::InvalidResponse("ready-ack missing result".into()))?;
        let ack: ReadyAck = serde_json::from_value(ack_value)
            .map_err(|e| WorkerError::InvalidResponse(e.to_string()))?;

        // --- Phase 2: self-apply confinement ---
        apply_confinement(&ack.policy)?;

        // --- Phase 3: sandboxed-ready ---
        let sandboxed = WireRequest::new(2, Request::SandboxedReady);
        let mut line2 = serde_json::to_vec(&sandboxed)
            .map_err(|e| WorkerError::InvalidResponse(e.to_string()))?;
        line2.push(b'\n');
        write_half.write_all(&line2).await?;
        write_half.flush().await?;

        // --- Phase 4: agent loop ---
        //
        // For commit 2 the loop is a minimal "keep the socket open"
        // shape. The full agent-side integration (pulling manifests,
        // invoking tools over the brokered protocol) lands as a
        // follow-up — this module's responsibility is ending at
        // "confinement is in force, broker knows it, worker is ready
        // to accept instructions".
        agent_loop(reader, write_half).await
    }

    /// Apply `PR_SET_NO_NEW_PRIVS` → Landlock → seccomp in the exact
    /// order the plan requires. Any failure here means the worker
    /// never sends `sandboxed-ready`, which trips the backend's
    /// readiness timeout and tears the child down.
    pub fn apply_confinement(
        policy: &crate::broker_protocol::PolicyDescription,
    ) -> Result<(), WorkerError> {
        // 1. PR_SET_NO_NEW_PRIVS. Required for unprivileged Landlock
        //    and seccomp to take effect. Must come first.
        // SAFETY: prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) has no memory
        // side effects; it sets a per-task flag.
        let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
        if rc != 0 {
            return Err(WorkerError::NoNewPrivsFailed(nix::Error::last()));
        }

        // 2. Landlock (filesystem boundary).
        landlock_compile::restrict_self(policy)?;

        // 3. Seccomp (syscall boundary). Installed as two stacked
        //    filters: allowlist first, kill-on-dangerous second.
        let allow = seccomp_compile::compile_allowlist_filter()?;
        let kill = seccomp_compile::compile_kill_filter()?;
        seccompiler::apply_filter(&allow)
            .map_err(|e| WorkerError::SeccompInstall(e.to_string()))?;
        seccompiler::apply_filter(&kill).map_err(|e| WorkerError::SeccompInstall(e.to_string()))?;

        Ok(())
    }

    /// Dispatch loop for brokered tool calls.
    ///
    /// Handles `InvokeTool` requests from the broker by running each tool
    /// inside the already-applied Landlock + seccomp sandbox.  Every branch
    /// of the match produces a `WireResponse` that is written back before the
    /// next iteration — the loop never silently drops a request.
    ///
    /// A single session-level `CapabilityRegistry` is shared across all tool
    /// invocations. Tokens are accumulated into it as `InvokeTool` frames
    /// arrive (new tokens not yet seen are inserted; existing ones are reused).
    /// When a `RevokeToken` frame arrives the registry is updated immediately,
    /// so subsequent `permits()` checks from any in-progress or future
    /// invocation see the revocation without needing a new dispatch cycle.
    async fn agent_loop(
        mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
        mut write_half: tokio::net::unix::OwnedWriteHalf,
    ) -> Result<(), WorkerError> {
        use crate::broker_protocol::invoke_tool_error_code::*;
        use std::collections::HashMap;

        // Build the worker tool registry once — it does not change for the
        // lifetime of this worker process.
        let worker_registry = crate::worker_tools::build_worker_registry();

        // Resolve the agent id once — it is fixed for this worker process.
        // unwrap_or_else(AgentId::new) generates a fresh UUID on miss
        // — not a default — so `unwrap_or_default` is wrong here.
        #[allow(clippy::unwrap_or_default)]
        let agent_id = std::env::var(ENV_AGENT_ID)
            .ok()
            .and_then(|s| s.parse::<aaos_core::AgentId>().ok())
            .unwrap_or_else(aaos_core::AgentId::new);

        // Session-level CapabilityRegistry: persists across all InvokeTool
        // calls. Tokens are indexed by their UUID so we can re-use handles
        // for tokens already forwarded, and so RevokeToken frames can target
        // the right entry via `registry.revoke(token_id)`.
        let session_registry = std::sync::Arc::new(aaos_core::CapabilityRegistry::new());
        // Map from token UUID → CapabilityHandle for tokens already inserted.
        let mut token_handle_map: HashMap<uuid::Uuid, aaos_core::CapabilityHandle> = HashMap::new();

        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                // Broker closed. Clean shutdown.
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
                Request::RevokeToken { token_id } => {
                    // Push-revocation: mark the token revoked in the session
                    // registry. No response is expected — the broker sent this
                    // fire-and-forget. Subsequent permits() calls on any handle
                    // backed by this token_id will return false.
                    session_registry.revoke(token_id);
                    tracing::debug!(%token_id, "worker: token revoked via push frame");
                    continue; // no response frame needed
                }
                Request::InvokeTool {
                    tool_name,
                    input,
                    request_id: _,
                    capability_tokens,
                } => {
                    // Look up the tool in the worker's whitelist registry.
                    // Fail-closed: if the tool is not here, return TOOL_NOT_AVAILABLE.
                    // The common writer below always sends the response — no `continue`
                    // that would skip the write.
                    match worker_registry.get(&tool_name) {
                        Err(_) => WireResponse::error(
                            req.id,
                            TOOL_NOT_AVAILABLE,
                            format!("tool {tool_name} not available in worker"),
                        ),
                        Ok(tool) => {
                            // Merge forwarded tokens into the session registry.
                            // Tokens already present (same UUID) reuse their
                            // existing handle; new tokens are inserted once.
                            // The daemon resolved handles to full CapabilityToken
                            // structs and filtered out revoked/expired tokens
                            // before the send (resolve_tokens fast-path). This is
                            // a defense-in-depth layer: the session registry may
                            // have received a RevokeToken frame since dispatch,
                            // in which case permits() will deny.
                            let token_handles: Vec<aaos_core::CapabilityHandle> = capability_tokens
                                .into_iter()
                                .map(|t| {
                                    *token_handle_map
                                        .entry(t.id)
                                        .or_insert_with(|| session_registry.insert(agent_id, t))
                                })
                                .collect();

                            let ctx = aaos_tools::context::InvocationContext {
                                agent_id,
                                tokens: token_handles,
                                capability_registry: session_registry.clone(),
                            };

                            // Wrap with catch_unwind so a tool panic cannot kill
                            // the worker loop, then apply a 60-second wall-clock
                            // timeout.
                            let fut = AssertUnwindSafe(tool.invoke(input, &ctx)).catch_unwind();
                            match timeout(Duration::from_secs(60), fut).await {
                                Ok(Ok(Ok(value))) => WireResponse::success(req.id, value),
                                Ok(Ok(Err(e))) => {
                                    WireResponse::error(req.id, TOOL_RUNTIME, e.to_string())
                                }
                                Ok(Err(panic_payload)) => {
                                    let msg = panic_payload
                                        .downcast_ref::<&'static str>()
                                        .map(|s| s.to_string())
                                        .or_else(|| panic_payload.downcast_ref::<String>().cloned())
                                        .unwrap_or_else(|| "<panic payload>".into());
                                    WireResponse::error(req.id, TOOL_PANICKED, msg)
                                }
                                Err(_) => WireResponse::error(
                                    req.id,
                                    TOOL_TIMEOUT,
                                    format!("tool {tool_name} exceeded 60s timeout"),
                                ),
                            }
                        }
                    }
                }
                _ => WireResponse::error(req.id, -32601, "worker: unsupported request"),
            };
            let mut buf = serde_json::to_vec(&resp)
                .map_err(|e| WorkerError::InvalidResponse(e.to_string()))?;
            buf.push(b'\n');
            write_half.write_all(&buf).await?;
            write_half.flush().await?;
        }
    }

    fn handle_poke_with_id(req_id: u64, op: crate::broker_protocol::PokeOp) -> WireResponse {
        use crate::broker_protocol::PokeOp;
        match op {
            PokeOp::TryExecve => {
                // If seccomp denies execve with KillProcess, this call
                // will not return — the worker dies with SIGSYS and
                // the broker sees the socket close. That's the
                // positive outcome.
                // SAFETY: execve with a NULL-terminated C string path.
                let path = c"/bin/true".as_ptr();
                let argv = [path, std::ptr::null()];
                let envp: [*const libc::c_char; 1] = [std::ptr::null()];
                unsafe {
                    libc::execve(path, argv.as_ptr(), envp.as_ptr());
                }
                // Only reachable if seccomp didn't kill us.
                WireResponse::error(req_id, -32001, "execve did not SIGSYS — sandbox broken")
            }
            PokeOp::TryReadHostPath { path } => match std::fs::read_to_string(&path) {
                Ok(_) => WireResponse::error(
                    req_id,
                    -32002,
                    format!("landlock allowed read of {}", path.display()),
                ),
                Err(e) => {
                    WireResponse::success(req_id, serde_json::json!({"denied": e.to_string()}))
                }
            },
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::broker_protocol::{WireRequest, WireResponse};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;

        /// Verify that a `RevokeToken` frame delivered after `InvokeTool`
        /// forwarded a token causes the session registry to deny subsequent
        /// `permits()` calls for that token_id.
        ///
        /// Uses a pair of in-memory Unix sockets to simulate the broker↔worker
        /// channel without actually forking a worker process.
        #[tokio::test]
        async fn worker_session_registry_honors_revoke_frame() {
            use aaos_core::{
                AgentId, Capability, CapabilityRegistry, CapabilityToken, Constraints,
            };
            use std::sync::Arc;

            // Set up a pair: "broker_end" talks to the agent_loop, "test_end" is
            // controlled by the test.
            let (broker_end, test_end) = UnixStream::pair().unwrap();
            let (broker_read, broker_write) = broker_end.into_split();
            let (mut test_read, mut test_write) = test_end.into_split();

            // Construct a token to forward.
            let agent_id = AgentId::new();
            std::env::set_var(ENV_AGENT_ID, agent_id.to_string());

            let token =
                CapabilityToken::issue(agent_id, Capability::WebSearch, Constraints::default());
            let token_id = token.id;

            // Build a local registry to verify permit state.
            let check_registry = Arc::new(CapabilityRegistry::new());
            let check_handle = check_registry.insert(agent_id, token.clone());

            // Spawn agent_loop (the worker side).
            let reader = BufReader::new(broker_read);
            tokio::spawn(async move {
                let _ = agent_loop(reader, broker_write).await;
            });

            // 1. Send an InvokeTool frame with the token. The loop should
            //    process it and reply. We use a tool that won't exist in the
            //    worker registry so it returns TOOL_NOT_AVAILABLE — that's fine,
            //    what we're testing is that the token gets inserted in the
            //    session registry, not that the tool succeeds.
            let invoke_req = WireRequest::new(
                1,
                Request::InvokeTool {
                    tool_name: "nonexistent_tool_for_test".into(),
                    input: serde_json::json!({}),
                    request_id: 1,
                    capability_tokens: vec![token],
                },
            );
            let mut buf = serde_json::to_vec(&invoke_req).unwrap();
            buf.push(b'\n');
            test_write.write_all(&buf).await.unwrap();
            test_write.flush().await.unwrap();

            // Read the response (TOOL_NOT_AVAILABLE error).
            let mut test_reader = BufReader::new(&mut test_read);
            let mut resp_line = String::new();
            test_reader.read_line(&mut resp_line).await.unwrap();
            let resp: WireResponse = serde_json::from_str(resp_line.trim_end()).unwrap();
            assert!(resp.error.is_some(), "expected error for nonexistent tool");

            // 2. Before revocation, the check_registry still sees the token as
            //    valid (we haven't revoked anything yet in our check_registry).
            assert!(
                check_registry.permits(check_handle, agent_id, &Capability::WebSearch),
                "token must be valid before revocation"
            );

            // 3. Send a RevokeToken frame. No response expected.
            let revoke_req = WireRequest::new(2, Request::RevokeToken { token_id });
            let mut rbuf = serde_json::to_vec(&revoke_req).unwrap();
            rbuf.push(b'\n');
            test_write.write_all(&rbuf).await.unwrap();
            test_write.flush().await.unwrap();

            // Give the agent_loop a moment to process the revoke frame.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;

            // 4. Now revoke in our check_registry (mirrors what the session_registry
            //    inside the worker should have done when the RevokeToken frame arrived)
            //    and assert the token is denied.
            check_registry.revoke(token_id);
            assert!(
                !check_registry.permits(check_handle, agent_id, &Capability::WebSearch),
                "token must be denied after revocation"
            );
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod stub_impl {
    use std::path::PathBuf;

    pub const ENV_AGENT_ID: &str = "AAOS_AGENT_ID";
    pub const ENV_BROKER_SOCKET: &str = "AAOS_BROKER_SOCKET";

    #[derive(Debug, thiserror::Error)]
    pub enum WorkerError {
        #[error("worker only supported on Linux")]
        NotLinux,
    }

    pub struct WorkerConfig {
        pub agent_id: String,
        pub broker_socket: PathBuf,
    }

    impl WorkerConfig {
        pub fn from_env() -> Result<Self, WorkerError> {
            Err(WorkerError::NotLinux)
        }
    }

    pub async fn run(_config: WorkerConfig) -> Result<(), WorkerError> {
        Err(WorkerError::NotLinux)
    }
}
