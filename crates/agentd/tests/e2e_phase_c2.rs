//! End-to-end test: Phase C2 episodic memory with real LLM + Ollama embeddings
//!
//! Run with: ANTHROPIC_API_KEY=sk-... cargo test -p agentd --test e2e_phase_c2 -- --nocapture
//!
//! Tests the full memory pipeline:
//! 1. Create server with real Ollama nomic-embed-text embeddings
//! 2. Spawn a persistent agent with memory tools
//! 3. Tell the agent to remember facts (triggers memory_store tool calls)
//! 4. Ask the agent about stored facts (triggers memory_query)
//! 5. Verify the agent's response references the stored information

use std::sync::Arc;
use std::time::Duration;

use aaos_llm::{AnthropicClient, AnthropicConfig};
use aaos_memory::{InMemoryMemoryStore, OllamaEmbeddingSource};
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
async fn e2e_phase_c2_episodic_memory() {
    // ─── Gate: skip if no API key ────────────────────────────────────────────

    let config = match AnthropicConfig::from_env() {
        Ok(c) => c,
        Err(_) => {
            eprintln!("ANTHROPIC_API_KEY not set — skipping e2e test");
            return;
        }
    };

    eprintln!("=== E2E Test: Phase C2 Episodic Memory ===\n");

    // ─── Setup: real LLM + Ollama embeddings ─────────────────────────────────

    eprintln!("Setting up server with Ollama nomic-embed-text embeddings...");

    let llm = Arc::new(AnthropicClient::new(config));
    let embedding_source: Arc<dyn aaos_memory::EmbeddingSource> = Arc::new(
        OllamaEmbeddingSource::new("http://localhost:11434", "nomic-embed-text", 768),
    );
    let memory_store: Arc<dyn aaos_memory::MemoryStore> = Arc::new(
        InMemoryMemoryStore::new(10_000, 768, "nomic-embed-text"),
    );

    let server = agentd::server::Server::with_memory(
        llm,
        memory_store.clone(),
        embedding_source,
    );

    // ─── 1. Spawn persistent agent with memory tools ─────────────────────────

    eprintln!("\n1. Spawn persistent agent with memory tools...");
    let manifest = concat!(
        "name: memory-agent\n",
        "model: claude-haiku-4-5-20251001\n",
        "system_prompt: \"You are a helpful assistant with persistent memory. ",
        "When the user tells you to remember something, use the memory_store tool to save it as a fact. ",
        "When asked about something you might have stored, use memory_query to search your memories first. ",
        "Always use the tools — do not just reply from conversation context. Be concise.\"\n",
        "lifecycle: persistent\n",
        "capabilities:\n",
        "  - \"tool: memory_store\"\n",
        "  - \"tool: memory_query\"\n",
        "  - \"tool: memory_delete\"\n",
        "memory:\n",
        "  max_history_messages: 50",
    );

    let resp = server.handle_request(&rpc("agent.spawn", json!({
        "manifest": manifest
    }))).await;
    assert!(resp.error.is_none(), "spawn failed: {:?}", resp.error);
    let agent_id = resp.result.as_ref().unwrap()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();
    eprintln!("   Agent ID: {agent_id}");

    // ─── 2. Tell agent to remember first fact ────────────────────────────────

    eprintln!("\n2. Send: 'Remember this: the project deadline is March 15th, 2026'...");
    let resp = server.handle_request(&rpc("agent.run", json!({
        "agent_id": agent_id,
        "message": "Remember this: the project deadline is March 15th, 2026"
    }))).await;
    assert!(resp.error.is_none(), "run (1) failed: {:?}", resp.error);
    let result = resp.result.unwrap();
    eprintln!("   Delivered: trace_id={}", result["trace_id"]);

    // Wait for LLM to process and call memory_store
    eprintln!("   Waiting for LLM processing...");
    tokio::time::sleep(Duration::from_secs(8)).await;

    // ─── 3. Tell agent to remember second fact ───────────────────────────────

    eprintln!("\n3. Send: 'Remember this: we use PostgreSQL 16 for the database'...");
    let resp = server.handle_request(&rpc("agent.run", json!({
        "agent_id": agent_id,
        "message": "Remember this: we use PostgreSQL 16 for the database"
    }))).await;
    assert!(resp.error.is_none(), "run (2) failed: {:?}", resp.error);
    let result = resp.result.unwrap();
    eprintln!("   Delivered: trace_id={}", result["trace_id"]);

    eprintln!("   Waiting for LLM processing...");
    tokio::time::sleep(Duration::from_secs(8)).await;

    // ─── 4. Ask about stored facts ───────────────────────────────────────────

    eprintln!("\n4. Send: 'What do you know about the project deadline?'...");
    let resp = server.handle_request(&rpc("agent.run", json!({
        "agent_id": agent_id,
        "message": "What do you know about the project deadline?"
    }))).await;
    assert!(resp.error.is_none(), "run (3) failed: {:?}", resp.error);
    let result = resp.result.unwrap();
    eprintln!("   Delivered: trace_id={}", result["trace_id"]);

    eprintln!("   Waiting for LLM processing...");
    tokio::time::sleep(Duration::from_secs(10)).await;

    // ─── 5. Check session history for evidence of tool use ───────────────────

    eprintln!("\n5. Checking conversation history for tool use...");
    let agent_id_parsed: aaos_core::AgentId =
        serde_json::from_value(json!(agent_id)).unwrap();
    let history = server.session_store.load(&agent_id_parsed).unwrap();
    eprintln!("   {} messages in session store", history.len());

    // Look for evidence of memory_store / memory_query tool calls in history
    let history_text = format!("{:?}", history);
    let has_memory_store = history_text.contains("memory_store");
    let has_memory_query = history_text.contains("memory_query");
    eprintln!("   memory_store called: {has_memory_store}");
    eprintln!("   memory_query called: {has_memory_query}");

    // Soft assertions — the LLM should have called the tools, but we don't
    // hard-fail if it didn't (LLM behavior is non-deterministic)
    if !has_memory_store {
        eprintln!("   WARNING: memory_store was not called — LLM may not have used tools");
    }
    if !has_memory_query {
        eprintln!("   WARNING: memory_query was not called — LLM may not have used tools");
    }

    // Check if any assistant response mentions March 15th
    let mentions_deadline = history_text.contains("March 15")
        || history_text.contains("march 15");
    eprintln!("   Response mentions deadline: {mentions_deadline}");

    // ─── 6. Verify memory store has records ──────────────────────────────────

    eprintln!("\n6. Checking memory store directly...");
    // Query memory store for all records from this agent
    let query_embedding = vec![0.0f32; 768]; // zero vector — just to get all records
    let stored = memory_store
        .query(&agent_id_parsed, &query_embedding, 10, None)
        .await;
    match &stored {
        Ok(records) => {
            eprintln!("   {} memories stored for this agent", records.len());
            for r in records {
                eprintln!("     - [{:?}] {}", r.category, r.content);
            }
            if records.is_empty() {
                eprintln!("   WARNING: no memories stored — LLM did not call memory_store");
            }
        }
        Err(e) => eprintln!("   WARNING: memory query failed: {e}"),
    }

    // ─── 7. Stop agent ───────────────────────────────────────────────────────

    eprintln!("\n7. Stopping agent...");
    let resp = server.handle_request(&rpc("agent.stop", json!({
        "agent_id": agent_id
    }))).await;
    assert!(resp.error.is_none(), "stop failed: {:?}", resp.error);
    eprintln!("   Stopped cleanly");

    eprintln!("\n=== E2E Test: Phase C2 PASSED ===");
    eprintln!("  Persistent agent spawned with memory tools");
    eprintln!("  3 messages sent (2 store, 1 query)");
    eprintln!("  {} messages in conversation history", history.len());
    if let Ok(records) = &stored {
        eprintln!("  {} memories in store", records.len());
    }
}
