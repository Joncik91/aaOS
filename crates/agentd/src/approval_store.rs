//! SQLite-backed persistence for the approval queue.
//!
//! Without this, [`ApprovalQueue`](crate::approval::ApprovalQueue) is purely
//! in-memory: a daemon restart loses every pending approval, and the agents
//! waiting on those approvals see `Denied(timeout)` after one hour. With this,
//! pending approvals survive restart — the daemon reloads them on startup,
//! computes the remaining timeout from the original `timestamp`, and re-arms
//! the timer.
//!
//! Schema mirrors [`PendingApproval`](crate::approval::PendingApproval) but
//! stores the JSON `input` as TEXT and serializes the rest verbatim. The
//! oneshot sender is **not** persisted; on reload the daemon reconstructs a
//! fresh `PendingApproval` whose sender side is held by the requesting agent's
//! reload-time wrapper. The reload caller is responsible for re-arming the
//! timeout against the loaded timestamp.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use aaos_core::AgentId;

/// Persisted snapshot of a pending approval. The runtime
/// [`PendingApproval`](crate::approval::PendingApproval) holds an extra
/// oneshot-sender field that has no on-disk representation.
#[derive(Debug, Clone)]
pub struct PersistedApproval {
    pub id: Uuid,
    pub agent_id: AgentId,
    pub agent_name: String,
    pub description: String,
    pub tool: Option<String>,
    pub input: Option<Value>,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum ApprovalStoreError {
    #[error("approval store I/O failed: {0}")]
    Io(String),
    #[error("approval store schema error: {0}")]
    Schema(String),
    #[error("approval store serialization failed: {0}")]
    Serde(String),
}

pub type ApprovalStoreResult<T> = std::result::Result<T, ApprovalStoreError>;

/// SQLite-backed approval persistence. One connection guarded by `Mutex`,
/// matching the pattern in `aaos-memory::SqliteMemoryStore`.
pub struct ApprovalStore {
    conn: Mutex<Connection>,
}

impl ApprovalStore {
    /// Open or create the database at `path`. Default production path:
    /// `/var/lib/aaos/approvals.db` (parallel to `memory.db`).
    pub fn open(path: &Path) -> ApprovalStoreResult<Self> {
        let conn = Connection::open(path)
            .map_err(|e| ApprovalStoreError::Io(format!("open failed: {e}")))?;
        Self::init(conn)
    }

    /// Open an in-memory database (for tests).
    pub fn in_memory() -> ApprovalStoreResult<Self> {
        let conn = Connection::open_in_memory()
            .map_err(|e| ApprovalStoreError::Io(format!("open in-memory failed: {e}")))?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> ApprovalStoreResult<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS approvals (
                id          TEXT PRIMARY KEY,
                agent_id    TEXT NOT NULL,
                agent_name  TEXT NOT NULL,
                description TEXT NOT NULL,
                tool        TEXT,
                input_json  TEXT,
                timestamp   TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_approvals_timestamp ON approvals(timestamp);",
        )
        .map_err(|e| ApprovalStoreError::Schema(format!("init failed: {e}")))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Insert a pending approval. Idempotent on `id` — a second insert with
    /// the same id replaces the row.
    pub fn insert(&self, approval: &PersistedApproval) -> ApprovalStoreResult<()> {
        let input_json = match &approval.input {
            Some(v) => Some(
                serde_json::to_string(v)
                    .map_err(|e| ApprovalStoreError::Serde(format!("input: {e}")))?,
            ),
            None => None,
        };
        let conn = self
            .conn
            .lock()
            .expect("approval store mutex poisoned by panic in another thread");
        conn.execute(
            "INSERT OR REPLACE INTO approvals
                (id, agent_id, agent_name, description, tool, input_json, timestamp)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                approval.id.to_string(),
                approval.agent_id.to_string(),
                approval.agent_name,
                approval.description,
                approval.tool,
                input_json,
                approval.timestamp.to_rfc3339(),
            ],
        )
        .map_err(|e| ApprovalStoreError::Io(format!("insert: {e}")))?;
        Ok(())
    }

    /// Remove a pending approval by id. No error if no row matches.
    pub fn remove(&self, id: Uuid) -> ApprovalStoreResult<()> {
        let conn = self
            .conn
            .lock()
            .expect("approval store mutex poisoned by panic in another thread");
        conn.execute(
            "DELETE FROM approvals WHERE id = ?",
            params![id.to_string()],
        )
        .map_err(|e| ApprovalStoreError::Io(format!("remove: {e}")))?;
        Ok(())
    }

    /// Load every pending approval. Used at daemon startup.
    pub fn list_pending(&self) -> ApprovalStoreResult<Vec<PersistedApproval>> {
        let conn = self
            .conn
            .lock()
            .expect("approval store mutex poisoned by panic in another thread");
        let mut stmt = conn
            .prepare(
                "SELECT id, agent_id, agent_name, description, tool, input_json, timestamp
                 FROM approvals",
            )
            .map_err(|e| ApprovalStoreError::Schema(format!("prepare: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                let id_str: String = row.get(0)?;
                let agent_id_str: String = row.get(1)?;
                let agent_name: String = row.get(2)?;
                let description: String = row.get(3)?;
                let tool: Option<String> = row.get(4)?;
                let input_json: Option<String> = row.get(5)?;
                let timestamp_str: String = row.get(6)?;
                Ok((
                    id_str,
                    agent_id_str,
                    agent_name,
                    description,
                    tool,
                    input_json,
                    timestamp_str,
                ))
            })
            .map_err(|e| ApprovalStoreError::Io(format!("query: {e}")))?;

        let mut out = Vec::new();
        for row in rows {
            let (id_str, agent_id_str, agent_name, description, tool, input_json, ts_str) =
                row.map_err(|e| ApprovalStoreError::Io(format!("row: {e}")))?;
            let id = Uuid::parse_str(&id_str)
                .map_err(|e| ApprovalStoreError::Serde(format!("id parse: {e}")))?;
            let agent_id = AgentId::from_uuid(
                Uuid::parse_str(&agent_id_str)
                    .map_err(|e| ApprovalStoreError::Serde(format!("agent_id parse: {e}")))?,
            );
            let input = match input_json {
                Some(s) => Some(
                    serde_json::from_str::<Value>(&s)
                        .map_err(|e| ApprovalStoreError::Serde(format!("input parse: {e}")))?,
                ),
                None => None,
            };
            let timestamp = DateTime::parse_from_rfc3339(&ts_str)
                .map_err(|e| ApprovalStoreError::Serde(format!("timestamp parse: {e}")))?
                .with_timezone(&Utc);
            out.push(PersistedApproval {
                id,
                agent_id,
                agent_name,
                description,
                tool,
                input,
                timestamp,
            });
        }
        Ok(out)
    }

    /// Drop pending approvals older than `max_age`. Called at daemon startup
    /// after `list_pending` so callers can record audit events for the
    /// timeouts before they're removed.
    pub fn purge_older_than(&self, max_age: Duration) -> ApprovalStoreResult<usize> {
        let cutoff = Utc::now()
            - chrono::Duration::from_std(max_age).map_err(|e| {
                ApprovalStoreError::Serde(format!("max_age out of chrono Duration range: {e}"))
            })?;
        let conn = self
            .conn
            .lock()
            .expect("approval store mutex poisoned by panic in another thread");
        let n = conn
            .execute(
                "DELETE FROM approvals WHERE timestamp < ?",
                params![cutoff.to_rfc3339()],
            )
            .map_err(|e| ApprovalStoreError::Io(format!("purge: {e}")))?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample(id: Uuid) -> PersistedApproval {
        PersistedApproval {
            id,
            agent_id: AgentId::new(),
            agent_name: "test-agent".into(),
            description: "approve writing /tmp/x".into(),
            tool: Some("file_write".into()),
            input: Some(json!({"path": "/tmp/x", "content": "hi"})),
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn insert_and_list() {
        let store = ApprovalStore::in_memory().unwrap();
        let id = Uuid::new_v4();
        let a = sample(id);
        store.insert(&a).unwrap();

        let pending = store.list_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, id);
        assert_eq!(pending[0].agent_name, "test-agent");
        assert_eq!(pending[0].tool.as_deref(), Some("file_write"));
        assert_eq!(
            pending[0].input.as_ref().and_then(|v| v.get("path")),
            Some(&json!("/tmp/x"))
        );
    }

    #[test]
    fn remove_clears_row() {
        let store = ApprovalStore::in_memory().unwrap();
        let id = Uuid::new_v4();
        store.insert(&sample(id)).unwrap();
        store.remove(id).unwrap();
        assert!(store.list_pending().unwrap().is_empty());
    }

    #[test]
    fn replace_on_duplicate_id() {
        let store = ApprovalStore::in_memory().unwrap();
        let id = Uuid::new_v4();
        let mut a = sample(id);
        store.insert(&a).unwrap();
        a.description = "replaced".into();
        store.insert(&a).unwrap();
        let pending = store.list_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].description, "replaced");
    }

    #[test]
    fn purge_older_than_drops_stale_rows() {
        let store = ApprovalStore::in_memory().unwrap();
        // Old row: timestamp 2 hours ago.
        let mut old = sample(Uuid::new_v4());
        old.timestamp = Utc::now() - chrono::Duration::hours(2);
        store.insert(&old).unwrap();
        // Fresh row.
        let fresh = sample(Uuid::new_v4());
        store.insert(&fresh).unwrap();

        let purged = store.purge_older_than(Duration::from_secs(3600)).unwrap();
        assert_eq!(purged, 1);
        let pending = store.list_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, fresh.id);
    }

    #[test]
    fn open_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");

        let id = Uuid::new_v4();
        {
            let store = ApprovalStore::open(&path).unwrap();
            store.insert(&sample(id)).unwrap();
        }
        let store = ApprovalStore::open(&path).unwrap();
        let pending = store.list_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, id);
    }
}
