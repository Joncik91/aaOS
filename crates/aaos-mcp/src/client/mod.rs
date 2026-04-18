pub mod proxy;
pub mod session;
pub mod transport;

use crate::config::{ClientConfig, TransportKind};
use crate::client::proxy::McpToolProxy;
use crate::client::session::McpSession;
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
            let transport: Arc<dyn McpTransport> = match &server_cfg.transport {
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
                    Arc::new(HttpTransport::new(url))
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
                    match StdioTransport::spawn(cmd) {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::warn!(
                                server = %server_cfg.name,
                                "failed to spawn stdio server: {e} — skipping"
                            );
                            continue;
                        }
                    }
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
            sessions.push(session);
        }

        Self { sessions }
    }
}
