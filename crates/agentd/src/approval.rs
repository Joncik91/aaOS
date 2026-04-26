use aaos_core::{AgentId, ApprovalResult, ApprovalService, CoreError, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::approval_store::{ApprovalStore, PersistedApproval};

/// Default timeout for an approval request.  After this elapses with no
/// human response, the request is removed from the queue and the
/// blocking `request()` call returns `ApprovalResult::Denied` with a
/// timeout reason.  Bug 23 fix: previously requests blocked forever on
/// `rx.await` with no upper bound — orphaned pending entries
/// accumulated indefinitely on operator absence.
///
/// 1 hour is long enough for a typical operator to notice the request
/// and respond, short enough that genuinely-abandoned requests don't
/// leak resources.  Override per-request via `request_with_timeout` if
/// needed; the basic `request()` always uses this default.
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(3600);

pub struct ApprovalQueue {
    pending: DashMap<Uuid, PendingApproval>,
    /// Optional persistence layer. When `Some`, every insert/remove writes
    /// through to disk so pending approvals survive a daemon restart.
    /// `None` is the legacy in-memory-only path used by tests and
    /// non-persistent constructors.
    store: Option<Arc<ApprovalStore>>,
}

struct PendingApproval {
    pub id: Uuid,
    pub agent_id: AgentId,
    pub agent_name: String,
    pub description: String,
    pub tool: Option<String>,
    pub input: Option<Value>,
    pub timestamp: DateTime<Utc>,
    response_tx: oneshot::Sender<ApprovalResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApprovalInfo {
    pub id: Uuid,
    pub agent_id: AgentId,
    pub agent_name: String,
    pub description: String,
    pub tool: Option<String>,
    pub input: Option<Value>,
    pub timestamp: DateTime<Utc>,
}

impl Default for ApprovalQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalQueue {
    pub fn new() -> Self {
        Self {
            pending: DashMap::new(),
            store: None,
        }
    }

    /// Production constructor — every pending approval is written through to
    /// `store` so a daemon restart can reload them.
    pub fn with_store(store: Arc<ApprovalStore>) -> Self {
        Self {
            pending: DashMap::new(),
            store: Some(store),
        }
    }

    pub fn list(&self) -> Vec<ApprovalInfo> {
        self.pending
            .iter()
            .map(|entry| {
                let p = entry.value();
                ApprovalInfo {
                    id: p.id,
                    agent_id: p.agent_id,
                    agent_name: p.agent_name.clone(),
                    description: p.description.clone(),
                    tool: p.tool.clone(),
                    input: p.input.clone(),
                    timestamp: p.timestamp,
                }
            })
            .collect()
    }

    pub fn respond(&self, id: Uuid, decision: ApprovalResult) -> Result<()> {
        match self.pending.remove(&id) {
            Some((_, pending)) => {
                if let Some(store) = &self.store {
                    if let Err(e) = store.remove(id) {
                        tracing::warn!(
                            approval_id = %id,
                            error = %e,
                            "approval store remove failed; persistence may be stale"
                        );
                    }
                }
                let _ = pending.response_tx.send(decision);
                Ok(())
            }
            None => Err(CoreError::Ipc(format!("no pending approval with id {id}"))),
        }
    }
}

#[async_trait]
impl ApprovalService for ApprovalQueue {
    async fn request(
        &self,
        agent_id: AgentId,
        agent_name: String,
        description: String,
        tool: Option<String>,
        input: Option<Value>,
    ) -> Result<ApprovalResult> {
        let id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();

        tracing::info!(
            approval_id = %id,
            agent = %agent_name,
            tool = ?tool,
            "approval requested — waiting for human response"
        );

        let timestamp = Utc::now();
        self.pending.insert(
            id,
            PendingApproval {
                id,
                agent_id,
                agent_name: agent_name.clone(),
                description: description.clone(),
                tool: tool.clone(),
                input: input.clone(),
                timestamp,
                response_tx: tx,
            },
        );

        if let Some(store) = &self.store {
            let persisted = PersistedApproval {
                id,
                agent_id,
                agent_name,
                description,
                tool,
                input,
                timestamp,
            };
            if let Err(e) = store.insert(&persisted) {
                tracing::warn!(
                    approval_id = %id,
                    error = %e,
                    "approval store insert failed; restart will lose this entry"
                );
            }
        }

        match tokio::time::timeout(DEFAULT_APPROVAL_TIMEOUT, rx).await {
            Ok(Ok(result)) => {
                if let Some(store) = &self.store {
                    if let Err(e) = store.remove(id) {
                        // Bug 43 fix (v0.2.8): orphan rows in the SQLite
                        // approvals table accumulate if remove fails
                        // silently.  Log so operators can detect the
                        // failing store and fix it before the table
                        // grows unbounded.
                        tracing::warn!(
                            approval_id = %id,
                            error = %e,
                            "approval store remove failed; orphan row leaked"
                        );
                    }
                }
                Ok(result)
            }
            Ok(Err(_)) => {
                self.pending.remove(&id);
                if let Some(store) = &self.store {
                    if let Err(e) = store.remove(id) {
                        // Bug 43 fix (v0.2.8): orphan rows in the SQLite
                        // approvals table accumulate if remove fails
                        // silently.  Log so operators can detect the
                        // failing store and fix it before the table
                        // grows unbounded.
                        tracing::warn!(
                            approval_id = %id,
                            error = %e,
                            "approval store remove failed; orphan row leaked"
                        );
                    }
                }
                Ok(ApprovalResult::Denied {
                    reason: "approval service unavailable".into(),
                })
            }
            Err(_elapsed) => {
                // Bug 23: request timed out with no human response.
                // Remove the orphaned pending entry and deny the call;
                // otherwise the agent blocks forever and the entry
                // leaks across daemon lifetime.
                self.pending.remove(&id);
                if let Some(store) = &self.store {
                    if let Err(e) = store.remove(id) {
                        // Bug 43 fix (v0.2.8): orphan rows in the SQLite
                        // approvals table accumulate if remove fails
                        // silently.  Log so operators can detect the
                        // failing store and fix it before the table
                        // grows unbounded.
                        tracing::warn!(
                            approval_id = %id,
                            error = %e,
                            "approval store remove failed; orphan row leaked"
                        );
                    }
                }
                tracing::warn!(
                    approval_id = %id,
                    timeout_secs = DEFAULT_APPROVAL_TIMEOUT.as_secs(),
                    "approval request timed out — denying"
                );
                Ok(ApprovalResult::Denied {
                    reason: format!(
                        "approval request timed out after {}s",
                        DEFAULT_APPROVAL_TIMEOUT.as_secs()
                    ),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn approval_flow() {
        let queue = Arc::new(ApprovalQueue::new());
        let queue_clone = queue.clone();

        let agent_id = AgentId::new();
        let handle = tokio::spawn(async move {
            queue_clone
                .request(
                    agent_id,
                    "test-agent".into(),
                    "test".into(),
                    Some("file_write".into()),
                    None,
                )
                .await
        });

        // Wait a moment for the request to be inserted
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Should be one pending
        let pending = queue.list();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].agent_name, "test-agent");
        assert_eq!(pending[0].tool, Some("file_write".into()));

        // Approve
        queue
            .respond(pending[0].id, ApprovalResult::Approved)
            .unwrap();

        let result = handle.await.unwrap().unwrap();
        assert_eq!(result, ApprovalResult::Approved);

        // Pending should be empty
        assert!(queue.list().is_empty());
    }

    #[tokio::test]
    async fn approval_denied() {
        let queue = Arc::new(ApprovalQueue::new());
        let queue_clone = queue.clone();

        let handle = tokio::spawn(async move {
            queue_clone
                .request(AgentId::new(), "agent".into(), "test".into(), None, None)
                .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let pending = queue.list();
        queue
            .respond(
                pending[0].id,
                ApprovalResult::Denied {
                    reason: "no".into(),
                },
            )
            .unwrap();

        let result = handle.await.unwrap().unwrap();
        assert!(matches!(result, ApprovalResult::Denied { .. }));
    }

    #[tokio::test]
    async fn respond_nonexistent() {
        let queue = ApprovalQueue::new();
        let result = queue.respond(Uuid::new_v4(), ApprovalResult::Approved);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn pending_approval_persists_through_store() {
        let store = Arc::new(ApprovalStore::in_memory().unwrap());
        let queue = Arc::new(ApprovalQueue::with_store(store.clone()));
        let queue_clone = queue.clone();

        let agent_id = AgentId::new();
        let handle = tokio::spawn(async move {
            queue_clone
                .request(
                    agent_id,
                    "persistent-agent".into(),
                    "approve write".into(),
                    Some("file_write".into()),
                    Some(serde_json::json!({"path": "/tmp/x"})),
                )
                .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Persistence path: store reflects the in-flight approval.
        let persisted = store.list_pending().unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].agent_name, "persistent-agent");
        assert_eq!(persisted[0].tool.as_deref(), Some("file_write"));

        // Approve and assert removal flushes through to the store.
        let pending = queue.list();
        queue
            .respond(pending[0].id, ApprovalResult::Approved)
            .unwrap();
        let _ = handle.await.unwrap().unwrap();
        assert!(store.list_pending().unwrap().is_empty());
    }

    #[tokio::test]
    async fn sender_dropped_returns_denied() {
        let queue = Arc::new(ApprovalQueue::new());
        let queue_clone = queue.clone();

        let handle = tokio::spawn(async move {
            queue_clone
                .request(AgentId::new(), "agent".into(), "test".into(), None, None)
                .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Remove the pending entry without responding — this drops the sender
        let pending = queue.list();
        assert_eq!(pending.len(), 1);
        queue.pending.remove(&pending[0].id);

        let result = handle.await.unwrap().unwrap();
        assert!(matches!(result, ApprovalResult::Denied { .. }));
    }
}
