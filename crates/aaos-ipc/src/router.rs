use std::sync::Arc;

use aaos_core::{AgentId, AuditEvent, AuditEventKind, AuditLog, Capability, CoreError, Result};
use tokio::sync::mpsc;

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
}
