//! Smoke test: persistent agent with real Anthropic API
//!
//! Run with: ANTHROPIC_API_KEY=sk-... cargo test -p agentd --test smoke_persistent -- --nocapture
//!
//! This spawns a persistent agent, sends it 3 messages building on each other,
//! and verifies conversation history works (the agent remembers prior turns).

use std::sync::Arc;
use std::time::Duration;

use aaos_llm::{AnthropicClient, AnthropicConfig};
use serde_json::json;

fn make_request(method: &str, params: serde_json::Value) -> agentd::api::JsonRpcRequest {
    agentd::api::JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(1),
        method: method.to_string(),
        params,
    }
}

#[tokio::test]
async fn persistent_agent_with_real_llm() {
    // Skip if no API key
    let config = match AnthropicConfig::from_env() {
        Ok(c) => c,
        Err(_) => {
            eprintln!("ANTHROPIC_API_KEY not set — skipping live smoke test");
            return;
        }
    };

    eprintln!("=== Phase B Smoke Test: Persistent Agent with Haiku 4.5 ===\n");

    let llm = Arc::new(AnthropicClient::new(config));
    let server = agentd::server::Server::with_llm_client(llm);

    // 1. Spawn a persistent agent
    let manifest = r#"
name: smoke-persistent
model: claude-haiku-4-5-20251001
system_prompt: "You are a helpful assistant with a great memory. Always be concise (1-2 sentences). When asked to remember something, confirm you will. When asked to recall, prove you remember by stating the exact fact."
lifecycle: persistent
memory:
  max_history_messages: 50
"#;

    eprintln!("1. Spawning persistent agent...");
    let resp = server
        .handle_request(&make_request("agent.spawn", json!({"manifest": manifest})))
        .await;
    assert!(resp.error.is_none(), "spawn failed: {:?}", resp.error);
    let agent_id = resp.result.as_ref().unwrap()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();
    eprintln!("   Agent ID: {agent_id}");

    // 2. Send message 1: give it a fact to remember
    eprintln!("\n2. Sending message 1: 'Remember: the secret code is BLUE-42'");
    let resp = server
        .handle_request(&make_request(
            "agent.run",
            json!({"agent_id": agent_id, "message": "Remember this: the secret code is BLUE-42"}),
        ))
        .await;
    assert!(resp.error.is_none(), "run failed: {:?}", resp.error);
    let result = resp.result.unwrap();
    eprintln!("   Delivered: trace_id={}, status={}", result["trace_id"], result["status"]);

    // Wait for LLM processing
    tokio::time::sleep(Duration::from_secs(5)).await;

    // 3. Send message 2: ask something unrelated
    eprintln!("\n3. Sending message 2: 'What is 2+2?'");
    let resp = server
        .handle_request(&make_request(
            "agent.run",
            json!({"agent_id": agent_id, "message": "What is 2+2?"}),
        ))
        .await;
    assert!(resp.error.is_none(), "run failed: {:?}", resp.error);
    let result = resp.result.unwrap();
    eprintln!("   Delivered: trace_id={}, status={}", result["trace_id"], result["status"]);

    tokio::time::sleep(Duration::from_secs(5)).await;

    // 4. Send message 3: ask it to recall the secret code
    eprintln!("\n4. Sending message 3: 'What was the secret code I told you earlier?'");
    let resp = server
        .handle_request(&make_request(
            "agent.run",
            json!({"agent_id": agent_id, "message": "What was the secret code I told you earlier?"}),
        ))
        .await;
    assert!(resp.error.is_none(), "run failed: {:?}", resp.error);
    let result = resp.result.unwrap();
    eprintln!("   Delivered: trace_id={}, status={}", result["trace_id"], result["status"]);

    // Wait for the 3rd message to be processed
    tokio::time::sleep(Duration::from_secs(5)).await;

    // 5. Check session store has history
    eprintln!("\n5. Checking session store for conversation history...");
    let agent_id_parsed: aaos_core::AgentId =
        serde_json::from_value(json!(agent_id)).unwrap();
    match server.session_store.load(&agent_id_parsed) {
        Ok(messages) => {
            eprintln!("   Session has {} messages in history", messages.len());
            // Each turn = user + assistant = 2 messages, 3 turns = at least 6
            assert!(
                messages.len() >= 6,
                "expected at least 6 messages (3 user + 3 assistant), got {}",
                messages.len()
            );
            eprintln!("   Conversation history preserved across 3 turns!");
        }
        Err(e) => {
            panic!("Failed to load session: {e}");
        }
    }

    // 6. Verify ephemeral agent still works with real LLM
    eprintln!("\n6. Verifying ephemeral agent still works...");
    let ephemeral_manifest = r#"
name: smoke-ephemeral
model: claude-haiku-4-5-20251001
system_prompt: "Reply with exactly one word."
"#;
    let resp = server
        .handle_request(&make_request(
            "agent.spawn_and_run",
            json!({"manifest": ephemeral_manifest, "message": "Say hello"}),
        ))
        .await;
    assert!(resp.error.is_none(), "ephemeral run failed: {:?}", resp.error);
    let result = resp.result.unwrap();
    eprintln!("   Ephemeral response: {}", result["response"]);
    assert!(!result["response"].as_str().unwrap().is_empty());

    // 7. Stop persistent agent
    eprintln!("\n7. Stopping persistent agent...");
    let resp = server
        .handle_request(&make_request(
            "agent.stop",
            json!({"agent_id": agent_id}),
        ))
        .await;
    assert!(resp.error.is_none(), "stop failed: {:?}", resp.error);

    let final_count = server.session_store.load(&agent_id_parsed)
        .map(|m| m.len()).unwrap_or(0);

    eprintln!("\n=== Phase B Smoke Test PASSED ===");
    eprintln!("  - Persistent agent spawned and processed 3 messages");
    eprintln!("  - Conversation history persisted ({final_count} messages)");
    eprintln!("  - Ephemeral agent still works");
    eprintln!("  - Clean shutdown");
}
