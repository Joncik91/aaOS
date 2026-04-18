use crate::client::transport::McpTransport;
use crate::types::{JsonRpcRequest, McpError, McpToolDefinition};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct McpSession {
    name: String,
    transport: RwLock<Arc<dyn McpTransport>>,
    tools: RwLock<Vec<McpToolDefinition>>,
    healthy: AtomicBool,
    next_id: AtomicU64,
}

impl McpSession {
    /// Connect, run initialize + tools/list handshake. Returns error if either fails.
    pub async fn connect(
        name: String,
        transport: Arc<dyn McpTransport>,
    ) -> Result<Arc<Self>, McpError> {
        let session = Arc::new(Self {
            name,
            transport: RwLock::new(transport),
            tools: RwLock::new(vec![]),
            healthy: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
        });
        session.handshake().await?;
        Ok(session)
    }

    async fn handshake(&self) -> Result<(), McpError> {
        let transport = self.transport.read().await.clone();

        // initialize
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let resp = transport
            .send(JsonRpcRequest::new(
                id,
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "clientInfo": { "name": "aaos", "version": "0.1" },
                    "capabilities": {}
                }),
            ))
            .await?;
        if let Some(err) = resp.error {
            return Err(McpError::Rpc {
                code: err.code,
                message: err.message,
            });
        }

        // tools/list
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let resp = transport
            .send(JsonRpcRequest::new(id, "tools/list", json!({})))
            .await?;
        if let Some(err) = resp.error {
            return Err(McpError::Rpc {
                code: err.code,
                message: err.message,
            });
        }

        let tools: Vec<McpToolDefinition> = resp
            .result
            .and_then(|r| r.get("tools").cloned())
            .and_then(|t| serde_json::from_value(t).ok())
            .unwrap_or_default();

        *self.tools.write().await = tools;
        self.healthy.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub async fn tools(&self) -> Vec<McpToolDefinition> {
        self.tools.read().await.clone()
    }

    /// Send a `tools/call` request to the remote MCP server.
    pub async fn call(
        &self,
        remote_name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        if !self.is_healthy() {
            return Err(McpError::Unhealthy);
        }
        let transport = self.transport.read().await.clone();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let resp = transport
            .send(JsonRpcRequest::new(
                id,
                "tools/call",
                json!({ "name": remote_name, "arguments": arguments }),
            ))
            .await
            .map_err(|e| {
                self.healthy.store(false, Ordering::Relaxed);
                e
            })?;
        if let Some(err) = resp.error {
            return Err(McpError::Rpc {
                code: err.code,
                message: err.message,
            });
        }
        Ok(resp.result.unwrap_or(serde_json::Value::Null))
    }

    /// Mark session unhealthy (called by reconnect loop on transport error).
    pub fn mark_unhealthy(&self) {
        self.healthy.store(false, Ordering::Relaxed);
    }

    /// Re-run the handshake with a new transport (used by reconnect loop).
    pub async fn handshake_with(&self, transport: Arc<dyn McpTransport>) -> Result<(), McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let resp = transport
            .send(JsonRpcRequest::new(
                id,
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "clientInfo": { "name": "aaos", "version": "0.1" },
                    "capabilities": {}
                }),
            ))
            .await?;
        if let Some(err) = resp.error {
            return Err(McpError::Rpc {
                code: err.code,
                message: err.message,
            });
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let resp = transport
            .send(JsonRpcRequest::new(id, "tools/list", json!({})))
            .await?;
        if let Some(err) = resp.error {
            return Err(McpError::Rpc {
                code: err.code,
                message: err.message,
            });
        }

        let tools: Vec<McpToolDefinition> = resp
            .result
            .and_then(|r| r.get("tools").cloned())
            .and_then(|t| serde_json::from_value(t).ok())
            .unwrap_or_default();

        *self.tools.write().await = tools;
        *self.transport.write().await = transport;
        self.healthy.store(true, Ordering::Relaxed);
        Ok(())
    }
}

/// Spawn a background task that reconnects `session` with exponential backoff
/// when it becomes unhealthy. The task runs until the Arc is dropped.
pub fn spawn_reconnect_loop(
    session: Arc<McpSession>,
    transport_factory: impl Fn() -> Arc<dyn McpTransport> + Send + 'static,
) {
    tokio::spawn(async move {
        let mut backoff_ms = 1_000u64;
        loop {
            if session.is_healthy() {
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                continue;
            }
            tracing::warn!(server = %session.name(), "MCP session unhealthy, reconnecting in {}ms", backoff_ms);
            tokio::time::sleep(tokio::time::Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(30_000);

            let transport = transport_factory();
            match session.handshake_with(transport).await {
                Ok(()) => {
                    backoff_ms = 1_000;
                    tracing::info!(server = %session.name(), "MCP session reconnected");
                }
                Err(e) => {
                    tracing::warn!(server = %session.name(), "reconnect failed: {e}");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::JsonRpcResponse;
    use serde_json::json;
    use std::sync::{Arc, Mutex as StdMutex};

    struct MockTransport {
        responses: StdMutex<Vec<JsonRpcResponse>>,
    }

    impl MockTransport {
        fn new(responses: Vec<JsonRpcResponse>) -> Arc<Self> {
            Arc::new(Self {
                responses: StdMutex::new(responses),
            })
        }
    }

    #[async_trait::async_trait]
    impl crate::client::transport::McpTransport for MockTransport {
        async fn send(
            &self,
            _req: crate::types::JsonRpcRequest,
        ) -> Result<JsonRpcResponse, crate::types::McpError> {
            let mut q = self.responses.lock().unwrap();
            if q.is_empty() {
                return Err(crate::types::McpError::Transport(
                    "no more responses".into(),
                ));
            }
            Ok(q.remove(0))
        }
        async fn close(&self) {}
    }

    #[tokio::test]
    async fn session_initializes_and_lists_tools() {
        let transport = MockTransport::new(vec![
            JsonRpcResponse::success(
                1,
                json!({ "protocolVersion": "2024-11-05", "capabilities": {} }),
            ),
            JsonRpcResponse::success(
                2,
                json!({
                    "tools": [{
                        "name": "echo",
                        "description": "echoes input",
                        "inputSchema": { "type": "object" }
                    }]
                }),
            ),
        ]);

        let session = McpSession::connect("test".into(), transport).await.unwrap();
        assert!(session.is_healthy());
        let tools = session.tools().await;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
    }

    #[tokio::test]
    async fn session_unhealthy_on_transport_error() {
        let transport = MockTransport::new(vec![JsonRpcResponse::error(1, -32000, "server error")]);
        let result = McpSession::connect("bad".into(), transport).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn call_marks_unhealthy_on_transport_error() {
        let transport = MockTransport::new(vec![
            // initialize
            JsonRpcResponse::success(1, json!({ "capabilities": {} })),
            // tools/list
            JsonRpcResponse::success(2, json!({ "tools": [] })),
            // tools/call — error (simulates server crash via "no more responses")
        ]);

        let session = McpSession::connect("test".into(), transport).await.unwrap();
        assert!(session.is_healthy());

        // Next call will get "no more responses" transport error
        let result = session.call("echo", json!({})).await;
        assert!(result.is_err());
        assert!(!session.is_healthy());
    }
}
