use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

use crate::context::InvocationContext;
use crate::tool::Tool;
use aaos_core::{CoreError, Result, ToolDefinition};

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

    async fn invoke(&self, input: Value, _ctx: &InvocationContext) -> Result<Value> {
        let url = input
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::InvalidManifest("missing 'url' parameter".into()))?;

        let max_bytes = input
            .get("max_bytes")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_MAX_BYTES);

        let response = self
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

        let bytes = response
            .bytes()
            .await
            .map_err(|e| CoreError::Ipc(format!("failed to read body: {e}")))?;

        let body = if bytes.len() > max_bytes {
            String::from_utf8_lossy(&bytes[..max_bytes]).into_owned()
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };

        Ok(json!({
            "status": status,
            "content_type": content_type,
            "body": body,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_core::AgentId;

    fn dummy_ctx() -> InvocationContext {
        InvocationContext {
            agent_id: AgentId::new(),
            tokens: vec![],
            capability_registry: std::sync::Arc::new(aaos_core::CapabilityRegistry::new()),
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
}
