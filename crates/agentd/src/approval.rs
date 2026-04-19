use aaos_core::{AgentId, ApprovalResult, ApprovalService, CoreError, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::oneshot;
use uuid::Uuid;

pub struct ApprovalQueue {
    pending: DashMap<Uuid, PendingApproval>,
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

        self.pending.insert(
            id,
            PendingApproval {
                id,
                agent_id,
                agent_name,
                description,
                tool,
                input,
                timestamp: Utc::now(),
                response_tx: tx,
            },
        );

        match rx.await {
            Ok(result) => Ok(result),
            Err(_) => {
                self.pending.remove(&id);
                Ok(ApprovalResult::Denied {
                    reason: "approval service unavailable".into(),
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
