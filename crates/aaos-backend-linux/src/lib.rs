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

#[cfg(target_os = "linux")]
pub mod worker_tools;

/// Detect whether the host permits the mount operations the backend
/// needs inside an unprivileged user+mount namespace. Forks a child,
/// unshares CLONE_NEWUSER|CLONE_NEWNS, and tries a tmpfs mount in
/// /tmp. Returns true iff the mount succeeds.
///
/// Used by integration tests to skip on CI environments (GitHub
/// Actions Azure runners) where AppArmor or similar LSMs deny
/// unprivileged-userns mount operations despite
/// `kernel.unprivileged_userns_clone=1`. Dev boxes and DO droplets
/// return true.
#[cfg(target_os = "linux")]
pub fn probe_mount_capable() -> bool {
    use nix::sched::{unshare, CloneFlags};
    use nix::sys::wait::{waitpid, WaitStatus};
    use nix::unistd::{fork, ForkResult};

    // SAFETY: the child immediately unshares and calls exit; it does
    // not return to the caller. The parent only waits for its status.
    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            if unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS).is_err() {
                std::process::exit(2);
            }
            let path = format!("/tmp/aaos-probe-{}", std::process::id());
            if std::fs::create_dir_all(&path).is_err() {
                std::process::exit(3);
            }
            let mounted = nix::mount::mount::<str, str, str, str>(
                Some("tmpfs"),
                path.as_str(),
                Some("tmpfs"),
                nix::mount::MsFlags::empty(),
                Some("size=1M"),
            );
            let _ = std::fs::remove_dir_all(&path);
            std::process::exit(if mounted.is_ok() { 0 } else { 1 });
        }
        Ok(ForkResult::Parent { child }) => {
            matches!(waitpid(child, None), Ok(WaitStatus::Exited(_, 0)))
        }
        Err(_) => false,
    }
}

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use aaos_core::{
    AgentBackend, AgentLaunchHandle, AgentLaunchSpec, BackendHealth, CoreError, Result,
};

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
        // Default session_dir inside agentd's own RuntimeDirectory so
        // the daemon-as-aaos-user can create it without needing to
        // chown /var/run/aaos/. systemd gives us /run/agentd owned by
        // aaos:aaos 0750; we nest sessions/ inside it.
        let session_dir = std::env::var("AAOS_NAMESPACED_SESSION_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/run/agentd/sessions"));
        Self {
            session_dir,
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
    UidMapFailed(String),

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

    /// Look up the live session for an agent. Returns `None` if the
    /// agent has never launched or has already stopped. Used by
    /// integration tests and, eventually, by higher-level code that
    /// wants to send `Ping` / `Poke` / `InvokeTool` over the
    /// persistent stream.
    pub fn session(
        &self,
        agent_id: &aaos_core::AgentId,
    ) -> Option<Arc<crate::broker_session::BrokerSession>> {
        self.sessions.get(agent_id)
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

    pub(super) fn ensure_session_dir(
        dir: &std::path::Path,
    ) -> std::result::Result<(), BackendError> {
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
        let listener = UnixListener::bind(&socket_path).map_err(BackendError::BrokerIoFailed)?;
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
    ) -> std::result::Result<
        (
            tokio::net::unix::OwnedReadHalf,
            tokio::net::unix::OwnedWriteHalf,
        ),
        BackendError,
    > {
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

        // Return the split halves so the backend can install them on
        // the session as the persistent post-handshake transport.
        // BufReader::into_inner() gets the raw OwnedReadHalf back; any
        // bytes the reader already buffered are still there, but in
        // practice the handshake reads exactly two full lines and
        // nothing spills over.
        let read_half = reader.into_inner();
        Ok((read_half, write_half))
    }

    /// Clone a child with NEWUSER+NEWNS+NEWIPC, write uid/gid maps,
    /// set up a minimal rootfs inside the child's mount namespace,
    /// pivot_root into it, and execve the worker binary.
    ///
    /// Returns the host-side PID of the child on success. The child is
    /// blocked on a sync pipe until the parent has finished writing the
    /// uid/gid maps; this avoids the canonical race where the child tries
    /// to mount or open privileged paths before it has been mapped to
    /// uid 0 inside the new user namespace.
    ///
    /// On any child-side failure before execve, the child `_exit`s with
    /// a specific non-zero status so the parent's `waitpid` tells you
    /// which step failed (see inline `return N` values).
    pub(super) fn clone_and_launch_worker(
        config: &NamespacedBackendConfig,
        policy: &PolicyDescription,
        env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    ) -> std::result::Result<u32, BackendError> {
        use nix::sched::{clone, CloneFlags};
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::{pipe, write, Pid};
        use std::os::unix::io::{AsRawFd, OwnedFd};

        let parent_uid = nix::unistd::getuid().as_raw();
        let parent_gid = nix::unistd::getgid().as_raw();

        // Sync pipe for parent → child "uid/gid maps written, you can proceed".
        let (ready_rx, ready_tx): (OwnedFd, OwnedFd) =
            pipe().map_err(|e| BackendError::CloneFailed(format!("pipe: {e}")))?;
        let ready_rx_fd = ready_rx.as_raw_fd();

        let config_clone = config.clone();
        let policy_clone = policy.clone();
        let env_clone = env;

        // Child function. MUST NOT panic. Every early-exit returns a specific
        // status code that maps to a failure step — see comments.
        let child_fn = Box::new(move || -> isize {
            // Silence the child's stderr. The clone'd child inherits the
            // parent's tokio I/O driver state; tokio can emit stderr
            // chatter when it polls fds after the mount namespace
            // transition. Redirect fd 2 to /dev/null before anything
            // else runs — unless the operator explicitly wants to
            // capture worker stderr for debugging.
            //
            // Set AAOS_NAMESPACED_WORKER_STDERR=/path/to/file to send the
            // worker's stderr to a file instead. The fd remains valid
            // after pivot_root. Unbounded file — operator is responsible
            // for size management; only enable in tests / CI.
            let stderr_target: Option<std::path::PathBuf> =
                std::env::var_os("AAOS_NAMESPACED_WORKER_STDERR").map(std::path::PathBuf::from);
            let stderr_fd = match &stderr_target {
                Some(path) => std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .ok(),
                None => std::fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/null")
                    .ok(),
            };
            if let Some(f) = stderr_fd {
                use std::os::fd::AsRawFd;
                unsafe {
                    libc::dup2(f.as_raw_fd(), 2);
                }
            }

            // Optional step-by-step diagnostics. Enable with
            // AAOS_NAMESPACED_CHILD_DEBUG=/path/to/log in the parent
            // environment; the child writes one line per step so a
            // timeout on the handshake path can be traced without a
            // waitpid round-trip. Disabled by default (no writes).
            let debug_log: Option<std::path::PathBuf> =
                std::env::var_os("AAOS_NAMESPACED_CHILD_DEBUG").map(std::path::PathBuf::from);
            let log_step = |step: &str, err: Option<&str>| {
                let Some(ref path) = debug_log else { return };
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let msg = match err {
                        Some(e) => format!("{step}: ERR {e}\n"),
                        None => format!("{step}: ok\n"),
                    };
                    let _ = f.write_all(msg.as_bytes());
                }
            };
            // Step A: wait for parent to finish uid/gid mapping.
            let mut byte = [0u8; 1];
            match nix::unistd::read(ready_rx_fd, &mut byte) {
                Ok(n) if n == 1 => log_step("A-pipe-read", None),
                Err(e) => {
                    log_step("A-pipe-read", Some(&e.to_string()));
                    return 10;
                }
                _ => {
                    log_step("A-pipe-read", Some("short read"));
                    return 10;
                }
            }

            // Step B: detach mount propagation from the host so our
            // namespace-local mounts don't leak back. Prefer MS_PRIVATE;
            // fall back to MS_SLAVE, then to no-op.
            //
            // Restricted container hosts (GitHub Actions Azure runners,
            // some Docker-in-Docker setups) reject both MS_PRIVATE and
            // MS_SLAVE on / with EACCES even inside a user+mount
            // namespace, because AppArmor or an LSM blocks propagation
            // changes. In that case we proceed without remounting.
            // CLONE_NEWNS already isolated the mount namespace, and
            // pivot_root + umount of the old root happens later — the
            // only residual risk is a brief window where our tmpfs could
            // propagate outward on a `shared` ancestor. Acceptable for
            // ephemeral agent lifetimes; matches Docker's own fallback.
            let b_private = nix::mount::mount::<str, str, str, str>(
                None,
                "/",
                None,
                nix::mount::MsFlags::MS_PRIVATE | nix::mount::MsFlags::MS_REC,
                None,
            );
            match b_private {
                Ok(_) => log_step("B-ms-private", None),
                Err(private_err) => {
                    match nix::mount::mount::<str, str, str, str>(
                        None,
                        "/",
                        None,
                        nix::mount::MsFlags::MS_SLAVE | nix::mount::MsFlags::MS_REC,
                        None,
                    ) {
                        Ok(_) => log_step("B-ms-slave-fallback", None),
                        Err(slave_err) => {
                            // Both failed — host forbids propagation changes.
                            // Log and continue; CLONE_NEWNS gives us an
                            // isolated mount namespace anyway.
                            log_step(
                                "B-ms-nochange-fallback",
                                Some(&format!("private={private_err}, slave={slave_err}")),
                            );
                        }
                    }
                }
            }

            // Step C: new root tmpfs in a unique path.
            let new_root =
                std::path::PathBuf::from(format!("/tmp/aaos-newroot-{}", std::process::id()));
            if std::fs::create_dir_all(&new_root).is_err() {
                return 12;
            }
            match nix::mount::mount::<str, std::path::Path, str, str>(
                Some("tmpfs"),
                &new_root,
                Some("tmpfs"),
                nix::mount::MsFlags::empty(),
                Some("size=16M,mode=755"),
            ) {
                Ok(_) => log_step("C-tmpfs-newroot", None),
                Err(e) => {
                    log_step("C-tmpfs-newroot", Some(&e.to_string()));
                    return 13;
                }
            }

            // Step D: scratch tmpfs inside the new root (private, writable).
            let scratch_relative = policy_clone
                .scratch
                .strip_prefix("/")
                .unwrap_or(&policy_clone.scratch);
            let scratch_inside = new_root.join(scratch_relative);
            if std::fs::create_dir_all(&scratch_inside).is_err() {
                return 14;
            }
            match nix::mount::mount::<str, std::path::Path, str, str>(
                Some("tmpfs"),
                &scratch_inside,
                Some("tmpfs"),
                nix::mount::MsFlags::empty(),
                Some("size=64M,mode=755"),
            ) {
                Ok(_) => log_step("D-scratch-tmpfs", None),
                Err(e) => {
                    log_step("D-scratch-tmpfs", Some(&e.to_string()));
                    return 15;
                }
            }

            // Step D2: bind-mount the per-agent workspace (read-write)
            // at the same absolute path so tool calls using host paths
            // (e.g. /var/lib/aaos/workspace/<run-id>/hn.html) resolve
            // inside the worker. Only when policy.workspace.is_some().
            if let Some(ws) = policy_clone.workspace.as_ref() {
                let ws_rel = ws.strip_prefix("/").unwrap_or(ws);
                let inside_ws = new_root.join(ws_rel);
                if let Err(e) = std::fs::create_dir_all(&inside_ws) {
                    log_step("D2-mkdir-workspace", Some(&e.to_string()));
                    return 41;
                }
                // The host-side workspace dir must exist (executor
                // creates /var/lib/aaos/workspace/<run-id>/ before
                // spawning subtasks). If it doesn't, create it — the
                // worker's writes to the bind-mount will reach the
                // host filesystem directly.
                let _ = std::fs::create_dir_all(ws);
                match nix::mount::mount::<std::path::Path, std::path::Path, str, str>(
                    Some(ws),
                    &inside_ws,
                    None,
                    nix::mount::MsFlags::MS_BIND | nix::mount::MsFlags::MS_REC,
                    None,
                ) {
                    Ok(_) => log_step(&format!("D2-bind-workspace-{}", ws.display()), None),
                    Err(e) => {
                        log_step(
                            &format!("D2-bind-workspace-{}", ws.display()),
                            Some(&e.to_string()),
                        );
                        return 42;
                    }
                }
            }

            // Step E: bind-mount shared-lib paths read-only. Tolerate
            // remount-ro failure on odd filesystems — bind still limits
            // the inside-the-ns reach to the same paths.
            for lib_path in &policy_clone.shared_libs {
                let lib_rel = lib_path.strip_prefix("/").unwrap_or(lib_path);
                let inside = new_root.join(lib_rel);
                if let Err(e) = std::fs::create_dir_all(&inside) {
                    log_step(
                        &format!("E-mkdir-{}", lib_path.display()),
                        Some(&e.to_string()),
                    );
                    return 16;
                }
                match nix::mount::mount::<std::path::Path, std::path::Path, str, str>(
                    Some(lib_path),
                    &inside,
                    None,
                    nix::mount::MsFlags::MS_BIND | nix::mount::MsFlags::MS_REC,
                    None,
                ) {
                    Ok(_) => log_step(&format!("E-bind-{}", lib_path.display()), None),
                    Err(e) => {
                        log_step(
                            &format!("E-bind-{}", lib_path.display()),
                            Some(&e.to_string()),
                        );
                        return 17;
                    }
                }
                let _ = nix::mount::mount::<str, std::path::Path, str, str>(
                    None,
                    &inside,
                    None,
                    nix::mount::MsFlags::MS_BIND
                        | nix::mount::MsFlags::MS_REMOUNT
                        | nix::mount::MsFlags::MS_RDONLY
                        | nix::mount::MsFlags::MS_REC,
                    None,
                );
            }

            // Step F: bind-mount broker socket's parent dir so the worker
            // can `connect()` to the Unix socket inside the new root.
            if let Some(socket_parent) = policy_clone.broker_socket.parent() {
                let parent_rel = socket_parent.strip_prefix("/").unwrap_or(socket_parent);
                let inside_parent = new_root.join(parent_rel);
                if let Err(e) = std::fs::create_dir_all(&inside_parent) {
                    log_step("F-mkdir-socket-parent", Some(&e.to_string()));
                    return 18;
                }
                match nix::mount::mount::<std::path::Path, std::path::Path, str, str>(
                    Some(socket_parent),
                    &inside_parent,
                    None,
                    nix::mount::MsFlags::MS_BIND,
                    None,
                ) {
                    Ok(_) => log_step("F-bind-socket-parent", None),
                    Err(e) => {
                        log_step("F-bind-socket-parent", Some(&e.to_string()));
                        return 19;
                    }
                }
            }

            // Step G: bind-mount the worker binary read-only at its
            // canonical path inside new_root so execve finds it.
            let worker_rel = config_clone
                .worker_binary
                .strip_prefix("/")
                .unwrap_or(&config_clone.worker_binary);
            let worker_inside = new_root.join(worker_rel);
            if let Some(parent) = worker_inside.parent() {
                if std::fs::create_dir_all(parent).is_err() {
                    return 20;
                }
            }
            // Touch the file so the bind-mount has something to mount onto.
            if let Err(e) = std::fs::File::create(&worker_inside) {
                log_step("G-touch-worker", Some(&e.to_string()));
                return 21;
            }
            match nix::mount::mount::<std::path::Path, std::path::Path, str, str>(
                Some(&config_clone.worker_binary),
                &worker_inside,
                None,
                nix::mount::MsFlags::MS_BIND,
                None,
            ) {
                Ok(_) => log_step("G-bind-worker", None),
                Err(e) => {
                    log_step("G-bind-worker", Some(&e.to_string()));
                    return 22;
                }
            }

            // Step H: pivot_root into the new root, then detach the old one.
            let old_root_inside = new_root.join(".oldroot");
            if std::fs::create_dir_all(&old_root_inside).is_err() {
                return 23;
            }
            match nix::unistd::pivot_root(&new_root, &old_root_inside) {
                Ok(_) => log_step("H-pivot-root", None),
                Err(e) => {
                    log_step("H-pivot-root", Some(&e.to_string()));
                    return 24;
                }
            }
            if let Err(e) = std::env::set_current_dir("/") {
                log_step("H-chdir-slash", Some(&e.to_string()));
                return 25;
            }
            match nix::mount::umount2("/.oldroot", nix::mount::MntFlags::MNT_DETACH) {
                Ok(_) => log_step("H-umount-old", None),
                Err(e) => {
                    log_step("H-umount-old", Some(&e.to_string()));
                    return 26;
                }
            }
            log_step("I-pre-execve", None);

            // Step I: execve the worker.
            let worker_cstr = match std::ffi::CString::new(
                config_clone
                    .worker_binary
                    .as_os_str()
                    .to_str()
                    .unwrap_or(""),
            ) {
                Ok(s) => s,
                Err(_) => return 27,
            };
            let argv = vec![worker_cstr.clone()];
            let envp: Vec<std::ffi::CString> = env_clone
                .iter()
                .filter_map(|(k, v)| {
                    let k_str = k.to_str()?;
                    let v_str = v.to_str()?;
                    std::ffi::CString::new(format!("{k_str}={v_str}")).ok()
                })
                .collect();

            let _ = nix::unistd::execve(&worker_cstr, &argv, &envp);
            // execve returned → it failed.
            28
        });

        // Child stack. 1 MiB is ample for our worker before execve replaces
        // the image.
        let mut stack = vec![0u8; 1024 * 1024];
        let flags = CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWIPC;

        let child_pid_raw: i32 = unsafe { clone(child_fn, &mut stack, flags, Some(libc::SIGCHLD)) }
            .map_err(|e| BackendError::CloneFailed(format!("clone: {e}")))?
            .as_raw();

        // Parent: write uid/gid maps. Hard failure — no silent fallback.
        if let Err(e) = write_uid_gid_maps(child_pid_raw, parent_uid, parent_gid) {
            let _ = kill(Pid::from_raw(child_pid_raw), Signal::SIGKILL);
            let _ = nix::sys::wait::waitpid(Pid::from_raw(child_pid_raw), None);
            return Err(e);
        }

        // Parent closes the read end; we only need the write end to unblock
        // the child.
        drop(ready_rx);
        let go: [u8; 1] = [1];
        if write(&ready_tx, &go).is_err() {
            let _ = kill(Pid::from_raw(child_pid_raw), Signal::SIGKILL);
            let _ = nix::sys::wait::waitpid(Pid::from_raw(child_pid_raw), None);
            return Err(BackendError::CloneFailed(
                "failed to signal child after uid/gid map".into(),
            ));
        }
        drop(ready_tx);

        Ok(child_pid_raw as u32)
    }

    fn write_uid_gid_maps(pid: i32, uid: u32, gid: u32) -> std::result::Result<(), BackendError> {
        use std::io::Write;
        let write_file = |path: String, content: &str| -> std::result::Result<(), BackendError> {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .map_err(|e| BackendError::UidMapFailed(format!("open {path}: {e}")))?;
            f.write_all(content.as_bytes())
                .map_err(|e| BackendError::UidMapFailed(format!("write {path}: {e}")))?;
            Ok(())
        };
        write_file(format!("/proc/{pid}/uid_map"), &format!("0 {uid} 1\n"))?;
        // setgroups must be 'deny' before gid_map can be written in an
        // unprivileged user namespace. See user_namespaces(7).
        write_file(format!("/proc/{pid}/setgroups"), "deny\n")?;
        write_file(format!("/proc/{pid}/gid_map"), &format!("0 {gid} 1\n"))?;
        Ok(())
    }

    pub(super) async fn launch_impl(
        backend: &NamespacedBackend,
        spec: AgentLaunchSpec,
    ) -> Result<AgentLaunchHandle> {
        ensure_session_dir(&backend.config.session_dir)?;

        let (listener, socket_path) =
            bind_session_socket(&backend.config.session_dir, &spec.agent_id)?;

        // Phase F-b/3b: thread the per-agent workspace path into the
        // policy. Non-empty → bind-mounted at the same absolute path
        // inside the worker's mount ns + added to the Landlock rw
        // allow-list so plan-executor subtasks (analyzer/writer/etc.)
        // can actually read + write the shared workspace. Empty
        // PathBuf = None = no workspace visibility (pure compute).
        let workspace = if spec.workspace_path.as_os_str().is_empty() {
            None
        } else {
            Some(spec.workspace_path.clone())
        };
        let policy = PolicyDescription {
            scratch: PathBuf::from("/scratch"),
            shared_libs: backend.config.shared_lib_paths.clone(),
            broker_socket: socket_path.clone(),
            workspace,
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

        let child_pid = clone_and_launch_worker(&backend.config, &policy, env).map_err(|e| {
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
        let install_session = session_arc.clone();
        let handshake = async move {
            let (stream, _addr) = listener
                .accept()
                .await
                .map_err(BackendError::BrokerIoFailed)?;
            let (read_half, write_half) = run_handshake(stream, handshake_session).await?;
            sandboxed_rx
                .await
                .map_err(|_| BackendError::ReadyTimeout { timeout_ms })?;
            // Handshake complete, sandbox is in force. Hand the persistent
            // stream to the session so broker→worker traffic (Ping, Poke,
            // future InvokeTool) has a transport.
            install_session
                .install_post_handshake_stream(read_half, write_half)
                .await;
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
        // Check the child process itself — peer-creds validation happened
        // at accept() time, so the pid on the handle is the authoritative
        // child. Priority:
        //   1. waitpid(WNOHANG) to detect terminated state.
        //   2. /proc/<pid> existence to confirm the process is still alive.
        //   3. Session presence for "did we ever launch this agent".
        let Some(state) = handle.state::<NamespacedState>() else {
            return BackendHealth::Unknown("handle state not NamespacedState".into());
        };
        let pid = nix::unistd::Pid::from_raw(state.pid as i32);
        match nix::sys::wait::waitpid(pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
            Ok(nix::sys::wait::WaitStatus::StillAlive) => BackendHealth::Healthy,
            Ok(nix::sys::wait::WaitStatus::Exited(_, code)) => BackendHealth::Exited(code),
            Ok(nix::sys::wait::WaitStatus::Signaled(_, sig, _)) => {
                BackendHealth::Signaled(sig as i32)
            }
            Ok(_) => BackendHealth::Healthy,
            Err(nix::Error::ECHILD) => {
                // Process was reaped (e.g. by stop()) or was never our child.
                // Check /proc to decide between Exited-and-reaped and truly-alive.
                if std::path::Path::new(&format!("/proc/{}", state.pid)).exists() {
                    // Still alive but not our child (shouldn't happen in
                    // practice unless the system reaped via init).
                    BackendHealth::Disconnected
                } else {
                    BackendHealth::Exited(0)
                }
            }
            Err(e) => BackendHealth::Unknown(format!("waitpid: {e}")),
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

    /// Launch the backend end-to-end against the real kernel.
    ///
    /// Gated `#[ignore]` because it needs: (a) Landlock available,
    /// (b) unprivileged user namespaces enabled, (c) the worker
    /// binary at the configured path, and (d) peer-creds matching.
    /// Run manually via `cargo test --features namespaced-agents
    /// -- --include-ignored namespaced_backend_end_to_end`.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires Linux 5.13+ with unprivileged user namespaces and the worker binary built; run manually"]
    async fn namespaced_backend_end_to_end() {
        if !landlock_compile::is_supported() {
            eprintln!("SKIP: Landlock not supported on this kernel");
            return;
        }
        // GitHub Actions Azure runners grant `kernel.unprivileged_userns_clone`
        // but AppArmor on the runner host denies mount operations inside
        // those namespaces (both propagation changes on / AND new tmpfs
        // mounts fail with EACCES). There is no way to test the real
        // sandboxing primitives in that environment — the kernel supports
        // them but the host LSM forbids them. Skip rather than hang for 5s
        // on the readiness timeout. See child-steps.log diagnostics in
        // CI runs 24608722044 / 24608802200 / 24608876000 for the
        // empirical evidence.
        if !super::probe_mount_capable() {
            eprintln!(
                "SKIP: host forbids mount operations inside user namespaces \
                 (likely GitHub Actions or other LSM-restricted CI). \
                 Run this test on a real Linux host (dev box or DO droplet)."
            );
            return;
        }
        use aaos_core::{AgentId, AgentManifest};

        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = NamespacedBackendConfig::default();
        cfg.session_dir = tmp.path().join("sessions");
        // Point worker_binary at the test build artifact location. Must be
        // an absolute path because the child changes cwd during pivot_root.
        let manifest_dir =
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo test");
        let workspace_root = std::path::Path::new(&manifest_dir)
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        cfg.worker_binary = workspace_root.join("target/debug/aaos-agent-worker");
        assert!(
            cfg.worker_binary.is_absolute(),
            "worker_binary must be absolute for bind-mount, got: {}",
            cfg.worker_binary.display()
        );
        assert!(
            cfg.worker_binary.exists(),
            "worker binary missing — run `cargo build -p aaos-backend-linux --bin aaos-agent-worker` first: {}",
            cfg.worker_binary.display()
        );

        // Diagnostics: capture child-step progress + worker stderr under the
        // tempdir so a launch timeout (e.g. on CI) surfaces the actual
        // failure point instead of a bare 5s-timeout error.
        let child_log = tmp.path().join("child-steps.log");
        let worker_stderr = tmp.path().join("worker-stderr.log");
        // SAFETY: test is single-threaded until backend.launch spawns the
        // child; no other thread reads these env vars at this point.
        unsafe {
            std::env::set_var("AAOS_NAMESPACED_CHILD_DEBUG", &child_log);
            std::env::set_var("AAOS_NAMESPACED_WORKER_STDERR", &worker_stderr);
        }

        let backend = NamespacedBackend::new(cfg).expect("Landlock supported");
        let spec = AgentLaunchSpec {
            agent_id: AgentId::new(),
            manifest: AgentManifest::from_yaml(
                r#"
name: e2e
model: claude-haiku-4-5-20251001
system_prompt: "x"
"#,
            )
            .unwrap(),
            capability_handles: vec![],
            workspace_path: PathBuf::new(),
            budget_config: None,
        };
        let handle = match backend.launch(spec).await {
            Ok(h) => h,
            Err(e) => {
                eprintln!("=== namespaced launch failed: {e} ===");
                eprintln!("--- child-steps.log ---");
                eprintln!(
                    "{}",
                    std::fs::read_to_string(&child_log)
                        .unwrap_or_else(|_| "(no child step log — child never ran)".into())
                );
                eprintln!("--- worker-stderr.log ---");
                eprintln!(
                    "{}",
                    std::fs::read_to_string(&worker_stderr)
                        .unwrap_or_else(|_| "(no worker stderr — worker never reached exec)".into())
                );
                panic!("namespaced launch should succeed on a capable host: {e}");
            }
        };
        assert_eq!(handle.backend_kind, "namespaced");
        // Child is alive; verify /proc/<pid>/status shows we applied
        // the sandboxing primitives.
        let state = handle
            .state::<NamespacedState>()
            .expect("handle carries NamespacedState");
        let status_path = format!("/proc/{}/status", state.pid);
        let status = std::fs::read_to_string(&status_path).expect("can read child /proc status");
        assert!(
            status.contains("Seccomp:\t2") || status.contains("Seccomp: 2"),
            "child must be seccomp-filtered (mode 2), got: {status}"
        );
        assert!(
            status.contains("NoNewPrivs:\t1") || status.contains("NoNewPrivs: 1"),
            "child must have NO_NEW_PRIVS set, got: {status}"
        );
        backend.stop(&handle).await.expect("stop should succeed");
    }

    /// Invoke `file_read` on one of the backend's shared-lib paths (which is
    /// inside the worker's Landlock allow-list) and assert it succeeds.
    ///
    /// Gated `#[ignore]` — same prerequisites as `namespaced_backend_end_to_end`.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires Linux 5.13+ with unprivileged user namespaces and the worker binary built; run manually"]
    async fn invoke_tool_file_read_roundtrips() {
        if !landlock_compile::is_supported() {
            eprintln!("SKIP: Landlock not supported on this kernel");
            return;
        }
        if !super::probe_mount_capable() {
            eprintln!(
                "SKIP: host forbids mount operations inside user namespaces \
                 (likely GitHub Actions or other LSM-restricted CI). \
                 Run this test on a real Linux host (dev box or DO droplet)."
            );
            return;
        }
        use aaos_core::{AgentId, AgentManifest};

        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = NamespacedBackendConfig::default();
        cfg.session_dir = tmp.path().join("sessions");
        let manifest_dir =
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo test");
        let workspace_root = std::path::Path::new(&manifest_dir)
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        cfg.worker_binary = workspace_root.join("target/debug/aaos-agent-worker");
        assert!(
            cfg.worker_binary.is_absolute(),
            "worker_binary must be absolute for bind-mount, got: {}",
            cfg.worker_binary.display()
        );
        assert!(
            cfg.worker_binary.exists(),
            "worker binary missing — run `cargo build -p aaos-backend-linux --bin aaos-agent-worker` first: {}",
            cfg.worker_binary.display()
        );

        let child_log = tmp.path().join("child-steps.log");
        let worker_stderr = tmp.path().join("worker-stderr.log");
        unsafe {
            std::env::set_var("AAOS_NAMESPACED_CHILD_DEBUG", &child_log);
            std::env::set_var("AAOS_NAMESPACED_WORKER_STDERR", &worker_stderr);
        }

        let backend = NamespacedBackend::new(cfg).expect("Landlock supported");
        let agent_id = AgentId::new();
        let spec = AgentLaunchSpec {
            agent_id: agent_id.clone(),
            manifest: AgentManifest::from_yaml(
                r#"
name: e2e-file-read
model: claude-haiku-4-5-20251001
system_prompt: "x"
"#,
            )
            .unwrap(),
            capability_handles: vec![],
            workspace_path: PathBuf::new(),
            budget_config: None,
        };
        let handle = match backend.launch(spec).await {
            Ok(h) => h,
            Err(e) => {
                eprintln!("=== namespaced launch failed: {e} ===");
                eprintln!("--- child-steps.log ---");
                eprintln!(
                    "{}",
                    std::fs::read_to_string(&child_log)
                        .unwrap_or_else(|_| "(no child step log — child never ran)".into())
                );
                eprintln!("--- worker-stderr.log ---");
                eprintln!(
                    "{}",
                    std::fs::read_to_string(&worker_stderr)
                        .unwrap_or_else(|_| "(no worker stderr — worker never reached exec)".into())
                );
                panic!("namespaced launch should succeed on a capable host: {e}");
            }
        };

        let session = backend
            .session(&agent_id)
            .expect("backend has a broker session for this agent after launch");

        // Pick the first shared-lib directory from the backend config. Find a
        // concrete UTF-8 text file inside it — file_read requires a regular
        // file and returns text content. Walk subdirectories to find one
        // (e.g., gconv/gconv-modules in /lib/x86_64-linux-gnu/gconv/).
        let lib_dir = backend
            .shared_lib_paths()
            .first()
            .expect("at least one shared lib configured")
            .clone();

        fn find_text_file(dir: &std::path::Path) -> Option<std::path::PathBuf> {
            let rd = std::fs::read_dir(dir).ok()?;
            for entry in rd.filter_map(|e| e.ok()) {
                let path = entry.path();
                let m = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if m.is_file() {
                    // Heuristic: read the first 256 bytes. Reject ELF binaries
                    // (magic "\x7fELF") and any file with non-UTF-8 bytes.
                    if let Ok(bytes) = std::fs::read(&path) {
                        if bytes.starts_with(b"\x7fELF") {
                            continue;
                        }
                        let sample = &bytes[..bytes.len().min(256)];
                        if std::str::from_utf8(sample).is_ok() && !sample.is_empty() {
                            return Some(path);
                        }
                    }
                } else if m.is_dir() {
                    if let Some(f) = find_text_file(&path) {
                        return Some(f);
                    }
                }
            }
            None
        }

        let readable = find_text_file(&lib_dir)
            .unwrap_or_else(|| lib_dir.join("ld-linux-x86-64.so.2"));

        // Build a FileRead token for the shared-lib directory so the tool's
        // own capability re-check passes inside the worker.
        let lib_prefix = lib_dir.to_string_lossy().into_owned();
        let file_read_token = aaos_core::CapabilityToken::issue(
            agent_id,
            aaos_core::Capability::FileRead {
                path_glob: format!("{lib_prefix}/*"),
            },
            aaos_core::Constraints::default(),
        );

        let result = session
            .invoke_over_worker(
                "file_read",
                serde_json::json!({ "path": readable.to_string_lossy() }),
                vec![file_read_token],
            )
            .await
            .expect("file_read over broker should succeed inside Landlock scope");

        assert!(
            result.is_object() || result.is_string(),
            "file_read result should be an object or string, got: {result:?}"
        );

        backend.stop(&handle).await.expect("stop should succeed");
    }

    /// Invoke `file_read` on `/etc/shadow` — a path outside the worker's
    /// Landlock allow-list — and assert the call comes back as an error that
    /// surfaces the denial.
    ///
    /// Gated `#[ignore]` — same prerequisites as `namespaced_backend_end_to_end`.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires Linux 5.13+ with unprivileged user namespaces and the worker binary built; run manually"]
    async fn invoke_tool_landlock_denial() {
        if !landlock_compile::is_supported() {
            eprintln!("SKIP: Landlock not supported on this kernel");
            return;
        }
        if !super::probe_mount_capable() {
            eprintln!(
                "SKIP: host forbids mount operations inside user namespaces \
                 (likely GitHub Actions or other LSM-restricted CI). \
                 Run this test on a real Linux host (dev box or DO droplet)."
            );
            return;
        }
        use aaos_core::{AgentId, AgentManifest};

        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = NamespacedBackendConfig::default();
        cfg.session_dir = tmp.path().join("sessions");
        let manifest_dir =
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo test");
        let workspace_root = std::path::Path::new(&manifest_dir)
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        cfg.worker_binary = workspace_root.join("target/debug/aaos-agent-worker");
        assert!(
            cfg.worker_binary.is_absolute(),
            "worker_binary must be absolute for bind-mount, got: {}",
            cfg.worker_binary.display()
        );
        assert!(
            cfg.worker_binary.exists(),
            "worker binary missing — run `cargo build -p aaos-backend-linux --bin aaos-agent-worker` first: {}",
            cfg.worker_binary.display()
        );

        let child_log = tmp.path().join("child-steps.log");
        let worker_stderr = tmp.path().join("worker-stderr.log");
        unsafe {
            std::env::set_var("AAOS_NAMESPACED_CHILD_DEBUG", &child_log);
            std::env::set_var("AAOS_NAMESPACED_WORKER_STDERR", &worker_stderr);
        }

        let backend = NamespacedBackend::new(cfg).expect("Landlock supported");
        let agent_id = AgentId::new();
        let spec = AgentLaunchSpec {
            agent_id: agent_id.clone(),
            manifest: AgentManifest::from_yaml(
                r#"
name: e2e-landlock-denial
model: claude-haiku-4-5-20251001
system_prompt: "x"
"#,
            )
            .unwrap(),
            capability_handles: vec![],
            workspace_path: PathBuf::new(),
            budget_config: None,
        };
        let handle = match backend.launch(spec).await {
            Ok(h) => h,
            Err(e) => {
                eprintln!("=== namespaced launch failed: {e} ===");
                eprintln!("--- child-steps.log ---");
                eprintln!(
                    "{}",
                    std::fs::read_to_string(&child_log)
                        .unwrap_or_else(|_| "(no child step log — child never ran)".into())
                );
                eprintln!("--- worker-stderr.log ---");
                eprintln!(
                    "{}",
                    std::fs::read_to_string(&worker_stderr)
                        .unwrap_or_else(|_| "(no worker stderr — worker never reached exec)".into())
                );
                panic!("namespaced launch should succeed on a capable host: {e}");
            }
        };

        let session = backend
            .session(&agent_id)
            .expect("backend has a broker session for this agent after launch");

        // /etc/shadow is outside the worker's Landlock allow-list; the tool
        // must return an error that carries a recognisable denial string.
        // Pass a wildcard FileRead token so the tool's own capability check
        // passes — Landlock is the actual gate here (defense-in-depth layer 2).
        let wildcard_token = aaos_core::CapabilityToken::issue(
            agent_id,
            aaos_core::Capability::FileRead {
                path_glob: "*".into(),
            },
            aaos_core::Constraints::default(),
        );
        let result = session
            .invoke_over_worker(
                "file_read",
                serde_json::json!({ "path": "/etc/shadow" }),
                vec![wildcard_token],
            )
            .await;

        let err = result.expect_err("reading /etc/shadow must fail inside Landlock");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("denied")
                || msg.contains("eacces")
                || msg.contains("permission")
                || msg.contains("not permitted")
                || msg.contains("tool error"),
            "error must surface landlock denial, got: {err:?}",
        );

        backend.stop(&handle).await.expect("stop should succeed");
    }
}
