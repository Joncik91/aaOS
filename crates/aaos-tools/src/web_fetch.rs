use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

use crate::context::InvocationContext;
use crate::tool::Tool;
use aaos_core::{Capability, CoreError, Result, ToolDefinition};

const DEFAULT_MAX_BYTES: usize = 50_000;
const TIMEOUT_SECS: u64 = 30;
const MAX_REDIRECTS: usize = 5;

pub struct WebFetchTool {
    http: Client,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WebFetchTool {
    pub fn new() -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
            .build()
            .expect("failed to build HTTP client");
        Self { http }
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_fetch".to_string(),
            description: "Fetch a URL via HTTP GET. Returns status, content type, and body text."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "URL to fetch" },
                    "max_bytes": { "type": "integer", "description": "Max response body size in bytes (default 50000)" }
                },
                "required": ["url"]
            }),
        }
    }

    async fn invoke(&self, input: Value, ctx: &InvocationContext) -> Result<Value> {
        let url = input
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::InvalidManifest("missing 'url' parameter".into()))?;

        let max_bytes = input
            .get("max_bytes")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_MAX_BYTES);

        // Extract host from URL. Reject malformed URLs, non-http(s) schemes,
        // and URLs without a host component.
        let parsed = reqwest::Url::parse(url)
            .map_err(|e| CoreError::InvalidManifest(format!("invalid URL: {e}")))?;
        match parsed.scheme() {
            "http" | "https" => {}
            other => {
                return Err(CoreError::InvalidManifest(format!(
                    "unsupported URL scheme '{other}' — only http/https allowed"
                )));
            }
        }
        let host = parsed
            .host_str()
            .ok_or_else(|| CoreError::InvalidManifest("URL has no host".into()))?
            .to_string();

        // Capability check: the agent must hold NetworkAccess that covers
        // this specific host. Matching is exact on the lowercased host; no
        // wildcard support yet (YAGNI — add when a real role needs it).
        // Normalize through the same extract_host used by the grant parser
        // so `api.example.com` and `https://api.example.com:443` compare
        // equal, and userinfo/port variations don't slip past.
        let requested = Capability::NetworkAccess {
            hosts: vec![aaos_core::extract_host(&host)],
        };
        let allowed = ctx.tokens.iter().any(|h| {
            ctx.capability_registry
                .permits(*h, ctx.agent_id, &requested)
        });
        if !allowed {
            return Err(CoreError::CapabilityDenied {
                agent_id: ctx.agent_id,
                capability: requested,
                reason: format!("web_fetch not permitted for host: {host}"),
            });
        }

        let mut response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| CoreError::Ipc(format!("fetch failed: {e}")))?;

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown")
            .to_string();

        // Early-reject oversized bodies by Content-Length. 10x the cap leaves
        // headroom for servers that over-report, but fails fast on obvious
        // garbage (gigabyte downloads, tarballs) without buffering any of it.
        if let Some(len) = response.content_length() {
            if len > (max_bytes as u64).saturating_mul(10) {
                return Err(CoreError::Ipc(format!(
                    "response body too large: content-length {len} exceeds 10x max_bytes {max_bytes}"
                )));
            }
        }

        // Stream chunks into a Vec capped at max_bytes. Stops reading as soon
        // as the cap is hit so a huge response never gets fully buffered.
        let mut buf: Vec<u8> = Vec::with_capacity(max_bytes.min(64 * 1024));
        let mut truncated = false;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| CoreError::Ipc(format!("failed to read body chunk: {e}")))?
        {
            let remaining = max_bytes.saturating_sub(buf.len());
            if chunk.len() >= remaining {
                buf.extend_from_slice(&chunk[..remaining]);
                truncated = true;
                break;
            }
            buf.extend_from_slice(&chunk);
        }

        let body = String::from_utf8_lossy(&buf).into_owned();

        Ok(json!({
            "status": status,
            "content_type": content_type,
            "body": body,
            "truncated": truncated,
            "bytes_read": buf.len(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_core::{AgentId, CapabilityRegistry, CapabilityToken, Constraints};

    fn dummy_ctx() -> InvocationContext {
        InvocationContext {
            agent_id: AgentId::new(),
            tokens: vec![],
            capability_registry: std::sync::Arc::new(aaos_core::CapabilityRegistry::new()),
        }
    }

    /// Context granting NetworkAccess to the given hosts.
    fn ctx_with_hosts(hosts: &[&str]) -> InvocationContext {
        let agent_id = AgentId::new();
        let registry = std::sync::Arc::new(CapabilityRegistry::new());
        let token = CapabilityToken::issue(
            agent_id,
            Capability::NetworkAccess {
                hosts: hosts.iter().map(|s| s.to_string()).collect(),
            },
            Constraints::default(),
        );
        let handle = registry.insert(agent_id, token);
        InvocationContext {
            agent_id,
            tokens: vec![handle],
            capability_registry: registry,
        }
    }

    #[test]
    fn web_fetch_definition() {
        let tool = WebFetchTool::new();
        let def = tool.definition();
        assert_eq!(def.name, "web_fetch");
    }

    #[tokio::test]
    async fn fetch_missing_url() {
        let tool = WebFetchTool::new();
        let result = tool.invoke(json!({}), &dummy_ctx()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_fetch_without_network_capability() {
        let tool = WebFetchTool::new();
        let result = tool
            .invoke(
                json!({ "url": "https://example.com/" }),
                &dummy_ctx(), // no tokens
            )
            .await
            .unwrap_err();
        assert!(
            matches!(result, CoreError::CapabilityDenied { .. }),
            "expected CapabilityDenied, got: {result}"
        );
    }

    #[tokio::test]
    async fn rejects_host_not_in_grant() {
        let tool = WebFetchTool::new();
        // Granted example.com; requesting attacker.com
        let ctx = ctx_with_hosts(&["example.com"]);
        let result = tool
            .invoke(json!({ "url": "https://attacker.com/steal" }), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(result, CoreError::CapabilityDenied { .. }));
    }

    #[tokio::test]
    async fn rejects_non_http_scheme() {
        let tool = WebFetchTool::new();
        let ctx = ctx_with_hosts(&["example.com"]);
        let result = tool
            .invoke(json!({ "url": "file:///etc/passwd" }), &ctx)
            .await
            .unwrap_err();
        assert!(result.to_string().contains("scheme"), "got: {result}");
    }

    /// Spawn a tiny TCP server that returns `body_bytes` bytes of 'a'
    /// with the given Content-Length. Returns the bound URL.
    async fn spawn_mock_server(body_bytes: usize, advertise_len: Option<usize>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Serve exactly one request then exit.
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut req_buf = [0u8; 1024];
                let _ = sock.read(&mut req_buf).await;
                let headers = match advertise_len {
                    Some(n) => format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {n}\r\n\r\n"
                    ),
                    None => "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\n".to_string(),
                };
                let _ = sock.write_all(headers.as_bytes()).await;
                let body = vec![b'a'; body_bytes];
                let _ = sock.write_all(&body).await;
                let _ = sock.flush().await;
                let _ = sock.shutdown().await;
            }
        });
        format!("http://{addr}/")
    }

    #[tokio::test]
    async fn truncates_body_at_max_bytes() {
        // Body 10x cap is the boundary — still allowed by content-length
        // reject (>10x rejects), but exceeds max_bytes so streaming truncates.
        let url = spawn_mock_server(100_000, Some(100_000)).await;
        let tool = WebFetchTool::new();
        let result = tool
            .invoke(
                json!({ "url": url, "max_bytes": 10_000 }),
                &ctx_with_hosts(&["127.0.0.1"]),
            )
            .await
            .unwrap();
        assert_eq!(result["truncated"], true);
        assert_eq!(result["bytes_read"], 10_000);
        assert_eq!(result["body"].as_str().unwrap().len(), 10_000);
    }

    #[tokio::test]
    async fn rejects_content_length_over_10x_cap() {
        // 600 KB advertised, cap 50 KB → 12x, must reject before streaming.
        let url = spawn_mock_server(1024, Some(600_000)).await;
        let tool = WebFetchTool::new();
        let err = tool
            .invoke(
                json!({ "url": url, "max_bytes": 50_000 }),
                &ctx_with_hosts(&["127.0.0.1"]),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("too large"), "got: {err}");
    }

    #[tokio::test]
    async fn small_body_under_cap_not_truncated() {
        let url = spawn_mock_server(500, Some(500)).await;
        let tool = WebFetchTool::new();
        let result = tool
            .invoke(
                json!({ "url": url, "max_bytes": 50_000 }),
                &ctx_with_hosts(&["127.0.0.1"]),
            )
            .await
            .unwrap();
        assert_eq!(result["truncated"], false);
        assert_eq!(result["bytes_read"], 500);
    }
}
