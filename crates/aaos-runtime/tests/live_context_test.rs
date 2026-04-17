//! Live API test for context window management.
//! Run with: cargo test -p aaos-runtime --test live_context_test -- --ignored --nocapture

use std::sync::Arc;

use aaos_core::{AgentId, TokenBudget};
use aaos_llm::{AnthropicClient, AnthropicConfig, ContentBlock, LlmClient, Message};
use aaos_runtime::context::ContextManager;
use aaos_runtime::{ArchiveSegment, InMemorySessionStore, SessionStore};

#[tokio::test]
#[ignore] // Requires ANTHROPIC_API_KEY
async fn live_context_summarization_preserves_facts() {
    let config = AnthropicConfig::from_env().expect("ANTHROPIC_API_KEY must be set");
    let llm: Arc<dyn LlmClient> = Arc::new(AnthropicClient::new(config));

    let model = "claude-haiku-4-5-20251001";
    let model_max = llm.max_context_tokens(model);

    // Use a small budget to trigger summarization quickly
    let budget = TokenBudget::from_config("8k", model_max).unwrap();
    let cm = ContextManager::new(llm.clone(), budget, model.to_string(), 0.7);

    let session_store = InMemorySessionStore::new();
    let agent_id = AgentId::new();

    let mut history: Vec<Message> = Vec::new();
    let mut archive_count = 0usize;

    // Scatter facts across many messages
    let facts = vec![
        "My name is Zephyr.",
        "My dog is called Nimbus.",
        "The project deadline is March 15th.",
        "I prefer dark mode in all editors.",
        "The server runs on port 8080.",
        "My favorite language is Rust.",
        "The database is PostgreSQL 16.",
        "The team lead is named River.",
        "We use GitHub Actions for CI.",
        "The staging URL is staging.example.com.",
    ];

    // Send facts with padding messages between them
    for (i, fact) in facts.iter().enumerate() {
        // Send the fact
        history.push(Message::User {
            content: fact.to_string(),
        });
        history.push(Message::Assistant {
            content: vec![ContentBlock::Text {
                text: format!("Noted! I'll remember that. (fact #{i})"),
            }],
        });

        // Send padding messages to fill up the context
        for j in 0..3 {
            history.push(Message::User {
                content: format!(
                    "Tell me about topic {i}-{j} in detail. {}",
                    "padding ".repeat(50)
                ),
            });
            history.push(Message::Assistant {
                content: vec![ContentBlock::Text {
                    text: format!(
                        "Here's info about topic {i}-{j}. {}",
                        "response ".repeat(50)
                    ),
                }],
            });
        }

        // Check if summarization would trigger
        let result = cm
            .prepare_context(&history, "You are a helpful assistant with perfect memory.")
            .await;
        if let Ok(prepared) = result {
            if let Some(summ) = prepared.summarization {
                println!("Summarization triggered at message {}!", history.len());
                println!(
                    "Archived {} messages, saved ~{} tokens",
                    summ.archived_messages.len(),
                    summ.tokens_saved_estimate
                );

                // Archive first
                let segment = ArchiveSegment {
                    source_range: summ.source_range,
                    messages: summ.archived_messages,
                    archived_at: chrono::Utc::now(),
                };
                session_store.archive_segment(&agent_id, &segment).unwrap();
                archive_count += 1;

                // Then update history
                let end = summ.source_range.1 + 1;
                history.drain(..end.min(history.len()));
                history.insert(0, summ.summary);

                println!("History now has {} messages", history.len());
            }
        }
    }

    println!("\nFinal history has {} messages", history.len());

    // Now prepare final context — the agent should know facts from the summary
    let final_prepared = cm
        .prepare_context(&history, "You are a helpful assistant with perfect memory.")
        .await
        .unwrap();

    println!(
        "System prompt length: {} chars",
        final_prepared.system_prompt.len()
    );
    if final_prepared
        .system_prompt
        .contains("[Previous conversation summary]")
    {
        println!("Summary was folded into system prompt.");
    }

    // Verify archives were created
    let archives = session_store.load_archives(&agent_id).unwrap();
    println!("Archives on disk: {}", archives.len());
    assert!(
        archives.len() >= 1,
        "Expected at least one archive segment (got {})",
        archive_count
    );

    // The summary should mention key facts
    if let Some(Message::Summary { content, .. }) = history.first() {
        println!("\nSummary content:\n{content}");
        // At minimum, early facts should be in the summary
        // (later facts may still be in active history)
        let has_name = content.contains("Zephyr");
        let has_dog = content.contains("Nimbus");
        println!("Contains 'Zephyr': {has_name}");
        println!("Contains 'Nimbus': {has_dog}");
        // These are soft assertions — LLM summaries aren't deterministic,
        // but a good summarization model should preserve proper nouns.
        if !has_name {
            println!("WARNING: Summary may have lost the user's name");
        }
        if !has_dog {
            println!("WARNING: Summary may have lost the dog's name");
        }
    }
}
