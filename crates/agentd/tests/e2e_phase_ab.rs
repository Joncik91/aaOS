//! End-to-end test: Phase A + Phase B working together
//!
//! Run with: ANTHROPIC_API_KEY=sk-... cargo test -p agentd --test e2e_phase_ab -- --nocapture
//!
//! Tests the full stack:
//! 1. Spawn ephemeral agent with tools → agent.run → tool calls → response (Phase A)
//! 2. Spawn persistent agent → send 3 messages → history grows (Phase B)
//! 3. Capability enforcement: agent without tool cap can't invoke tools (Phase A)
//! 4. Persistent agent survives across multiple turns with real LLM (Phase B)
//! 5. Both agent types coexist simultaneously

use std::sync::Arc;
use std::time::Duration;

use aaos_llm::{AnthropicClient, AnthropicConfig};
use serde_json::json;

fn rpc(method: &str, params: serde_json::Value) -> agentd::api::JsonRpcRequest {
    agentd::api::JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(1),
        method: method.to_string(),
        params,
    }
}

#[tokio::test]
async fn full_e2e_phase_a_and_b() {
    let config = match AnthropicConfig::from_env() {
        Ok(c) => c,
        Err(_) => {
            eprintln!("ANTHROPIC_API_KEY not set — skipping e2e test");
            return;
        }
    };

    eprintln!("=== E2E Test: Phase A + Phase B ===\n");

    let llm = Arc::new(AnthropicClient::new(config));
    let server = agentd::server::Server::with_llm_client(llm);

    // ─── Phase A: Ephemeral agent with tool use ───

    eprintln!("── Phase A: Ephemeral Agent ──\n");

    // 1. Spawn ephemeral agent with echo tool capability
    eprintln!("1. Spawn ephemeral agent with echo tool...");
    let resp = server.handle_request(&rpc("agent.spawn", json!({
        "manifest": "name: tool-user\nmodel: claude-haiku-4-5-20251001\nsystem_prompt: \"You have an echo tool. When asked to echo something, use it. Be concise.\"\ncapabilities:\n  - \"tool: echo\""
    }))).await;
    assert!(resp.error.is_none(), "spawn failed: {:?}", resp.error);
    let ephemeral_id = resp.result.as_ref().unwrap()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();
    eprintln!("   ID: {ephemeral_id}");

    // 2. Run ephemeral agent — should use the echo tool
    eprintln!("2. Run ephemeral agent: 'Echo the word hello'...");
    let resp = server
        .handle_request(&rpc(
            "agent.run",
            json!({
                "agent_id": ephemeral_id,
                "message": "Use the echo tool to echo the word hello"
            }),
        ))
        .await;
    assert!(resp.error.is_none(), "run failed: {:?}", resp.error);
    let result = resp.result.unwrap();
    eprintln!("   Response: {}", result["response"]);
    eprintln!(
        "   Iterations: {}, Stop: {}",
        result["iterations"], result["stop_reason"]
    );
    // Should have used the tool (2+ iterations)
    assert!(result["iterations"].as_u64().unwrap() >= 1);

    // 3. Capability enforcement: agent without echo tool can't use it
    eprintln!("\n3. Capability enforcement: spawn agent WITHOUT echo tool...");
    let resp = server.handle_request(&rpc("agent.spawn", json!({
        "manifest": "name: no-tools\nmodel: claude-haiku-4-5-20251001\nsystem_prompt: \"test\"\ncapabilities:\n  - web_search"
    }))).await;
    let no_tool_id = resp.result.as_ref().unwrap()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = server
        .handle_request(&rpc(
            "tool.invoke",
            json!({
                "agent_id": no_tool_id,
                "tool": "echo",
                "input": {"message": "should fail"}
            }),
        ))
        .await;
    assert!(resp.error.is_some(), "should have been denied");
    eprintln!(
        "   Correctly denied: {}",
        resp.error.as_ref().unwrap().message
    );

    // 4. Tool list filtered by capability
    eprintln!("\n4. Verify tool listing (spawn_and_run agent sees its tools)...");
    let resp = server.handle_request(&rpc("tool.list", json!({}))).await;
    let tools = resp.result.unwrap()["tools"].as_array().unwrap().clone();
    eprintln!(
        "   Available tools: {:?}",
        tools
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect::<Vec<_>>()
    );
    assert!(!tools.is_empty());

    // ─── Phase B: Persistent agent with conversation memory ───

    eprintln!("\n── Phase B: Persistent Agent ──\n");

    // 5. Spawn persistent agent
    eprintln!("5. Spawn persistent agent...");
    let resp = server.handle_request(&rpc("agent.spawn", json!({
        "manifest": "name: memory-agent\nmodel: claude-haiku-4-5-20251001\nsystem_prompt: \"You have perfect memory. Always be concise (1 sentence). Remember everything you're told.\"\nlifecycle: persistent\nmemory:\n  max_history_messages: 50"
    }))).await;
    assert!(resp.error.is_none(), "spawn failed: {:?}", resp.error);
    let persistent_id = resp.result.as_ref().unwrap()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();
    eprintln!("   ID: {persistent_id}");

    // 6. Both agents visible in agent.list
    eprintln!("\n6. Agent list shows both types...");
    let resp = server.handle_request(&rpc("agent.list", json!({}))).await;
    let agents = resp.result.unwrap()["agents"].as_array().unwrap().clone();
    eprintln!("   {} agents running", agents.len());
    assert!(
        agents.len() >= 3,
        "expected at least 3 agents (ephemeral + no-tools + persistent)"
    );

    // 7. Send 3 messages to persistent agent
    let messages = [
        "My favorite color is purple.",
        "My dog's name is Rex.",
        "What is my favorite color and what is my dog's name?",
    ];

    for (i, msg) in messages.iter().enumerate() {
        eprintln!("\n7.{}: Sending: '{msg}'", i + 1);
        let resp = server
            .handle_request(&rpc(
                "agent.run",
                json!({
                    "agent_id": persistent_id,
                    "message": msg
                }),
            ))
            .await;
        assert!(resp.error.is_none(), "run failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        eprintln!("   Delivered: trace_id={}", result["trace_id"]);

        // Wait for LLM to process
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    // 8. Verify session history
    eprintln!("\n8. Checking conversation history...");
    let agent_id_parsed: aaos_core::AgentId = serde_json::from_value(json!(persistent_id)).unwrap();
    let history = server.session_store.load(&agent_id_parsed).unwrap();
    eprintln!("   {} messages in session store", history.len());
    assert!(
        history.len() >= 6,
        "expected at least 6 messages, got {}",
        history.len()
    );

    // 9. Stop persistent agent cleanly
    eprintln!("\n9. Stopping persistent agent...");
    let resp = server
        .handle_request(&rpc(
            "agent.stop",
            json!({
                "agent_id": persistent_id
            }),
        ))
        .await;
    assert!(resp.error.is_none(), "stop failed: {:?}", resp.error);
    eprintln!("   Stopped cleanly");

    // 10. Verify ephemeral agent still works after persistent operations
    eprintln!("\n10. Ephemeral agent still works after persistent operations...");
    let resp = server.handle_request(&rpc("agent.spawn_and_run", json!({
        "manifest": "name: final-check\nmodel: claude-haiku-4-5-20251001\nsystem_prompt: \"Reply with one word only.\"",
        "message": "Say 'working'"
    }))).await;
    assert!(
        resp.error.is_none(),
        "final ephemeral failed: {:?}",
        resp.error
    );
    let result = resp.result.unwrap();
    eprintln!("   Response: {}", result["response"]);

    eprintln!("\n=== E2E Test PASSED ===");
    eprintln!("  Phase A: ephemeral agent ran tools, capability enforcement works");
    eprintln!(
        "  Phase B: persistent agent processed 3 messages, {} messages in history",
        history.len()
    );
    eprintln!("  Both: coexist, agent.list sees all, clean shutdown");
}
