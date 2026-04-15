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
    use std::path::PathBuf;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    use crate::broker_protocol::{
        ReadyAck, Request, WireRequest, WireResponse,
    };
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
            let agent_id = std::env::var(ENV_AGENT_ID)
                .map_err(|_| WorkerError::MissingEnv(ENV_AGENT_ID))?;
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
        let mut line = serde_json::to_vec(&ready)
            .map_err(|e| WorkerError::InvalidResponse(e.to_string()))?;
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
        let ack_value = resp.result.ok_or_else(|| {
            WorkerError::InvalidResponse("ready-ack missing result".into())
        })?;
        let ack: ReadyAck = serde_json::from_value(ack_value)
            .map_err(|e| WorkerError::InvalidResponse(e.to_string()))?;

        // --- Phase 2: self-apply confinement ---
        apply_confinement(&ack.policy)?;

        // --- Phase 3: sandboxed-ready ---
        let sandboxed =
            WireRequest::new(2, Request::SandboxedReady);
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
            return Err(WorkerError::NoNewPrivsFailed(
                nix::Error::last(),
            ));
        }

        // 2. Landlock (filesystem boundary).
        landlock_compile::restrict_self(policy)?;

        // 3. Seccomp (syscall boundary). Installed as two stacked
        //    filters: allowlist first, kill-on-dangerous second.
        let allow = seccomp_compile::compile_allowlist_filter()?;
        let kill = seccomp_compile::compile_kill_filter()?;
        seccompiler::apply_filter(&allow)
            .map_err(|e| WorkerError::SeccompInstall(e.to_string()))?;
        seccompiler::apply_filter(&kill)
            .map_err(|e| WorkerError::SeccompInstall(e.to_string()))?;

        Ok(())
    }

    /// The minimal agent loop: read poke commands (integration tests)
    /// and otherwise sleep. For production this becomes the message
    /// dispatch loop for brokered tool calls.
    async fn agent_loop(
        mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
        mut write_half: tokio::net::unix::OwnedWriteHalf,
    ) -> Result<(), WorkerError> {
        let mut next_id: u64 = 3;
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
                Request::Poke { op } => handle_poke(op),
                _ => WireResponse::error(
                    req.id,
                    -32601,
                    "worker: unsupported request",
                ),
            };
            let mut buf = serde_json::to_vec(&resp)
                .map_err(|e| WorkerError::InvalidResponse(e.to_string()))?;
            buf.push(b'\n');
            write_half.write_all(&buf).await?;
            write_half.flush().await?;
            next_id += 1;
            let _ = next_id;
        }
    }

    fn handle_poke(op: crate::broker_protocol::PokeOp) -> WireResponse {
        use crate::broker_protocol::PokeOp;
        match op {
            PokeOp::TryExecve => {
                // If seccomp denies execve with KillProcess, this call
                // will not return — the worker dies with SIGSYS and
                // the broker sees the socket close. That's the
                // positive outcome.
                // SAFETY: execve with a NULL-terminated C string path.
                let path = b"/bin/true\0".as_ptr() as *const libc::c_char;
                let argv = [path, std::ptr::null()];
                let envp: [*const libc::c_char; 1] = [std::ptr::null()];
                unsafe {
                    libc::execve(path, argv.as_ptr(), envp.as_ptr());
                }
                // Only reachable if seccomp didn't kill us.
                WireResponse::error(0, -32001, "execve did not SIGSYS — sandbox broken")
            }
            PokeOp::TryReadHostPath { path } => {
                match std::fs::read_to_string(&path) {
                    Ok(_) => WireResponse::error(
                        0,
                        -32002,
                        format!("landlock allowed read of {}", path.display()),
                    ),
                    Err(e) => WireResponse::success(
                        0,
                        serde_json::json!({"denied": e.to_string()}),
                    ),
                }
            }
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
