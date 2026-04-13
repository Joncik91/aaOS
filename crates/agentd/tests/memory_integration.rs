//! Integration test: Phase C2 episodic memory via JSON-RPC (mock embeddings)
//!
//! Run with: cargo test -p agentd --test memory_integration -- --nocapture
//!
//! Tests memory_store, memory_query, memory_delete through the server's
//! tool.invoke RPC method using MockEmbeddingSource (no external deps).

use std::sync::Arc;
use std::sync::Mutex;

use aaos_core::TokenUsage;
use aaos_llm::{CompletionRequest, CompletionResponse, ContentBlock, LlmClient, LlmResult, LlmStopReason};
use async_trait::async_trait;
use serde_json::json;

fn rpc(method: &str, params: serde_json::Value) -> agentd::api::JsonRpcRequest {
    agentd::api::JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(1),
        method: method.to_string(),
        params,
    }
}

/// Minimal mock LLM — we never actually call the LLM in these tests,
/// but Server::with_llm_client requires one.
struct MockLlm {
    responses: Mutex<Vec<LlmResult<CompletionResponse>>>,
}

impl MockLlm {
    fn stub() -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(vec![Ok(CompletionResponse {
                content: vec![ContentBlock::Text { text: "ok".into() }],
                stop_reason: LlmStopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            })]),
        })
    }
}

#[async_trait]
impl LlmClient for MockLlm {
    fn max_context_tokens(&self, _model: &str) -> u32 {
        200_000
    }

    async fn complete(&self, _req: CompletionRequest) -> LlmResult<CompletionResponse> {
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            Ok(CompletionResponse {
                content: vec![ContentBlock::Text { text: "fallback".into() }],
                stop_reason: LlmStopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            })
        } else {
            responses.remove(0)
        }
    }
}

#[tokio::test]
async fn memory_tools_via_rpc() {
    eprintln!("=== Integration Test: Phase C2 Memory Tools ===\n");

    let server = agentd::server::Server::with_llm_client(MockLlm::stub());

    // ─── 1. Spawn agent with memory capabilities ─────────────────────────────

    eprintln!("1. Spawn agent with memory_store, memory_query, memory_delete capabilities...");
    let resp = server.handle_request(&rpc("agent.spawn", json!({
        "manifest": "name: mem-agent\nmodel: claude-haiku-4-5-20251001\nsystem_prompt: \"test\"\ncapabilities:\n  - \"tool: memory_store\"\n  - \"tool: memory_query\"\n  - \"tool: memory_delete\""
    }))).await;
    assert!(resp.error.is_none(), "spawn failed: {:?}", resp.error);
    let agent_id = resp.result.as_ref().unwrap()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();
    eprintln!("   Agent ID: {agent_id}");

    // ─── 2. Store a fact via memory_store ────────────────────────────────────

    eprintln!("\n2. Store fact: 'The project deadline is March 15th'...");
    let resp = server.handle_request(&rpc("tool.invoke", json!({
        "agent_id": agent_id,
        "tool": "memory_store",
        "input": {
            "content": "The project deadline is March 15th",
            "category": "fact"
        }
    }))).await;
    assert!(resp.error.is_none(), "memory_store failed: {:?}", resp.error);
    let result = resp.result.unwrap();
    assert_eq!(result["result"]["status"], "stored");
    let memory_id = result["result"]["memory_id"].as_str().unwrap().to_string();
    eprintln!("   Stored: memory_id={memory_id}");

    // Store a second fact
    eprintln!("   Store second fact: 'We use PostgreSQL 16'...");
    let resp = server.handle_request(&rpc("tool.invoke", json!({
        "agent_id": agent_id,
        "tool": "memory_store",
        "input": {
            "content": "We use PostgreSQL 16 for the database",
            "category": "fact"
        }
    }))).await;
    assert!(resp.error.is_none(), "memory_store (2nd) failed: {:?}", resp.error);
    let result2 = resp.result.unwrap();
    let memory_id_2 = result2["result"]["memory_id"].as_str().unwrap().to_string();
    eprintln!("   Stored: memory_id={memory_id_2}");

    // ─── 3. Query memories via memory_query ──────────────────────────────────

    eprintln!("\n3. Query: 'project deadline'...");
    let resp = server.handle_request(&rpc("tool.invoke", json!({
        "agent_id": agent_id,
        "tool": "memory_query",
        "input": {
            "query": "project deadline"
        }
    }))).await;
    assert!(resp.error.is_none(), "memory_query failed: {:?}", resp.error);
    let query_result = resp.result.unwrap();
    let count = query_result["result"]["count"].as_u64().unwrap();
    eprintln!("   Results: {count}");
    assert!(count >= 1, "expected at least 1 result, got {count}");

    // Verify at least one result contains our stored content
    let results = query_result["result"]["results"].as_array().unwrap();
    let contents: Vec<&str> = results
        .iter()
        .filter_map(|r| r["content"].as_str())
        .collect();
    eprintln!("   Contents: {contents:?}");
    // With mock embeddings (random vectors), both results will be returned;
    // we just verify at least one exists and has content
    assert!(!contents.is_empty());

    // ─── 4. Delete a memory via memory_delete ────────────────────────────────

    eprintln!("\n4. Delete memory: {memory_id}...");
    let resp = server.handle_request(&rpc("tool.invoke", json!({
        "agent_id": agent_id,
        "tool": "memory_delete",
        "input": {
            "memory_id": memory_id
        }
    }))).await;
    assert!(resp.error.is_none(), "memory_delete failed: {:?}", resp.error);
    let del_result = resp.result.unwrap();
    assert_eq!(del_result["result"]["status"], "deleted");
    eprintln!("   Deleted successfully");

    // Verify the deleted memory is gone — query should return fewer results
    eprintln!("   Verify deleted memory is gone...");
    let resp = server.handle_request(&rpc("tool.invoke", json!({
        "agent_id": agent_id,
        "tool": "memory_query",
        "input": {
            "query": "project deadline",
            "limit": 10
        }
    }))).await;
    assert!(resp.error.is_none());
    let after_delete = resp.result.unwrap();
    let remaining_ids: Vec<&str> = after_delete["result"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["memory_id"].as_str())
        .collect();
    assert!(
        !remaining_ids.contains(&memory_id.as_str()),
        "deleted memory should not appear in query results"
    );
    eprintln!("   Confirmed: deleted memory not in results");

    // ─── 5. Agent isolation: second agent sees no memories ───────────────────

    eprintln!("\n5. Agent isolation: spawn second agent, query memories...");
    let resp = server.handle_request(&rpc("agent.spawn", json!({
        "manifest": "name: other-agent\nmodel: claude-haiku-4-5-20251001\nsystem_prompt: \"test\"\ncapabilities:\n  - \"tool: memory_query\""
    }))).await;
    assert!(resp.error.is_none(), "spawn agent2 failed: {:?}", resp.error);
    let agent2_id = resp.result.as_ref().unwrap()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();
    eprintln!("   Agent2 ID: {agent2_id}");

    let resp = server.handle_request(&rpc("tool.invoke", json!({
        "agent_id": agent2_id,
        "tool": "memory_query",
        "input": {
            "query": "project deadline"
        }
    }))).await;
    assert!(resp.error.is_none(), "memory_query (agent2) failed: {:?}", resp.error);
    let isolated_result = resp.result.unwrap();
    let isolated_count = isolated_result["result"]["count"].as_u64().unwrap();
    eprintln!("   Agent2 results: {isolated_count}");
    assert_eq!(
        isolated_count, 0,
        "second agent should see 0 memories from first agent"
    );

    // ─── 6. Capability enforcement: agent without memory cap can't use it ────

    eprintln!("\n6. Capability enforcement: agent without memory cap...");
    let resp = server.handle_request(&rpc("agent.spawn", json!({
        "manifest": "name: no-memory\nmodel: claude-haiku-4-5-20251001\nsystem_prompt: \"test\"\ncapabilities:\n  - \"tool: echo\""
    }))).await;
    let no_mem_id = resp.result.as_ref().unwrap()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = server.handle_request(&rpc("tool.invoke", json!({
        "agent_id": no_mem_id,
        "tool": "memory_store",
        "input": {"content": "should fail", "category": "fact"}
    }))).await;
    assert!(resp.error.is_some(), "should have been denied");
    eprintln!("   Correctly denied: {}", resp.error.as_ref().unwrap().message);

    eprintln!("\n=== Integration Test PASSED ===");
    eprintln!("  memory_store: facts stored via RPC");
    eprintln!("  memory_query: results returned, count correct");
    eprintln!("  memory_delete: memory removed, verified gone");
    eprintln!("  Agent isolation: second agent sees 0 memories");
    eprintln!("  Capability enforcement: denied without cap");
}
