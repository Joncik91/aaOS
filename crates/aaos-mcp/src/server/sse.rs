use aaos_core::{AgentId, AuditEvent};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::Stream;
use std::convert::Infallible;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

pub fn audit_sse_stream(
    rx: broadcast::Receiver<AuditEvent>,
    agent_id: AgentId,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        match result {
            Ok(event) if event.agent_id == agent_id => {
                match serde_json::to_string(&event) {
                    Ok(data) => Some(Ok(Event::default().data(data))),
                    Err(e) => {
                        tracing::error!(agent_id = %agent_id, error = %e, "audit event serialisation failed");
                        None
                    }
                }
            }
            _ => None,
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}
