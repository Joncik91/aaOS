//! Integration tests for McpServer. Starts a real axum listener, sends
//! HTTP requests, asserts responses.

use aaos_core::{AgentId, AuditEvent};
use aaos_mcp::server::{McpServerBackend, RunStatus};
use async_trait::async_trait;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

struct MockBackend {
    submitted: Mutex<Vec<String>>,
    tx: broadcast::Sender<AuditEvent>,
}

impl MockBackend {
    fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(16);
        Arc::new(Self {
            submitted: Mutex::new(vec![]),
            tx,
        })
    }
}

#[async_trait]
impl McpServerBackend for MockBackend {
    async fn submit_goal(&self, goal: String, _role: Option<String>) -> anyhow::Result<AgentId> {
        self.submitted.lock().unwrap().push(goal);
        Ok(AgentId::new())
    }

    fn run_status(&self, _agent_id: &AgentId) -> RunStatus {
        RunStatus::Running
    }

    async fn cancel(&self, _agent_id: &AgentId) -> bool {
        true
    }

    fn subscribe_audit(&self) -> broadcast::Receiver<AuditEvent> {
        self.tx.subscribe()
    }
}

async fn start_server(backend: Arc<dyn McpServerBackend>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let addr = format!("127.0.0.1:{port}");

    use axum::{
        routing::{get, post},
        Router,
    };
    let app = Router::new()
        .route("/mcp", post(aaos_mcp::server::handlers::handle_jsonrpc))
        .route("/mcp/events", get(aaos_mcp::server::handlers::handle_sse))
        .with_state(backend);

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    addr
}

#[tokio::test]
async fn initialize_returns_server_info() {
    let backend = MockBackend::new();
    let addr = start_server(backend).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{addr}/mcp"))
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();

    assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(resp["result"]["serverInfo"]["name"], "aaos");
}

#[tokio::test]
async fn tools_list_returns_three_tools() {
    let backend = MockBackend::new();
    let addr = start_server(backend).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{addr}/mcp"))
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();

    let tools = resp["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 3);
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"submit_goal"));
    assert!(names.contains(&"get_agent_status"));
    assert!(names.contains(&"cancel_agent"));
}

#[tokio::test]
async fn submit_goal_returns_run_id() {
    let backend = MockBackend::new();
    let addr = start_server(backend).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{addr}/mcp"))
        .json(&json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "tools/call",
            "params": {
                "name": "submit_goal",
                "arguments": { "goal": "fetch HN and summarise" }
            }
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();

    assert!(resp["result"]["run_id"].is_string());
    assert!(resp["error"].is_null());
}

#[tokio::test]
async fn cancel_agent_returns_cancelled_true() {
    let backend = MockBackend::new();
    let addr = start_server(backend).await;
    let client = reqwest::Client::new();

    let fake_id = AgentId::new().to_string();
    let resp = client
        .post(format!("http://{addr}/mcp"))
        .json(&json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "tools/call",
            "params": { "name": "cancel_agent", "arguments": { "run_id": fake_id } }
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();

    assert_eq!(resp["result"]["cancelled"], true);
}
