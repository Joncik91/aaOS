use std::sync::Arc;
use tokio::sync::broadcast;

use aaos_core::{AuditEvent, AuditLog};

/// Fan-out audit log. Forwards every recorded event to an inner sink AND
/// to any tokio broadcast subscribers. Used by the streaming JSON-RPC
/// methods (`agent.submit_streaming`, `agent.logs_streaming`) to receive
/// live events without touching the existing stdout/in-memory sinks.
pub struct BroadcastAuditLog {
    inner: Arc<dyn AuditLog>,
    tx: broadcast::Sender<AuditEvent>,
}

impl BroadcastAuditLog {
    /// Create a new broadcast wrapper. `capacity` is the per-subscriber
    /// ring-buffer size; slow subscribers beyond this will see `RecvError::Lagged`.
    pub fn new(inner: Arc<dyn AuditLog>, capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { inner, tx }
    }

    /// Subscribe to future audit events. The receiver only sees events
    /// recorded after `subscribe()` returns — past events stay with the inner
    /// sink (e.g. InMemoryAuditLog) and are queried separately.
    pub fn subscribe(&self) -> broadcast::Receiver<AuditEvent> {
        self.tx.subscribe()
    }
}

impl AuditLog for BroadcastAuditLog {
    fn record(&self, event: AuditEvent) {
        // Clone for the broadcast; inner takes the original.
        let for_broadcast = event.clone();
        self.inner.record(event);
        // send() errors only when there are zero subscribers — that's fine.
        let _ = self.tx.send(for_broadcast);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_core::{AgentId, AuditEvent, AuditEventKind, InMemoryAuditLog};

    fn make_spawned(name: &str) -> AuditEvent {
        AuditEvent::new(
            AgentId::new(),
            AuditEventKind::AgentSpawned {
                manifest_name: name.into(),
            },
        )
    }

    #[tokio::test]
    async fn broadcast_forwards_to_inner_and_subscribers() {
        let inner = Arc::new(InMemoryAuditLog::new());
        let log = BroadcastAuditLog::new(inner.clone(), 16);
        let mut rx = log.subscribe();

        log.record(make_spawned("fetcher"));

        let received = rx.recv().await.expect("subscriber receives event");
        assert!(matches!(
            received.event,
            AuditEventKind::AgentSpawned { .. }
        ));
        assert_eq!(inner.len(), 1, "inner sink also got the event");
    }

    #[tokio::test]
    async fn late_subscriber_does_not_see_past_events() {
        let inner = Arc::new(InMemoryAuditLog::new());
        let log = BroadcastAuditLog::new(inner.clone(), 16);

        log.record(make_spawned("a"));
        let mut rx = log.subscribe();
        log.record(make_spawned("b"));

        let received = rx.recv().await.expect("subscriber receives post-subscribe event");
        match received.event {
            AuditEventKind::AgentSpawned { manifest_name } => {
                assert_eq!(manifest_name, "b");
            }
            other => panic!("expected AgentSpawned, got {:?}", other),
        }
        assert_eq!(
            inner.len(),
            2,
            "inner sink has both events; subscriber only the post-subscribe one"
        );
    }

    #[tokio::test]
    async fn record_succeeds_with_no_subscribers() {
        let inner = Arc::new(InMemoryAuditLog::new());
        let log = BroadcastAuditLog::new(inner.clone(), 16);
        // No subscribe() call here. record must still succeed and reach inner.
        log.record(make_spawned("x"));
        assert_eq!(inner.len(), 1);
    }

    #[tokio::test]
    async fn multiple_subscribers_each_receive_event() {
        let inner = Arc::new(InMemoryAuditLog::new());
        let log = BroadcastAuditLog::new(inner.clone(), 16);
        let mut rx_a = log.subscribe();
        let mut rx_b = log.subscribe();

        log.record(make_spawned("evt"));

        let a = rx_a.recv().await.expect("a receives");
        let b = rx_b.recv().await.expect("b receives");
        assert!(matches!(a.event, AuditEventKind::AgentSpawned { .. }));
        assert!(matches!(b.event, AuditEventKind::AgentSpawned { .. }));
    }
}
