use std::sync::Arc;

use aaos_core::{AgentId, AuditEvent, AuditEventKind, AuditLog, Capability, CoreError, Result};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::message::{McpMessage, McpResponse};

type MessageSender = mpsc::Sender<McpMessage>;
type ResponseSender = mpsc::Sender<McpResponse>;
type CapabilityChecker = Arc<dyn Fn(AgentId, &Capability) -> bool + Send + Sync>;

/// Registration for an agent's message channels.
struct AgentChannels {
    message_tx: MessageSender,
    #[allow(dead_code)]
    response_tx: ResponseSender,
}

/// Routes MCP messages between agents with capability validation.
///
/// Every message passes through the router, which:
/// 1. Validates the sender has the capability to message the recipient
/// 2. Logs the message to the audit trail
/// 3. Delivers it to the recipient's channel
pub struct MessageRouter {
    channels: dashmap::DashMap<AgentId, AgentChannels>,
    audit_log: Arc<dyn AuditLog>,
    capability_checker: CapabilityChecker,
    pending_responses: dashmap::DashMap<Uuid, oneshot::Sender<McpResponse>>,
}

impl MessageRouter {
    pub fn new(
        audit_log: Arc<dyn AuditLog>,
        capability_checker: impl Fn(AgentId, &Capability) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            channels: dashmap::DashMap::new(),
            audit_log,
            capability_checker: Arc::new(capability_checker),
            pending_responses: dashmap::DashMap::new(),
        }
    }

    /// Register an agent's message channels.
    /// Returns receivers for incoming messages and responses.
    pub fn register(
        &self,
        agent_id: AgentId,
    ) -> (mpsc::Receiver<McpMessage>, mpsc::Receiver<McpResponse>) {
        let (msg_tx, msg_rx) = mpsc::channel(64);
        let (resp_tx, resp_rx) = mpsc::channel(64);
        self.channels.insert(
            agent_id,
            AgentChannels {
                message_tx: msg_tx,
                response_tx: resp_tx,
            },
        );
        (msg_rx, resp_rx)
    }

    /// Unregister an agent.
    pub fn unregister(&self, agent_id: &AgentId) {
        self.channels.remove(agent_id);
    }

    /// Route a message from sender to recipient.
    pub async fn route(&self, message: McpMessage) -> Result<()> {
        let sender = message.metadata.sender;
        let recipient = message.metadata.recipient;

        // Check capability: sender must be allowed to message recipient
        let required = Capability::MessageSend {
            target_agents: vec![recipient.to_string()],
        };
        if !(self.capability_checker)(sender, &required) {
            self.audit_log.record(AuditEvent::new(
                sender,
                AuditEventKind::CapabilityDenied {
                    capability: required,
                    reason: format!("agent {sender} cannot message {recipient}"),
                },
            ));
            return Err(CoreError::CapabilityDenied {
                agent_id: sender,
                capability: Capability::MessageSend {
                    target_agents: vec![recipient.to_string()],
                },
                reason: "message send not permitted".into(),
            });
        }

        // Log the send
        self.audit_log.record(AuditEvent::new(
            sender,
            AuditEventKind::MessageSent {
                from: sender,
                to: recipient,
            },
        ));

        // Deliver to recipient
        let channel = self
            .channels
            .get(&recipient)
            .ok_or(CoreError::AgentNotFound(recipient))?;

        channel
            .message_tx
            .send(message)
            .await
            .map_err(|_| CoreError::Ipc(format!("failed to deliver message to {recipient}")))?;

        // Log delivery
        self.audit_log.record(AuditEvent::new(
            recipient,
            AuditEventKind::MessageDelivered {
                from: sender,
                to: recipient,
            },
        ));

        Ok(())
    }

    /// Number of registered agents.
    pub fn agent_count(&self) -> usize {
        self.channels.len()
    }

    pub fn register_pending(&self, trace_id: Uuid, tx: oneshot::Sender<McpResponse>) {
        self.pending_responses.insert(trace_id, tx);
    }

    pub fn respond(&self, trace_id: Uuid, response: McpResponse) -> bool {
        if let Some((_, tx)) = self.pending_responses.remove(&trace_id) {
            tx.send(response).is_ok()
        } else {
            false
        }
    }

    pub fn pending_count(&self) -> usize {
        self.pending_responses.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_core::InMemoryAuditLog;

    fn always_allow(_: AgentId, _: &Capability) -> bool {
        true
    }

    fn always_deny(_: AgentId, _: &Capability) -> bool {
        false
    }

    #[tokio::test]
    async fn route_message_between_agents() {
        let log = Arc::new(InMemoryAuditLog::new());
        let router = MessageRouter::new(log.clone(), always_allow);

        let sender = AgentId::new();
        let recipient = AgentId::new();
        router.register(sender);
        let (mut msg_rx, _resp_rx) = router.register(recipient);

        let msg = McpMessage::new(sender, recipient, "hello", serde_json::json!({}));
        router.route(msg).await.unwrap();

        let received = msg_rx.recv().await.unwrap();
        assert_eq!(received.method, "hello");
        assert!(log.len() >= 2); // sent + delivered
    }

    #[tokio::test]
    async fn route_denied_without_capability() {
        let log = Arc::new(InMemoryAuditLog::new());
        let router = MessageRouter::new(log.clone(), always_deny);

        let sender = AgentId::new();
        let recipient = AgentId::new();
        router.register(sender);
        router.register(recipient);

        let msg = McpMessage::new(sender, recipient, "hello", serde_json::json!({}));
        let result = router.route(msg).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn route_to_nonexistent_agent() {
        let log = Arc::new(InMemoryAuditLog::new());
        let router = MessageRouter::new(log, always_allow);

        let sender = AgentId::new();
        let recipient = AgentId::new();
        router.register(sender);
        // recipient not registered

        let msg = McpMessage::new(sender, recipient, "hello", serde_json::json!({}));
        let result = router.route(msg).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn register_pending_and_respond() {
        let log = Arc::new(InMemoryAuditLog::new());
        let router = MessageRouter::new(log, always_allow);

        let trace_id = Uuid::new_v4();
        let (tx, rx) = tokio::sync::oneshot::channel();
        router.register_pending(trace_id, tx);

        let responder = AgentId::new();
        let response = McpResponse {
            jsonrpc: "2.0".to_string(),
            id: Uuid::new_v4(),
            result: Some(serde_json::json!({"answer": 42})),
            error: None,
            metadata: crate::message::ResponseMetadata {
                responder,
                timestamp: chrono::Utc::now(),
                trace_id,
            },
        };

        assert!(router.respond(trace_id, response.clone()));
        let received = rx.await.unwrap();
        assert_eq!(received.result, Some(serde_json::json!({"answer": 42})));
    }

    #[tokio::test]
    async fn respond_to_nonexistent_returns_false() {
        let log = Arc::new(InMemoryAuditLog::new());
        let router = MessageRouter::new(log, always_allow);

        let trace_id = Uuid::new_v4();
        let responder = AgentId::new();
        let response = McpResponse {
            jsonrpc: "2.0".to_string(),
            id: Uuid::new_v4(),
            result: Some(serde_json::json!({})),
            error: None,
            metadata: crate::message::ResponseMetadata {
                responder,
                timestamp: chrono::Utc::now(),
                trace_id,
            },
        };

        assert!(!router.respond(trace_id, response));
    }

    #[tokio::test]
    async fn respond_after_receiver_dropped_returns_false() {
        let log = Arc::new(InMemoryAuditLog::new());
        let router = MessageRouter::new(log, always_allow);

        let trace_id = Uuid::new_v4();
        let (tx, rx) = tokio::sync::oneshot::channel();
        router.register_pending(trace_id, tx);
        drop(rx);

        let responder = AgentId::new();
        let response = McpResponse {
            jsonrpc: "2.0".to_string(),
            id: Uuid::new_v4(),
            result: Some(serde_json::json!({})),
            error: None,
            metadata: crate::message::ResponseMetadata {
                responder,
                timestamp: chrono::Utc::now(),
                trace_id,
            },
        };

        assert!(!router.respond(trace_id, response));
    }
}
