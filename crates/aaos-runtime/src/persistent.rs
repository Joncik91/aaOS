use std::sync::Arc;
use std::time::{Duration, Instant};

use aaos_core::{
    AgentId, AgentManifest, AuditEvent, AuditEventKind, AuditLog, PromptSource,
};
use aaos_ipc::{McpMessage, MessageRouter};
use aaos_llm::{AgentExecutor, ExecutionStopReason, Message};
use tokio::sync::mpsc;

use crate::context::ContextManager;
use crate::process::AgentCommand;
use crate::session::{ArchiveSegment, SessionStore};

/// Minimum interval between `SessionStoreError` audit events per agent.
/// A persistently-broken store doesn't need to spam the audit log.
const SESSION_STORE_ERROR_THROTTLE: Duration = Duration::from_secs(60);

/// Extract the user message string from an McpMessage's params.
fn extract_user_message(msg: &McpMessage) -> String {
    msg.params
        .get("message")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| msg.method.clone())
}

/// Run the persistent agent message loop.
pub async fn persistent_agent_loop(
    agent_id: AgentId,
    manifest: AgentManifest,
    mut message_rx: mpsc::Receiver<McpMessage>,
    mut command_rx: mpsc::Receiver<AgentCommand>,
    executor: AgentExecutor,
    session_store: Arc<dyn SessionStore>,
    router: Arc<MessageRouter>,
    audit_log: Arc<dyn AuditLog>,
    context_manager: Option<Arc<ContextManager>>,
) {
    let mut history: Vec<Message> = session_store
        .load(&agent_id)
        .unwrap_or_default();

    let max_history = manifest.memory.max_history_messages.unwrap_or(100);
    let mut messages_processed: u64 = 0;
    // Throttles repeated SessionStoreError audit events so a broken store
    // doesn't drown the log in per-turn failures.
    let mut last_session_store_error: Option<Instant> = None;

    // Pre-resolve system prompt once for the loop lifetime
    let system_prompt_str = match &manifest.system_prompt {
        PromptSource::Inline(s) => s.clone(),
        PromptSource::File(path) => {
            std::fs::read_to_string(path)
                .unwrap_or_else(|_| format!("Failed to read prompt from {}", path.display()))
        }
    };

    audit_log.record(AuditEvent::new(
        agent_id,
        AuditEventKind::AgentLoopStarted {
            lifecycle: "persistent".into(),
        },
    ));

    loop {
        tokio::select! {
            msg = message_rx.recv() => {
                let Some(msg) = msg else { break; };

                let trace_id = msg.metadata.trace_id;

                audit_log.record(AuditEvent::new(
                    agent_id,
                    AuditEventKind::AgentMessageReceived {
                        trace_id,
                        method: msg.method.clone(),
                    },
                ));

                let user_input = extract_user_message(&msg);

                // Prepare context (summarize if needed) BEFORE calling executor
                let (messages_for_llm, prompt_for_llm) = if let Some(ref cm) = context_manager {
                    match cm.prepare_context(&history, &system_prompt_str).await {
                        Ok(prepared) => {
                            // Handle summarization success: archive, then mutate history
                            if let Some(ref summ) = prepared.summarization {
                                // 1. Archive the old messages FIRST
                                let segment = ArchiveSegment {
                                    source_range: summ.source_range,
                                    messages: summ.archived_messages.clone(),
                                    archived_at: chrono::Utc::now(),
                                };
                                let _ = session_store.archive_segment(&agent_id, &segment);

                                // 2. Drain old messages from history, insert summary
                                let (_, end) = summ.source_range;
                                history.drain(..=end);
                                history.insert(0, summ.summary.clone());

                                // 3. Clear and rewrite session store with new history.
                                // NOTE: clear+append is not atomic. If append partially persists
                                // before failing, the on-disk store can diverge from the in-memory
                                // history until the next summarization cycle. A transactional
                                // replace() on the SessionStore trait would close this; deferred
                                // as a follow-up.
                                let should_emit = match last_session_store_error {
                                    Some(prev) if prev.elapsed() < SESSION_STORE_ERROR_THROTTLE => false,
                                    _ => { last_session_store_error = Some(Instant::now()); true }
                                };
                                if let Err(e) = session_store.clear(&agent_id) {
                                    if should_emit {
                                        audit_log.record(AuditEvent::new(
                                            agent_id,
                                            AuditEventKind::SessionStoreError {
                                                operation: "clear".to_string(),
                                                message: e.to_string(),
                                            },
                                        ));
                                    }
                                    // Do NOT append — writing over a non-cleared store would
                                    // mix old and new history. The in-memory history is still
                                    // intact and will be rewritten next cycle.
                                } else if let Err(e) = session_store.append(&agent_id, &history) {
                                    if should_emit {
                                        audit_log.record(AuditEvent::new(
                                            agent_id,
                                            AuditEventKind::SessionStoreError {
                                                operation: "append".to_string(),
                                                message: e.to_string(),
                                            },
                                        ));
                                    }
                                }

                                // 4. Emit audit event
                                audit_log.record(AuditEvent::new(
                                    agent_id,
                                    AuditEventKind::ContextSummarized {
                                        messages_summarized: summ.archived_messages.len() as u32,
                                        source_range: summ.source_range,
                                        tokens_saved_estimate: summ.tokens_saved_estimate,
                                    },
                                ));
                            }

                            // Handle summarization failure (non-fatal — context is
                            // returned uncompressed). Emit a structured audit event
                            // so operators can see the failure and its category.
                            if let Some(ref failure) = prepared.summarization_failure {
                                audit_log.record(AuditEvent::new(
                                    agent_id,
                                    AuditEventKind::ContextSummarizationFailed {
                                        reason: failure.message.clone(),
                                        fallback: "original_history".into(),
                                        failure_kind: failure.kind.clone(),
                                    },
                                ));
                            }

                            (prepared.messages, prepared.system_prompt)
                        }
                        Err(reason) => {
                            // prepare_context currently never returns Err; this
                            // branch is kept for forward compatibility.
                            audit_log.record(AuditEvent::new(
                                agent_id,
                                AuditEventKind::ContextSummarizationFailed {
                                    reason: reason.clone(),
                                    fallback: "original_history".into(),
                                    failure_kind: aaos_core::SummarizationFailureKind::BoundarySelection,
                                },
                            ));
                            (history.clone(), system_prompt_str.clone())
                        }
                    }
                } else {
                    (history.clone(), system_prompt_str.clone())
                };

                let result = executor
                    .run_with_history_and_prompt(
                        agent_id, &manifest, &user_input,
                        &messages_for_llm, &prompt_for_llm,
                    )
                    .await;

                match result.stop_reason {
                    ExecutionStopReason::Error(ref err_msg) => {
                        audit_log.record(AuditEvent::new(
                            agent_id,
                            AuditEventKind::AgentExecutionCompleted {
                                stop_reason: format!("error: {err_msg}"),
                                total_iterations: result.iterations,
                            },
                        ));

                        let error_response = McpMessage::new(
                            agent_id, agent_id, "error", serde_json::json!({})
                        ).respond_err(agent_id, -32603, err_msg.clone());
                        let _ = router.respond(trace_id, error_response);
                    }
                    _ => {
                        history.extend(result.transcript_delta.iter().cloned());

                        if history.len() > max_history {
                            history.drain(..history.len() - max_history);
                        }

                        let _ = session_store.append(&agent_id, &result.transcript_delta);

                        audit_log.record(AuditEvent::new(
                            agent_id,
                            AuditEventKind::AgentExecutionCompleted {
                                stop_reason: result.stop_reason.to_string(),
                                total_iterations: result.iterations,
                            },
                        ));

                        let success_response = McpMessage::new(
                            agent_id, agent_id, "result", serde_json::json!({})
                        ).respond_ok(agent_id, serde_json::json!({
                            "response": result.response,
                            "usage": {
                                "input_tokens": result.usage.input_tokens,
                                "output_tokens": result.usage.output_tokens,
                            },
                            "iterations": result.iterations,
                            "stop_reason": result.stop_reason.to_string(),
                        }));
                        let _ = router.respond(trace_id, success_response);
                    }
                }

                messages_processed += 1;
            }
            cmd = command_rx.recv() => {
                match cmd {
                    Some(AgentCommand::Stop) => break,
                    Some(AgentCommand::Pause) => {
                        loop {
                            match command_rx.recv().await {
                                Some(AgentCommand::Resume) => break,
                                Some(AgentCommand::Stop) | None => {
                                    audit_log.record(AuditEvent::new(
                                        agent_id,
                                        AuditEventKind::AgentLoopStopped {
                                            reason: "stopped_while_paused".into(),
                                            messages_processed,
                                        },
                                    ));
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                    Some(AgentCommand::Resume) => {}
                    None => break,
                }
            }
        }
    }

    audit_log.record(AuditEvent::new(
        agent_id,
        AuditEventKind::AgentLoopStopped {
            reason: "normal".into(),
            messages_processed,
        },
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_core::{InMemoryAuditLog, TokenUsage};
    use aaos_llm::{
        CompletionRequest, CompletionResponse, ContentBlock, LlmClient, LlmResult, LlmStopReason,
        ExecutorConfig,
    };
    use async_trait::async_trait;
    use std::sync::Mutex;
    use crate::session::InMemorySessionStore;

    struct MockLlmClient {
        responses: Mutex<Vec<LlmResult<CompletionResponse>>>,
    }

    impl MockLlmClient {
        fn with_text(text: &str) -> Self {
            Self {
                responses: Mutex::new(vec![Ok(CompletionResponse {
                    content: vec![ContentBlock::Text { text: text.into() }],
                    stop_reason: LlmStopReason::EndTurn,
                    usage: TokenUsage { input_tokens: 10, output_tokens: 5 },
                })]),
            }
        }

        fn with_responses(responses: Vec<LlmResult<CompletionResponse>>) -> Self {
            Self {
                responses: Mutex::new(responses),
            }
        }
    }

    #[async_trait]
    impl LlmClient for MockLlmClient {
        fn max_context_tokens(&self, _model: &str) -> u32 {
            200_000
        }

        async fn complete(&self, _req: CompletionRequest) -> LlmResult<CompletionResponse> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Ok(CompletionResponse {
                    content: vec![ContentBlock::Text { text: "default".into() }],
                    stop_reason: LlmStopReason::EndTurn,
                    usage: TokenUsage { input_tokens: 5, output_tokens: 3 },
                })
            } else {
                responses.remove(0)
            }
        }
    }

    struct MockAgentServices;

    #[async_trait]
    impl aaos_core::AgentServices for MockAgentServices {
        async fn invoke_tool(&self, _: AgentId, _: &str, _: serde_json::Value) -> aaos_core::Result<serde_json::Value> {
            Ok(serde_json::json!({"ok": true}))
        }
        async fn send_message(&self, _: AgentId, _: serde_json::Value) -> aaos_core::Result<serde_json::Value> {
            Ok(serde_json::json!({"status": "delivered"}))
        }
        async fn request_approval(&self, _: AgentId, _: String, _: std::time::Duration) -> aaos_core::Result<aaos_core::ApprovalResult> {
            Ok(aaos_core::ApprovalResult::Approved)
        }
        async fn report_usage(&self, _: AgentId, _: TokenUsage) -> aaos_core::Result<()> {
            Ok(())
        }
        async fn list_tools(&self, _: AgentId) -> aaos_core::Result<Vec<aaos_core::ToolDefinition>> {
            Ok(vec![])
        }

        async fn send_and_wait(
            &self,
            _: AgentId,
            _: AgentId,
            _: String,
            _: serde_json::Value,
            _: std::time::Duration,
        ) -> aaos_core::Result<serde_json::Value> {
            Err(aaos_core::CoreError::Ipc("not implemented in mock".into()))
        }
    }

    fn test_manifest() -> AgentManifest {
        AgentManifest::from_yaml(r#"
name: persistent-test
model: claude-haiku-4-5-20251001
system_prompt: "You are a test assistant."
lifecycle: persistent
"#).unwrap()
    }

    #[tokio::test]
    async fn persistent_loop_processes_message() {
        let agent_id = AgentId::new();
        let (msg_tx, msg_rx) = mpsc::channel(64);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let audit_log = Arc::new(InMemoryAuditLog::new());
        let session_store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
        let router = Arc::new(MessageRouter::new(audit_log.clone(), |_, _| true));

        let llm: Arc<dyn LlmClient> = Arc::new(MockLlmClient::with_text("Hello back!"));
        let services: Arc<dyn aaos_core::AgentServices> = Arc::new(MockAgentServices);
        let executor = AgentExecutor::new(llm, services, ExecutorConfig::default());

        let handle = tokio::spawn(persistent_agent_loop(
            agent_id, test_manifest(), msg_rx, cmd_rx,
            executor, session_store.clone(), router.clone(), audit_log.clone(),
            None,
        ));

        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        let msg = McpMessage::new(AgentId::new(), agent_id, "agent.run",
            serde_json::json!({"message": "Hello"}));
        let trace_id = msg.metadata.trace_id;
        router.register_pending(trace_id, resp_tx);
        msg_tx.send(msg).await.unwrap();

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            resp_rx,
        ).await.unwrap().unwrap();

        assert!(response.result.is_some());
        let result = response.result.unwrap();
        assert_eq!(result["response"], "Hello back!");

        cmd_tx.send(AgentCommand::Stop).await.unwrap();
        handle.await.unwrap();

        let stored = session_store.load(&agent_id).unwrap();
        assert!(!stored.is_empty());
    }

    #[tokio::test]
    async fn persistent_loop_survives_executor_error() {
        let agent_id = AgentId::new();
        let (msg_tx, msg_rx) = mpsc::channel(64);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let audit_log = Arc::new(InMemoryAuditLog::new());
        let session_store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
        let router = Arc::new(MessageRouter::new(audit_log.clone(), |_, _| true));

        let llm: Arc<dyn LlmClient> = Arc::new(MockLlmClient::with_responses(vec![
            Err(aaos_llm::LlmError::Other("simulated failure".into())),
            Ok(CompletionResponse {
                content: vec![ContentBlock::Text { text: "Recovered!".into() }],
                stop_reason: LlmStopReason::EndTurn,
                usage: TokenUsage { input_tokens: 10, output_tokens: 5 },
            }),
        ]));
        let services: Arc<dyn aaos_core::AgentServices> = Arc::new(MockAgentServices);
        let executor = AgentExecutor::new(llm, services, ExecutorConfig::default());

        let handle = tokio::spawn(persistent_agent_loop(
            agent_id, test_manifest(), msg_rx, cmd_rx,
            executor, session_store.clone(), router.clone(), audit_log.clone(),
            None,
        ));

        // First message: will fail
        let msg1 = McpMessage::new(AgentId::new(), agent_id, "agent.run",
            serde_json::json!({"message": "Fail please"}));
        let trace1 = msg1.metadata.trace_id;
        let (tx1, rx1) = tokio::sync::oneshot::channel();
        router.register_pending(trace1, tx1);
        msg_tx.send(msg1).await.unwrap();

        let resp1 = tokio::time::timeout(std::time::Duration::from_secs(5), rx1)
            .await.unwrap().unwrap();
        assert!(resp1.error.is_some());

        // Second message: should succeed (loop survived)
        let msg2 = McpMessage::new(AgentId::new(), agent_id, "agent.run",
            serde_json::json!({"message": "Recover"}));
        let trace2 = msg2.metadata.trace_id;
        let (tx2, rx2) = tokio::sync::oneshot::channel();
        router.register_pending(trace2, tx2);
        msg_tx.send(msg2).await.unwrap();

        let resp2 = tokio::time::timeout(std::time::Duration::from_secs(5), rx2)
            .await.unwrap().unwrap();
        assert!(resp2.result.is_some());
        assert_eq!(resp2.result.unwrap()["response"], "Recovered!");

        cmd_tx.send(AgentCommand::Stop).await.unwrap();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn persistent_loop_stop_clean_shutdown() {
        let agent_id = AgentId::new();
        let (_msg_tx, msg_rx) = mpsc::channel(64);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let audit_log = Arc::new(InMemoryAuditLog::new());
        let session_store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
        let router = Arc::new(MessageRouter::new(audit_log.clone(), |_, _| true));

        let llm: Arc<dyn LlmClient> = Arc::new(MockLlmClient::with_text("unused"));
        let services: Arc<dyn aaos_core::AgentServices> = Arc::new(MockAgentServices);
        let executor = AgentExecutor::new(llm, services, ExecutorConfig::default());

        let handle = tokio::spawn(persistent_agent_loop(
            agent_id, test_manifest(), msg_rx, cmd_rx,
            executor, session_store, router, audit_log.clone(),
            None,
        ));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        cmd_tx.send(AgentCommand::Stop).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await.unwrap().unwrap();

        let events = audit_log.events();
        let loop_started = events.iter().any(|e| matches!(&e.event, AuditEventKind::AgentLoopStarted { .. }));
        let loop_stopped = events.iter().any(|e| matches!(&e.event, AuditEventKind::AgentLoopStopped { .. }));
        assert!(loop_started);
        assert!(loop_stopped);
    }

    #[test]
    fn session_store_error_audit_kind_serializes() {
        // Fix 2 introduces SessionStoreError — verify it serializes with the
        // expected fields. End-to-end testing of the throttle and clear-failure
        // path requires driving summarization through persistent_agent_loop,
        // which needs a live LLM mock set up for it; this unit test at least
        // pins the audit event shape.
        let ev = AuditEvent::new(
            AgentId::new(),
            AuditEventKind::SessionStoreError {
                operation: "clear".to_string(),
                message: "disk full".to_string(),
            },
        );
        let json = serde_json::to_string(&ev.event).unwrap();
        assert!(json.contains("\"kind\":\"session_store_error\""), "unexpected: {json}");
        assert!(json.contains("\"operation\":\"clear\""));
        assert!(json.contains("\"message\":\"disk full\""));
    }
}
