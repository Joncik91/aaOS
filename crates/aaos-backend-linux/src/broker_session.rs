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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aaos_core::AgentId;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{oneshot, Mutex};

use crate::broker_protocol::PolicyDescription;
use crate::broker_protocol::{PokeOp, Request, WireRequest, WireResponse};

/// Pending in-flight request awaiting a response. The reader task takes
/// the oneshot sender out of the map when a matching response arrives
/// and sends the response through it, unblocking the caller awaiting the
/// other end of the channel.
type PendingResponses = Mutex<HashMap<u64, oneshot::Sender<WireResponse>>>;

/// Errors raised when sending a request to a worker over the persistent
/// post-handshake stream.
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    /// No write half installed on the session (e.g. the handshake
    /// hasn't run yet, or it failed). Calling a send_* method before
    /// the session is ready is a programmer error; surfacing it as a
    /// proper error rather than a panic makes tests more informative.
    #[error("broker session has no write half — handshake did not complete?")]
    NotConnected,
    /// Underlying socket I/O failed.
    #[error("broker I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// Could not serialize the outgoing request.
    #[error("serialize request: {0}")]
    Serialize(serde_json::Error),
    /// The reader task was torn down (worker exited, broker shutdown)
    /// between the time we parked the oneshot receiver and now.
    #[error("response channel closed — worker gone")]
    ResponseChannelClosed,
    /// The worker did not reply within the deadline.
    #[error("timeout waiting for response to request {request_id} after {elapsed_ms} ms")]
    Timeout { request_id: u64, elapsed_ms: u64 },
}

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

    /// Write half of the persistent broker↔worker stream. Installed by
    /// `install_post_handshake_stream` once the handshake completes.
    /// Guarded by `Mutex` so concurrent `send_*` callers serialize their
    /// writes — the wire protocol is line-oriented, partial writes would
    /// corrupt the framing.
    write_half: Mutex<Option<OwnedWriteHalf>>,

    /// In-flight requests awaiting responses from the worker. The reader
    /// task populates the tx side; `send_request` awaits the rx side.
    /// Keyed by `WireRequest.id`.
    pending: Arc<PendingResponses>,

    /// Monotonic request-id source. Starts at 100 so it cannot collide
    /// with the `Ready` (1) and `SandboxedReady` (2) ids used during the
    /// handshake.
    next_request_id: AtomicU64,
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
            write_half: Mutex::new(None),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_request_id: AtomicU64::new(100),
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

    /// Install the persistent post-handshake stream. Called by the
    /// backend after `run_handshake` completes: the stream's read half
    /// becomes a reader task that populates `pending`, the write half
    /// is stored so `send_*` methods can issue requests.
    ///
    /// The reader task exits when the worker closes the socket (clean
    /// shutdown) or when the read fails (crash). On exit it drains the
    /// pending map, dropping any parked `oneshot::Sender`s — callers
    /// awaiting on the matching rx get `ResponseChannelClosed`.
    pub async fn install_post_handshake_stream(
        self: Arc<Self>,
        read_half: tokio::net::unix::OwnedReadHalf,
        write_half: OwnedWriteHalf,
    ) {
        {
            let mut guard = self.write_half.lock().await;
            *guard = Some(write_half);
        }
        let pending = self.pending.clone();
        let agent_id = self.agent_id;
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};

            let mut reader = BufReader::new(read_half);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        tracing::debug!(%agent_id, "broker reader: worker closed");
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(%agent_id, error=%e, "broker reader: read failed");
                        break;
                    }
                }
                let trimmed = line.trim_end();
                if trimmed.is_empty() {
                    continue;
                }
                let resp: WireResponse = match serde_json::from_str(trimmed) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(
                            %agent_id,
                            error=%e,
                            line=%trimmed,
                            "broker reader: malformed response"
                        );
                        continue;
                    }
                };
                let tx = {
                    let mut map = pending.lock().await;
                    map.remove(&resp.id)
                };
                match tx {
                    Some(sender) => {
                        let _ = sender.send(resp);
                    }
                    None => {
                        tracing::warn!(
                            %agent_id,
                            id = resp.id,
                            "broker reader: response to unknown request id"
                        );
                    }
                }
            }

            // Reader exiting — drain pending so any awaiters wake up with
            // ResponseChannelClosed rather than hanging forever.
            let mut map = pending.lock().await;
            map.clear();
        });
    }

    /// Send a request to the worker and await the matching response.
    /// Serializes, writes a newline-terminated JSON frame, parks a
    /// oneshot receiver in `pending`, waits until the reader task
    /// routes the response (or the deadline elapses).
    async fn send_request(
        &self,
        req: Request,
        timeout: Duration,
    ) -> Result<WireResponse, SendError> {
        let id = self.next_request_id.fetch_add(1, Ordering::AcqRel);
        let wire = WireRequest::new(id, req);
        let mut buf = serde_json::to_vec(&wire).map_err(SendError::Serialize)?;
        buf.push(b'\n');

        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.pending.lock().await;
            map.insert(id, tx);
        }

        // Write the frame. On failure, evict our pending entry so the
        // reader task doesn't later find a stray sender for an id that
        // never went on the wire.
        {
            let mut guard = self.write_half.lock().await;
            let wh = guard.as_mut().ok_or(SendError::NotConnected)?;
            use tokio::io::AsyncWriteExt;
            if let Err(e) = wh.write_all(&buf).await {
                self.pending.lock().await.remove(&id);
                return Err(SendError::Io(e));
            }
            if let Err(e) = wh.flush().await {
                self.pending.lock().await.remove(&id);
                return Err(SendError::Io(e));
            }
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_closed)) => Err(SendError::ResponseChannelClosed),
            Err(_) => {
                // Timed out — pull our entry back so a late-arriving
                // response isn't matched to a ghost waiter.
                self.pending.lock().await.remove(&id);
                Err(SendError::Timeout {
                    request_id: id,
                    elapsed_ms: timeout.as_millis() as u64,
                })
            }
        }
    }

    /// Send a `Ping` and assert the returned nonce matches. Returns the
    /// round-trip elapsed time as a diagnostic — callers that just want
    /// liveness can `.is_ok()` the result.
    ///
    /// First real transport use of the persistent post-handshake stream.
    /// No sandbox-escape semantics; a successful Pong proves the channel
    /// exists and framing survives a round trip.
    pub async fn send_ping(&self, nonce: u64, timeout: Duration) -> Result<Duration, SendError> {
        let started = Instant::now();
        let resp = self.send_request(Request::Ping { nonce }, timeout).await?;
        if let Some(err) = resp.error {
            return Err(SendError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("ping failed: {} ({})", err.message, err.code),
            )));
        }
        let result = resp.result.unwrap_or(serde_json::Value::Null);
        let echoed = result
            .get("nonce")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                SendError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "ping response missing nonce",
                ))
            })?;
        if echoed != nonce {
            return Err(SendError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("ping nonce mismatch: sent {nonce}, got {echoed}"),
            )));
        }
        Ok(started.elapsed())
    }

    /// Send a `Poke` request to the worker and await its response. The
    /// response semantics match what the worker's `handle_poke_with_id`
    /// produces — this method just plumbs the round-trip. Integration
    /// tests that exercise sandbox-escape paths (`TryExecve`,
    /// `TryReadHostPath`) use this instead of reinventing the wire
    /// dance.
    pub async fn send_poke(
        &self,
        op: PokeOp,
        timeout: Duration,
    ) -> Result<WireResponse, SendError> {
        self.send_request(Request::Poke { op }, timeout).await
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
pub fn peer_creds_from_stream(stream: &tokio::net::UnixStream) -> std::io::Result<PeerCreds> {
    let creds = stream.peer_cred()?;
    Ok(PeerCreds {
        pid: creds.pid().unwrap_or(0) as u32,
        uid: creds.uid(),
        gid: creds.gid(),
    })
}

#[cfg(not(target_os = "linux"))]
pub fn peer_creds_from_stream(_stream: &tokio::net::UnixStream) -> std::io::Result<PeerCreds> {
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
        let (session, rx) =
            BrokerSession::new(id, 1, 1, 1, sample_policy(), PathBuf::from("/tmp/a.sock"));
        session.fire_sandboxed_ready().await;
        assert!(rx.await.is_ok());
        // Second fire is a no-op (logs warning, doesn't panic).
        session.fire_sandboxed_ready().await;
    }
}
