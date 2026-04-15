//! `aaos-backend-linux` — second `AgentBackend` implementation (after
//! `InProcessBackend` in `aaos-runtime`).
//!
//! Launches each agent in its own Linux user+mount+IPC namespace,
//! self-applies Landlock (filesystem) and seccomp-BPF (syscalls) inside
//! the worker, and routes all tool invocations through a broker session
//! over a per-agent Unix socket authenticated by `SO_PEERCRED`.
//!
//! See `plans/2026-04-15-namespaced-backend-v4.md` for the design
//! rationale and three-round Copilot review history.
//!
//! ## Non-negotiable constraints (plan rounds 2–3)
//!
//! - NO `CLONE_NEWPID`. Avoids PID-1-in-namespace semantics.
//! - NO `CLONE_NEWNET`. All network brokered.
//! - Landlock + seccomp are self-applied inside the worker AFTER the
//!   child starts running and AFTER `PR_SET_NO_NEW_PRIVS`.
//! - `launch()` returns `Ok` only after the worker has sent
//!   `sandboxed-ready` — not after the earlier `ready`.
//! - Broker sessions bound to the accepted Unix socket via
//!   `SO_PEERCRED`; no bearer secrets.
//! - No blanket workspace bind-mount. Scratch tmpfs is the only
//!   writable path; user-data file ops go through the broker.
//! - uid/gid mapping failure is a hard error; no silent fallback.
//! - Fail closed on missing Landlock.

pub mod broker_protocol;
pub mod broker_session;
pub mod landlock_compile;
pub mod seccomp_compile;
pub mod worker;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use aaos_core::{AgentBackend, AgentLaunchHandle, AgentLaunchSpec, BackendHealth, CoreError, Result};

use crate::broker_session::SessionMap;

/// Configuration for constructing a [`NamespacedBackend`].
#[derive(Debug, Clone)]
pub struct NamespacedBackendConfig {
    pub session_dir: PathBuf,
    pub worker_binary: PathBuf,
    pub shared_lib_paths: Vec<PathBuf>,
    pub ready_timeout_ms: u64,
}

impl Default for NamespacedBackendConfig {
    fn default() -> Self {
        let ready_timeout_ms = std::env::var("AAOS_NAMESPACED_READY_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5000);
        Self {
            session_dir: PathBuf::from("/var/run/aaos/sessions"),
            worker_binary: PathBuf::from("/usr/bin/aaos-agent-worker"),
            shared_lib_paths: vec![
                PathBuf::from("/lib/x86_64-linux-gnu"),
                PathBuf::from("/lib64"),
                PathBuf::from("/usr/lib/x86_64-linux-gnu"),
            ],
            ready_timeout_ms,
        }
    }
}

/// Error taxonomy. Mapped to [`CoreError::Ipc`] at the trait boundary.
#[derive(Debug, Error)]
pub enum BackendError {
    #[error("Landlock not available on this kernel (need ABI v1+)")]
    LandlockUnsupported,

    #[error("clone() failed: {0}")]
    CloneFailed(String),

    #[error("uid/gid mapping failed: {0}")]
    UidMapFailed(std::io::Error),

    #[error("mount setup failed: {0}")]
    MountFailed(String),

    #[error("worker binary launch failed: {0}")]
    LaunchFailed(String),

    #[error("worker did not reach sandboxed-ready within {timeout_ms}ms")]
    ReadyTimeout { timeout_ms: u64 },

    #[error("peer-creds mismatch: expected pid {expected}, got {actual}")]
    PeerCredsMismatch { expected: u32, actual: u32 },

    #[error("seccomp compile failed: {0}")]
    SeccompCompileFailed(String),

    #[error("broker I/O failed: {0}")]
    BrokerIoFailed(std::io::Error),

    #[error("session directory setup failed: {0}")]
    SessionDirFailed(std::io::Error),

    #[error("backend only supported on Linux")]
    NotLinux,
}

impl From<BackendError> for CoreError {
    fn from(e: BackendError) -> Self {
        CoreError::Ipc(e.to_string())
    }
}

/// Linux-namespaced agent backend.
pub struct NamespacedBackend {
    config: NamespacedBackendConfig,
    sessions: Arc<SessionMap>,
}

/// State kept in [`AgentLaunchHandle`] for agents launched by this backend.
pub struct NamespacedState {
    pub pid: u32,
    pub session_socket_path: PathBuf,
}

impl NamespacedBackend {
    pub fn new(config: NamespacedBackendConfig) -> std::result::Result<Self, BackendError> {
        if !landlock_compile::is_supported() {
            return Err(BackendError::LandlockUnsupported);
        }
        Ok(Self {
            config,
            sessions: Arc::new(SessionMap::new()),
        })
    }

    pub fn shared_lib_paths(&self) -> &[PathBuf] {
        &self.config.shared_lib_paths
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

#[cfg(target_os = "linux")]
mod launch_impl {
    use super::*;
    use crate::broker_protocol::{PolicyDescription, ReadyAck, Request, WireRequest, WireResponse};
    use crate::broker_session::{peer_creds_from_stream, BrokerSession};

    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    pub(super) fn ensure_session_dir(dir: &std::path::Path) -> std::result::Result<(), BackendError> {
        if !dir.exists() {
            std::fs::create_dir_all(dir).map_err(BackendError::SessionDirFailed)?;
        }
        let mut perms = std::fs::metadata(dir)
            .map_err(BackendError::SessionDirFailed)?
            .permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(dir, perms).map_err(BackendError::SessionDirFailed)?;
        Ok(())
    }

    pub(super) fn bind_session_socket(
        session_dir: &std::path::Path,
        agent_id: &aaos_core::AgentId,
    ) -> std::result::Result<(UnixListener, PathBuf), BackendError> {
        let socket_path = session_dir.join(format!("{}.sock", agent_id));
        let _ = std::fs::remove_file(&socket_path);
        let listener =
            UnixListener::bind(&socket_path).map_err(BackendError::BrokerIoFailed)?;
        let mut perms = std::fs::metadata(&socket_path)
            .map_err(BackendError::BrokerIoFailed)?
            .permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&socket_path, perms).map_err(BackendError::BrokerIoFailed)?;
        Ok((listener, socket_path))
    }

    pub(super) async fn run_handshake(
        stream: tokio::net::UnixStream,
        session: Arc<BrokerSession>,
    ) -> std::result::Result<(), BackendError> {
        let peer = peer_creds_from_stream(&stream).map_err(BackendError::BrokerIoFailed)?;
        if let Err(e) = session.peer_creds_match(peer) {
            return Err(match e {
                crate::broker_session::PeerCredsError::PidMismatch { expected, actual } => {
                    BackendError::PeerCredsMismatch { expected, actual }
                }
                other => BackendError::BrokerIoFailed(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    other.to_string(),
                )),
            });
        }

        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(BackendError::BrokerIoFailed)?;
        if n == 0 {
            return Err(BackendError::BrokerIoFailed(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "worker closed before ready",
            )));
        }
        let ready: WireRequest = serde_json::from_str(line.trim_end()).map_err(|e| {
            BackendError::BrokerIoFailed(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                e.to_string(),
            ))
        })?;
        if !matches!(ready.request, Request::Ready { .. }) {
            return Err(BackendError::BrokerIoFailed(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "first message was not `ready`",
            )));
        }

        let ack = WireResponse::success(
            ready.id,
            serde_json::to_value(ReadyAck {
                policy: session.policy.clone(),
            })
            .map_err(|e| {
                BackendError::BrokerIoFailed(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.to_string(),
                ))
            })?,
        );
        let mut out = serde_json::to_vec(&ack).map_err(|e| {
            BackendError::BrokerIoFailed(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                e.to_string(),
            ))
        })?;
        out.push(b'\n');
        write_half
            .write_all(&out)
            .await
            .map_err(BackendError::BrokerIoFailed)?;
        write_half
            .flush()
            .await
            .map_err(BackendError::BrokerIoFailed)?;

        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(BackendError::BrokerIoFailed)?;
        if n == 0 {
            return Err(BackendError::BrokerIoFailed(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "worker died before sandboxed-ready",
            )));
        }
        let sandboxed: WireRequest = serde_json::from_str(line.trim_end()).map_err(|e| {
            BackendError::BrokerIoFailed(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                e.to_string(),
            ))
        })?;
        if !matches!(sandboxed.request, Request::SandboxedReady) {
            return Err(BackendError::BrokerIoFailed(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "expected sandboxed-ready",
            )));
        }
        session.fire_sandboxed_ready().await;
        Ok(())
    }

    /// The clone + uid/gid-map + mount + pivot_root + launch path.
    ///
    /// Implementing this safely requires (a) a statically-linked
    /// worker binary or a fully populated rootfs with /proc, (b) root
    /// or user-ns creation privilege on the host, and (c) careful
    /// handling of the uid_map write race window. The plan permits
    /// deferring this to manual verification on a root-privileged
    /// Linux host; CI exercises only the compile-side units.
    pub(super) fn clone_and_launch_worker(
        _config: &NamespacedBackendConfig,
        _policy: &PolicyDescription,
        _env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    ) -> std::result::Result<u32, BackendError> {
        Err(BackendError::CloneFailed(
            "clone+pivot_root path deferred to manual verification per plan v4; \
             requires root on Linux >= 5.13. Use InProcessBackend for CI. See \
             plans/2026-04-15-namespaced-backend-v4.md."
                .into(),
        ))
    }

    pub(super) async fn launch_impl(
        backend: &NamespacedBackend,
        spec: AgentLaunchSpec,
    ) -> Result<AgentLaunchHandle> {
        ensure_session_dir(&backend.config.session_dir)?;

        let (listener, socket_path) =
            bind_session_socket(&backend.config.session_dir, &spec.agent_id)?;

        let policy = PolicyDescription {
            scratch: PathBuf::from("/scratch"),
            shared_libs: backend.config.shared_lib_paths.clone(),
            broker_socket: socket_path.clone(),
        };

        let my_uid = nix::unistd::getuid().as_raw();
        let my_gid = nix::unistd::getgid().as_raw();

        let env = vec![
            (
                std::ffi::OsString::from(crate::worker::ENV_AGENT_ID),
                std::ffi::OsString::from(spec.agent_id.to_string()),
            ),
            (
                std::ffi::OsString::from(crate::worker::ENV_BROKER_SOCKET),
                std::ffi::OsString::from(socket_path.as_os_str()),
            ),
        ];

        let child_pid = clone_and_launch_worker(&backend.config, &policy, env)
            .map_err(|e| {
                let _ = std::fs::remove_file(&socket_path);
                CoreError::from(e)
            })?;

        let (session, sandboxed_rx) = BrokerSession::new(
            spec.agent_id,
            child_pid,
            my_uid,
            my_gid,
            policy,
            socket_path.clone(),
        );
        let session_arc = Arc::new(session);
        backend.sessions.insert(session_arc.clone());

        let timeout_ms = backend.config.ready_timeout_ms;
        let handshake_session = session_arc.clone();
        let handshake = async move {
            let (stream, _addr) = listener
                .accept()
                .await
                .map_err(BackendError::BrokerIoFailed)?;
            run_handshake(stream, handshake_session).await?;
            sandboxed_rx.await.map_err(|_| BackendError::ReadyTimeout {
                timeout_ms,
            })?;
            Ok::<_, BackendError>(())
        };

        match tokio::time::timeout(Duration::from_millis(timeout_ms), handshake).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = std::fs::remove_file(&socket_path);
                backend.sessions.remove(&spec.agent_id);
                return Err(e.into());
            }
            Err(_) => {
                let _ = std::fs::remove_file(&socket_path);
                backend.sessions.remove(&spec.agent_id);
                return Err(BackendError::ReadyTimeout { timeout_ms }.into());
            }
        }

        Ok(AgentLaunchHandle::new(
            spec.agent_id,
            "namespaced",
            NamespacedState {
                pid: child_pid,
                session_socket_path: socket_path,
            },
        ))
    }
}

#[async_trait]
impl AgentBackend for NamespacedBackend {
    async fn launch(&self, spec: AgentLaunchSpec) -> Result<AgentLaunchHandle> {
        #[cfg(target_os = "linux")]
        {
            launch_impl::launch_impl(self, spec).await
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = spec;
            Err(BackendError::NotLinux.into())
        }
    }

    async fn stop(&self, handle: &AgentLaunchHandle) -> Result<()> {
        if let Some(session) = self.sessions.remove(&handle.agent_id) {
            let _ = std::fs::remove_file(&session.socket_path);
            #[cfg(target_os = "linux")]
            {
                if let Some(state) = handle.state::<NamespacedState>() {
                    let pid = nix::unistd::Pid::from_raw(state.pid as i32);
                    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
                }
            }
        }
        Ok(())
    }

    async fn health(&self, handle: &AgentLaunchHandle) -> BackendHealth {
        match self.sessions.get(&handle.agent_id) {
            Some(_) => BackendHealth::Healthy,
            None => BackendHealth::Unknown("no session for this handle".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_has_no_user_data_paths() {
        let cfg = NamespacedBackendConfig::default();
        for p in &cfg.shared_lib_paths {
            let s = p.to_string_lossy();
            for forbidden in &["/data", "/home", "/output", "/src"] {
                assert!(
                    !s.contains(forbidden),
                    "shared_lib_paths must not contain user-data prefix {forbidden}: {s}"
                );
            }
        }
        assert_eq!(cfg.ready_timeout_ms, 5000);
    }

    #[test]
    fn backend_new_fail_closed_on_missing_landlock() {
        let result = NamespacedBackend::new(NamespacedBackendConfig::default());
        if landlock_compile::is_supported() {
            assert!(result.is_ok(), "Landlock supported but new() failed");
        } else {
            assert!(
                matches!(result, Err(BackendError::LandlockUnsupported)),
                "new() must fail closed when Landlock unavailable"
            );
        }
    }

    #[test]
    fn namespaced_state_roundtrips_through_opaque_handle() {
        use aaos_core::AgentId;
        let id = AgentId::new();
        let state = NamespacedState {
            pid: 4242,
            session_socket_path: PathBuf::from("/var/run/aaos/sessions/a.sock"),
        };
        let handle = AgentLaunchHandle::new(id, "namespaced", state);
        let got = handle
            .state::<NamespacedState>()
            .expect("state<NamespacedState>");
        assert_eq!(got.pid, 4242);
        assert_eq!(handle.backend_kind, "namespaced");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn session_dir_ensure_creates_with_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        launch_impl::ensure_session_dir(&dir).expect("ensure_session_dir");
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn session_socket_binds_with_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let agent_id = aaos_core::AgentId::new();
        let (_listener, socket_path) =
            launch_impl::bind_session_socket(&dir, &agent_id).expect("bind");
        let mode = std::fs::metadata(&socket_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert!(socket_path.exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn launch_surfaces_structured_error_until_clone_path_ships() {
        // Commit 2 deliberately defers the clone+pivot_root path to
        // manual verification on a root-privileged Linux host. The
        // `launch()` call returns a structured error in CI. This test
        // pins that contract so any future change that flips it to
        // silent success — which would produce an unsandboxed process
        // under the "namespaced" label — is caught.
        if !landlock_compile::is_supported() {
            return;
        }
        use aaos_core::{AgentId, AgentManifest};

        // Use a tempdir so we don't need /var/run/aaos/sessions.
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = NamespacedBackendConfig::default();
        cfg.session_dir = tmp.path().join("sessions");
        let backend = NamespacedBackend::new(cfg).expect("Landlock supported");
        let spec = AgentLaunchSpec {
            agent_id: AgentId::new(),
            manifest: AgentManifest::from_yaml(
                r#"
name: t
model: claude-haiku-4-5-20251001
system_prompt: "x"
"#,
            )
            .unwrap(),
            capability_handles: vec![],
            workspace_path: PathBuf::new(),
            budget_config: None,
        };
        let err = backend.launch(spec).await.expect_err(
            "launch must surface structured error until the clone path ships",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("clone") || msg.contains("deferred"),
            "expected clone/deferred error, got: {msg}"
        );
    }
}
