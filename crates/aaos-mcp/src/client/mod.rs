pub mod proxy;
pub mod session;
pub mod transport;

use crate::config::{ClientConfig, TransportKind};
use crate::client::proxy::McpToolProxy;
use crate::client::session::{spawn_reconnect_loop, McpSession};
use crate::client::transport::{HttpTransport, McpTransport, StdioTransport};
use aaos_tools::ToolRegistry;
use std::sync::Arc;

pub struct McpClient {
    sessions: Vec<Arc<McpSession>>,
}

impl McpClient {
    /// Connect to all configured servers and register their tools into `registry`.
    /// Errors from individual servers are logged and skipped; others proceed.
    pub async fn connect_and_register(
        config: &ClientConfig,
        registry: &ToolRegistry,
    ) -> Self {
        let mut sessions = Vec::new();

        for server_cfg in &config.servers {
            // Build the initial transport and a factory closure for the reconnect loop.
            // The factory must be 'static + Send, so we capture only the fields we need.
            let (transport, factory): (Arc<dyn McpTransport>, Box<dyn Fn() -> Arc<dyn McpTransport> + Send + 'static>) =
                match &server_cfg.transport {
                    TransportKind::Http => {
                        let url = match &server_cfg.url {
                            Some(u) => u.clone(),
                            None => {
                                tracing::warn!(
                                    server = %server_cfg.name,
                                    "http transport requires `url` — skipping"
                                );
                                continue;
                            }
                        };
                        let url_clone = url.clone();
                        let t: Arc<dyn McpTransport> = Arc::new(HttpTransport::new(url));
                        let f: Box<dyn Fn() -> Arc<dyn McpTransport> + Send + 'static> =
                            Box::new(move || Arc::new(HttpTransport::new(url_clone.clone())));
                        (t, f)
                    }
                    TransportKind::Stdio => {
                        let cmd = match &server_cfg.command {
                            Some(c) if !c.is_empty() => c.clone(),
                            _ => {
                                tracing::warn!(
                                    server = %server_cfg.name,
                                    "stdio transport requires `command` — skipping"
                                );
                                continue;
                            }
                        };
                        let t = match StdioTransport::spawn(cmd.clone()) {
                            Ok(t) => t,
                            Err(e) => {
                                tracing::warn!(
                                    server = %server_cfg.name,
                                    "failed to spawn stdio server: {e} — skipping"
                                );
                                continue;
                            }
                        };
                        let server_name = server_cfg.name.clone();
                        let f: Box<dyn Fn() -> Arc<dyn McpTransport> + Send + 'static> =
                            Box::new(move || match StdioTransport::spawn(cmd.clone()) {
                                Ok(t) => t,
                                Err(e) => {
                                    tracing::warn!(
                                        server = %server_name,
                                        "reconnect: failed to spawn stdio server: {e}"
                                    );
                                    // Return a transport that will immediately fail, causing
                                    // the reconnect loop to retry with backoff.
                                    Arc::new(HttpTransport::new(String::new()))
                                }
                            });
                        (t, f)
                    }
                };

            let session =
                match McpSession::connect(server_cfg.name.clone(), transport).await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(
                            server = %server_cfg.name,
                            "MCP handshake failed: {e} — skipping"
                        );
                        continue;
                    }
                };

            let tools = session.tools().await;
            let registered = tools.len();

            for tool_def in tools {
                let proxy = McpToolProxy::new(
                    &server_cfg.name,
                    tool_def.name,
                    tool_def.description.unwrap_or_default(),
                    tool_def.input_schema,
                    session.clone(),
                );
                registry.register(Arc::new(proxy));
            }

            tracing::info!(
                server = %server_cfg.name,
                tools = registered,
                "MCP server connected"
            );

            sessions.push(session.clone());
            spawn_reconnect_loop(session, factory);
        }

        Self { sessions }
    }

    /// Returns all active sessions. Useful for health checks and diagnostics.
    pub fn sessions(&self) -> &[Arc<McpSession>] {
        &self.sessions
    }
}
