//! Broker-side session state.
//!
//! One [`BrokerSession`] per launched agent. Created before `clone()`,
//! bound to the accepted Unix socket fd after the worker connects and
//! passes [`SO_PEERCRED`](peer_creds_match) validation.
//!
//! Sessions are **bound to the accepted fd**: after the initial
//! peer-creds check at `accept()` time, no per-message re-validation
//! happens. The kernel guarantees the other end of that fd is the
//! same process that connected (unless the fd is duped — which, in
//! a namespaced worker with seccomp denying `dup2`, isn't a vector
//! we worry about).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use aaos_core::AgentId;
use tokio::sync::{oneshot, Mutex};

use crate::broker_protocol::PolicyDescription;

/// One session = one launched worker.
pub struct BrokerSession {
    pub agent_id: AgentId,

    /// PID the broker expects to see on the other end of the Unix
    /// socket when the worker calls `connect()`. Set by [`launch`] to
    /// the clone'd child's pid. Used for peer-creds validation.
    ///
    /// [`launch`]: crate::NamespacedBackend::launch
    pub expected_pid: u32,

    /// uid the broker expects the peer to match (agentd's own uid).
    pub expected_uid: u32,

    /// gid the broker expects the peer to match (agentd's own gid).
    pub expected_gid: u32,

    /// The policy description that was / will be sent in `ready-ack`.
    pub policy: PolicyDescription,

    /// Unix socket path this session listens on.
    pub socket_path: PathBuf,

    /// Wall-clock creation time. Diagnostic only.
    pub created_at: Instant,

    /// Last time a message arrived on the bound socket. Updated each
    /// message, read by `health()`.
    pub last_activity: Mutex<Instant>,

    /// One-shot signal fired when the worker sends
    /// [`crate::broker_protocol::Request::SandboxedReady`]. This is
    /// the signal that unblocks `launch()`.
    pub sandboxed_ready_tx: Mutex<Option<oneshot::Sender<()>>>,
}

impl BrokerSession {
    /// Construct a fresh session. `sandboxed_ready_rx` is the
    /// matching receiver the backend awaits; keep it alongside the
    /// session until the handshake fires.
    pub fn new(
        agent_id: AgentId,
        expected_pid: u32,
        expected_uid: u32,
        expected_gid: u32,
        policy: PolicyDescription,
        socket_path: PathBuf,
    ) -> (Self, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        let now = Instant::now();
        let session = Self {
            agent_id,
            expected_pid,
            expected_uid,
            expected_gid,
            policy,
            socket_path,
            created_at: now,
            last_activity: Mutex::new(now),
            sandboxed_ready_tx: Mutex::new(Some(tx)),
        };
        (session, rx)
    }

    /// Validate credentials from `SO_PEERCRED`.
    ///
    /// `expected_pid` is matched exactly — the connecting peer must be
    /// the pid the backend launched. A mismatch indicates either a bug
    /// (wrong session socket path chosen by a worker) or an attempted
    /// hijack (another process on the host connecting to the socket).
    /// Both are hard errors.
    pub fn peer_creds_match(&self, peer: PeerCreds) -> Result<(), PeerCredsError> {
        if peer.pid != self.expected_pid {
            return Err(PeerCredsError::PidMismatch {
                expected: self.expected_pid,
                actual: peer.pid,
            });
        }
        if peer.uid != self.expected_uid {
            return Err(PeerCredsError::UidMismatch {
                expected: self.expected_uid,
                actual: peer.uid,
            });
        }
        if peer.gid != self.expected_gid {
            return Err(PeerCredsError::GidMismatch {
                expected: self.expected_gid,
                actual: peer.gid,
            });
        }
        Ok(())
    }

    /// Fire the `sandboxed-ready` notification. Idempotent — calling
    /// twice is a no-op (second call logs a warning).
    pub async fn fire_sandboxed_ready(&self) {
        let mut guard = self.sandboxed_ready_tx.lock().await;
        if let Some(tx) = guard.take() {
            let _ = tx.send(());
        } else {
            tracing::warn!(
                agent_id = %self.agent_id,
                "sandboxed-ready fired twice; second fire ignored"
            );
        }
    }
}

/// Extracted `SO_PEERCRED` values. Kept as a plain struct to isolate
/// the broker logic from nix/tokio-specific types, so unit tests can
/// supply synthetic creds without a real socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCreds {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PeerCredsError {
    #[error("peer pid mismatch: expected {expected}, got {actual}")]
    PidMismatch { expected: u32, actual: u32 },
    #[error("peer uid mismatch: expected {expected}, got {actual}")]
    UidMismatch { expected: u32, actual: u32 },
    #[error("peer gid mismatch: expected {expected}, got {actual}")]
    GidMismatch { expected: u32, actual: u32 },
}

/// Collection of all currently-live sessions, keyed by agent id.
///
/// `dashmap` is cheap for the read-mostly workload (workers look up
/// their own session once at accept time; there's no lock contention
/// between different agents' sessions). Insertion and removal are
/// serialised per-agent at the `AgentId` granularity.
#[derive(Default)]
pub struct SessionMap {
    inner: dashmap::DashMap<AgentId, Arc<BrokerSession>>,
}

impl SessionMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, session: Arc<BrokerSession>) {
        self.inner.insert(session.agent_id, session);
    }

    pub fn get(&self, agent_id: &AgentId) -> Option<Arc<BrokerSession>> {
        self.inner.get(agent_id).map(|r| r.clone())
    }

    pub fn remove(&self, agent_id: &AgentId) -> Option<Arc<BrokerSession>> {
        self.inner.remove(agent_id).map(|(_, v)| v)
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

/// Helper: on Linux, extract `SO_PEERCRED` from a
/// [`tokio::net::UnixStream`]. On non-Linux, always returns an error.
#[cfg(target_os = "linux")]
pub fn peer_creds_from_stream(
    stream: &tokio::net::UnixStream,
) -> std::io::Result<PeerCreds> {
    let creds = stream.peer_cred()?;
    Ok(PeerCreds {
        pid: creds.pid().unwrap_or(0) as u32,
        uid: creds.uid(),
        gid: creds.gid(),
    })
}

#[cfg(not(target_os = "linux"))]
pub fn peer_creds_from_stream(
    _stream: &tokio::net::UnixStream,
) -> std::io::Result<PeerCreds> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "SO_PEERCRED only supported on Linux",
    ))
}

/// Opaque map of session socket paths (for debug printing only — not
/// for session lookup; lookups go through [`SessionMap`]).
pub type SessionPaths = HashMap<AgentId, PathBuf>;

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_policy() -> PolicyDescription {
        PolicyDescription {
            scratch: PathBuf::from("/scratch"),
            shared_libs: vec![],
            broker_socket: PathBuf::from("/tmp/s.sock"),
        }
    }

    #[test]
    fn broker_session_stores_and_retrieves() {
        let map = SessionMap::new();
        let id = AgentId::new();
        let (session, _rx) = BrokerSession::new(
            id,
            4242,
            1000,
            1000,
            sample_policy(),
            PathBuf::from("/tmp/a.sock"),
        );
        map.insert(Arc::new(session));
        assert_eq!(map.len(), 1);
        let got = map.get(&id).expect("must find session");
        assert_eq!(got.agent_id, id);
        assert_eq!(got.expected_pid, 4242);
        let removed = map.remove(&id).expect("remove must return");
        assert_eq!(removed.agent_id, id);
        assert!(map.is_empty());
    }

    #[test]
    fn peer_creds_mismatch_detected() {
        let id = AgentId::new();
        let (session, _rx) = BrokerSession::new(
            id,
            4242,
            1000,
            1000,
            sample_policy(),
            PathBuf::from("/tmp/a.sock"),
        );

        // Wrong pid.
        assert_eq!(
            session.peer_creds_match(PeerCreds {
                pid: 9999,
                uid: 1000,
                gid: 1000
            }),
            Err(PeerCredsError::PidMismatch {
                expected: 4242,
                actual: 9999
            })
        );

        // Wrong uid.
        assert_eq!(
            session.peer_creds_match(PeerCreds {
                pid: 4242,
                uid: 1,
                gid: 1000
            }),
            Err(PeerCredsError::UidMismatch {
                expected: 1000,
                actual: 1
            })
        );

        // Wrong gid.
        assert_eq!(
            session.peer_creds_match(PeerCreds {
                pid: 4242,
                uid: 1000,
                gid: 2
            }),
            Err(PeerCredsError::GidMismatch {
                expected: 1000,
                actual: 2
            })
        );

        // Match.
        assert!(session
            .peer_creds_match(PeerCreds {
                pid: 4242,
                uid: 1000,
                gid: 1000
            })
            .is_ok());
    }

    #[tokio::test]
    async fn sandboxed_ready_fires_once() {
        let id = AgentId::new();
        let (session, rx) = BrokerSession::new(
            id,
            1,
            1,
            1,
            sample_policy(),
            PathBuf::from("/tmp/a.sock"),
        );
        session.fire_sandboxed_ready().await;
        assert!(rx.await.is_ok());
        // Second fire is a no-op (logs warning, doesn't panic).
        session.fire_sandboxed_ready().await;
    }
}
