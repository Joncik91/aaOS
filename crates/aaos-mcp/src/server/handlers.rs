use crate::server::McpServerBackend;
use crate::types::{JsonRpcRequest, JsonRpcResponse};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

/// POST /mcp — JSON-RPC dispatcher.
pub async fn handle_jsonrpc(
    State(backend): State<Arc<dyn McpServerBackend>>,
    body: Result<Json<JsonRpcRequest>, axum::extract::rejection::JsonRejection>,
) -> Json<JsonRpcResponse> {
    let Json(req) = match body {
        Ok(j) => j,
        Err(_) => {
            return Json(JsonRpcResponse::error(Value::Null, -32700, "Parse error"));
        }
    };
    let resp = match req.method.as_str() {
        "initialize" => JsonRpcResponse::success(
            req.id,
            json!({
                "protocolVersion": "2024-11-05",
                "serverInfo": { "name": "aaos", "version": "0.1" },
                "capabilities": { "tools": {} }
            }),
        ),
        "tools/list" => JsonRpcResponse::success(
            req.id,
            json!({
                "tools": [
                    {
                        "name": "submit_goal",
                        "description": "Submit a goal to aaOS. Returns a run_id for status polling.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "goal": { "type": "string", "description": "The goal to execute" },
                                "role": { "type": "string", "description": "Optional role override" }
                            },
                            "required": ["goal"]
                        }
                    },
                    {
                        "name": "get_agent_status",
                        "description": "Get the status of a submitted run.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "run_id": { "type": "string" }
                            },
                            "required": ["run_id"]
                        }
                    },
                    {
                        "name": "cancel_agent",
                        "description": "Cancel a running agent.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "run_id": { "type": "string" }
                            },
                            "required": ["run_id"]
                        }
                    }
                ]
            }),
        ),
        "tools/call" => dispatch_tool_call(req.id, &req.params, backend).await,
        _ => JsonRpcResponse::error(req.id, -32601, "Method not found"),
    };
    Json(resp)
}

async fn dispatch_tool_call(
    id: Value,
    params: &Value,
    backend: Arc<dyn McpServerBackend>,
) -> JsonRpcResponse {
    let tool_name = match params.get("name").and_then(|n| n.as_str()) {
        Some(n) => n,
        None => return JsonRpcResponse::error(id, -32602, "missing tool name"),
    };
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    match tool_name {
        "submit_goal" => {
            let goal = match args.get("goal").and_then(|g| g.as_str()) {
                Some(g) => g.to_string(),
                None => return JsonRpcResponse::error(id, -32602, "missing 'goal'"),
            };
            let role = args
                .get("role")
                .and_then(|r| r.as_str())
                .map(str::to_string);

            match backend.submit_goal(goal, role).await {
                Ok(agent_id) => {
                    JsonRpcResponse::success(id, json!({ "run_id": agent_id.to_string() }))
                }
                Err(e) => JsonRpcResponse::error(id, -32000, e.to_string()),
            }
        }
        "get_agent_status" => {
            let run_id_str = match args.get("run_id").and_then(|r| r.as_str()) {
                Some(s) => s,
                None => return JsonRpcResponse::error(id, -32602, "missing 'run_id'"),
            };
            let agent_id = match run_id_str.parse::<aaos_core::AgentId>() {
                Ok(a) => a,
                Err(_) => return JsonRpcResponse::error(id, -32602, "invalid run_id"),
            };
            let status = backend.run_status(&agent_id);
            JsonRpcResponse::success(id, serde_json::to_value(status).unwrap())
        }
        "cancel_agent" => {
            let run_id_str = match args.get("run_id").and_then(|r| r.as_str()) {
                Some(s) => s,
                None => return JsonRpcResponse::error(id, -32602, "missing 'run_id'"),
            };
            let agent_id = match run_id_str.parse::<aaos_core::AgentId>() {
                Ok(a) => a,
                Err(_) => return JsonRpcResponse::error(id, -32602, "invalid run_id"),
            };
            let cancelled = backend.cancel(&agent_id).await;
            JsonRpcResponse::success(id, json!({ "cancelled": cancelled }))
        }
        _ => JsonRpcResponse::error(id, -32602, format!("unknown tool: {tool_name}")),
    }
}

#[derive(Deserialize)]
pub struct SseQuery {
    pub run_id: String,
}

/// GET /mcp/events?run_id=<id> — SSE stream of audit events for a run.
pub async fn handle_sse(
    State(backend): State<Arc<dyn McpServerBackend>>,
    Query(q): Query<SseQuery>,
) -> Response {
    let agent_id = match q.run_id.parse::<aaos_core::AgentId>() {
        Ok(a) => a,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid run_id").into_response(),
    };
    let rx = backend.subscribe_audit();
    crate::server::sse::audit_sse_stream(rx, agent_id).into_response()
}
