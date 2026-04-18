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
            // Silence stderr in the child. The clone'd child inherits the
            // parent's tokio I/O driver state; tokio can emit a noisy
            // panic to stderr when it polls fds after the mount namespace
            // transition. That panic is cosmetic — child_fn completes
            // successfully, execve replaces the image, the worker runs
            // normally. But the stderr output pollutes test runs.
            // Redirect fd 2 to /dev/null before anything else runs.
            if let Ok(devnull) = std::fs::OpenOptions::new().write(true).open("/dev/null") {
                use std::os::fd::AsRawFd;
                unsafe {
                    libc::dup2(devnull.as_raw_fd(), 2);
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

            // Step B: make current mount propagation private so our
            // namespace-local mounts don't leak to the host.
            match nix::mount::mount::<str, str, str, str>(
                None,
                "/",
                None,
                nix::mount::MsFlags::MS_PRIVATE | nix::mount::MsFlags::MS_REC,
                None,
            ) {
                Ok(_) => log_step("B-ms-private", None),
                Err(e) => {
                    log_step("B-ms-private", Some(&e.to_string()));
                    return 11;
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
        let handle = backend
            .launch(spec)
            .await
            .expect("namespaced launch should succeed on a capable host");
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
}
