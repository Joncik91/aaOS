use crate::types::{JsonRpcRequest, JsonRpcResponse, McpError};
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

#[async_trait]
pub trait McpTransport: Send + Sync {
    async fn send(&self, req: JsonRpcRequest) -> Result<JsonRpcResponse, McpError>;
    async fn close(&self);
}

// ── HTTP transport ─────────────────────────────────────────────────────────

pub struct HttpTransport {
    pub base_url: String,
    client: reqwest::Client,
}

impl HttpTransport {
    pub fn new(base_url: String) -> Self {
        Self { base_url, client: reqwest::Client::new() }
    }
}

#[async_trait]
impl McpTransport for HttpTransport {
    async fn send(&self, req: JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
        let url = format!("{}/mcp", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&req)
            .send()
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?
            .json::<JsonRpcResponse>()
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;
        Ok(resp)
    }

    async fn close(&self) {}
}

// ── stdio transport ────────────────────────────────────────────────────────

struct StdioInner {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

pub struct StdioTransport {
    inner: Mutex<Option<StdioInner>>,
    command: Vec<String>,
    closed: AtomicBool,
}

impl StdioTransport {
    pub fn spawn(command: Vec<String>) -> Result<Arc<Self>, McpError> {
        let mut cmd = tokio::process::Command::new(&command[0]);
        cmd.args(&command[1..]);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::null());

        let mut child = cmd
            .spawn()
            .map_err(|e| McpError::Transport(format!("spawn failed: {e}")))?;

        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());

        Ok(Arc::new(Self {
            inner: Mutex::new(Some(StdioInner { child, stdin, stdout })),
            command,
            closed: AtomicBool::new(false),
        }))
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn send(&self, req: JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(McpError::Transport("transport closed".into()));
        }
        let mut guard = self.inner.lock().await;
        let inner = guard.as_mut().ok_or_else(|| McpError::Transport("closed".into()))?;

        let mut line = serde_json::to_string(&req).map_err(McpError::Json)?;
        line.push('\n');
        inner
            .stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;

        let mut response_line = String::new();
        inner
            .stdout
            .read_line(&mut response_line)
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;

        if response_line.is_empty() {
            return Err(McpError::Transport("server closed stdout".into()));
        }

        serde_json::from_str(&response_line).map_err(McpError::Json)
    }

    async fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let mut guard = self.inner.lock().await;
        if let Some(mut inner) = guard.take() {
            let _ = inner.child.kill().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_transport_constructs() {
        let t = HttpTransport::new("http://localhost:9999".into());
        assert_eq!(t.base_url, "http://localhost:9999");
    }
}
