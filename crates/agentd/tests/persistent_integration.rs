//! Integration tests for Phase B: Persistent Agents & Request-Response IPC

use std::sync::Arc;
use std::time::Duration;

use aaos_core::TokenUsage;
use aaos_llm::{
    CompletionRequest, CompletionResponse, ContentBlock, LlmClient, LlmResult, LlmStopReason,
};
use async_trait::async_trait;
use serde_json::json;

/// Mock LLM that echoes back the user message.
struct EchoLlm;

#[async_trait]
impl LlmClient for EchoLlm {
    fn max_context_tokens(&self, _model: &str) -> u32 {
        200_000
    }

    async fn complete(&self, req: CompletionRequest) -> LlmResult<CompletionResponse> {
        let last_user = req
            .messages
            .iter()
            .rev()
            .find_map(|m| {
                if let aaos_llm::Message::User { content } = m {
                    Some(content.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "no message".into());

        Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text: format!("Echo: {last_user}"),
            }],
            stop_reason: LlmStopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
            },
        })
    }
}

/// Mock LLM that counts how many messages are in its history.
struct HistoryCountLlm;

#[async_trait]
impl LlmClient for HistoryCountLlm {
    fn max_context_tokens(&self, _model: &str) -> u32 {
        200_000
    }

    async fn complete(&self, req: CompletionRequest) -> LlmResult<CompletionResponse> {
        let msg_count = req.messages.len();
        Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text: format!("I see {msg_count} messages in history"),
            }],
            stop_reason: LlmStopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
            },
        })
    }
}

fn make_request(method: &str, params: serde_json::Value) -> agentd::api::JsonRpcRequest {
    agentd::api::JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(1),
        method: method.to_string(),
        params,
    }
}

#[tokio::test]
async fn persistent_agent_processes_multiple_messages() {
    let server = agentd::server::Server::with_llm_client(Arc::new(EchoLlm));
    let manifest = r#"
name: echo-persistent
model: test
system_prompt: "Echo back."
lifecycle: persistent
"#;
    let resp = server
        .handle_request(&make_request("agent.spawn", json!({"manifest": manifest})))
        .await;
    let agent_id = resp.result.unwrap()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    for i in 1..=3 {
        let resp = server
            .handle_request(&make_request(
                "agent.run",
                json!({"agent_id": agent_id, "message": format!("Message {i}")}),
            ))
            .await;
        let result = resp.result.unwrap();
        assert!(result.get("trace_id").is_some());
    }

    tokio::time::sleep(Duration::from_millis(500)).await;

    let resp = server
        .handle_request(&make_request("agent.stop", json!({"agent_id": agent_id})))
        .await;
    assert!(resp.result.is_some());
}

#[tokio::test]
async fn persistent_agent_maintains_conversation_history() {
    let server = agentd::server::Server::with_llm_client(Arc::new(HistoryCountLlm));
    let manifest = r#"
name: history-test
model: test
system_prompt: "Count messages."
lifecycle: persistent
"#;
    let resp = server
        .handle_request(&make_request("agent.spawn", json!({"manifest": manifest})))
        .await;
    let agent_id = resp.result.unwrap()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    for _ in 0..3 {
        server
            .handle_request(&make_request(
                "agent.run",
                json!({"agent_id": agent_id, "message": "hello"}),
            ))
            .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    server
        .handle_request(&make_request("agent.stop", json!({"agent_id": agent_id})))
        .await;
}

#[tokio::test]
async fn ephemeral_agent_unchanged() {
    let server = agentd::server::Server::with_llm_client(Arc::new(EchoLlm));
    let manifest = r#"
name: ephemeral-test
model: test
system_prompt: "Echo."
"#;
    let resp = server
        .handle_request(&make_request(
            "agent.spawn_and_run",
            json!({"manifest": manifest, "message": "Hello ephemeral"}),
        ))
        .await;
    let result = resp.result.unwrap();
    assert!(result.get("response").is_some());
    assert_eq!(result["response"], "Echo: Hello ephemeral");
}
