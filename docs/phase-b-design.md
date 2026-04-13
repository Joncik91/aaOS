# Phase B: Persistent Agents & Request-Response IPC — Design Spec

## Overview

Phase B extends aaOS from ephemeral single-shot agents to persistent agents that run continuously, exchange request-response messages, and maintain conversation history across turns and restarts.

**Constraint:** Nothing breaks for ephemeral agents. Every change is additive. The existing spawn-run-die path stays exactly as-is. All 111 existing tests must pass without modification.

## Prerequisites (Phase A, complete)

- Capability-based security with unforgeable tokens
- Tool invocation with two-level enforcement
- Audit trail with 14 event kinds and causal tracing
- Parent-child spawning with capability narrowing
- Human-in-the-loop approval queue
- MCP message routing (fire-and-forget)

## What Already Exists (~40% ready)

| Component | Status | Location |
|-----------|--------|----------|
| `Lifecycle::Persistent` enum | Defined, parsed from YAML, ignored at runtime | `aaos-core/src/manifest.rs:15-23` |
| `message_rx` / `response_rx` channels | Allocated at spawn via `router.register()`, never consumed | `aaos-runtime/src/process.rs:58-59` |
| `ApprovalQueue` oneshot pattern | Implements request-response: sender blocks on `oneshot::Receiver`, responder sends via `oneshot::Sender` | `agentd/src/approval.rs:74-115` |
| `AgentCommand` channel | Exists for Pause/Resume/Stop; only Stop partially wired | `aaos-runtime/src/process.rs:56-57` |
| `SupervisorConfig` + backoff | Restart policy with Always/OnFailure/Never + backoff calculation | `aaos-runtime/src/scheduler.rs` |
| `RoundRobinScheduler` | Implemented with priority support, not used | `aaos-runtime/src/scheduler.rs:35-80` |

---

## Sub-Spec 1: Persistent Agent Lifecycle

### Goal

Agents declared as `lifecycle: persistent` run continuously in a background task, consuming messages from their `message_rx` channel in a loop.

### Changes to `AgentRegistry::spawn()`

Branch on `manifest.lifecycle` after creating the `AgentProcess`:

- **`OnDemand` (default):** No change. Agent is registered, waits for `agent.run` calls.
- **`Persistent`:** Take ownership of `message_rx` and `command_rx` from `AgentProcess` (via `Option::take()`). Spawn a `tokio::task` running `persistent_agent_loop()`. Store the `JoinHandle<()>` in the registry for cleanup.

Extract shared setup (executor creation, capability injection, audit logging) into a helper function used by both paths.

### The Persistent Agent Loop

```rust
async fn persistent_agent_loop(
    agent_id: AgentId,
    manifest: AgentManifest,
    mut message_rx: mpsc::Receiver<McpMessage>,
    mut command_rx: mpsc::Receiver<AgentCommand>,
    services: Arc<dyn AgentServices>,
    executor: AgentExecutor,
    session_store: Arc<dyn SessionStore>,  // Sub-spec 3; Option in sub-spec 1
    router: Arc<MessageRouter>,
    audit_log: Arc<dyn AuditLog>,
) {
    // Load conversation history (sub-spec 3; empty vec in sub-spec 1)
    let mut history: Vec<Message> = session_store
        .load(&agent_id)
        .unwrap_or_default();

    loop {
        tokio::select! {
            msg = message_rx.recv() => {
                let Some(msg) = msg else { break; }; // Channel closed

                let trace_id = msg.metadata.trace_id;

                // Extract user message from McpMessage params
                // McpMessage.params is a serde_json::Value; extract "message" field as string
                // e.g. params: {"message": "What is 2+2?"} -> "What is 2+2?"
                let user_input = extract_user_message(&msg);

                // Run executor with history
                let result = executor
                    .run_with_history(&agent_id, &manifest, &user_input, &history)
                    .await;

                match result {
                    Ok(execution_result) => {
                        // Append new messages to history (sub-spec 3)
                        // ...

                        // Send response if caller is waiting (sub-spec 2)
                        if let Some(trace_id) = trace_id {
                            let response = McpResponse::success(/* ... */);
                            let _ = router.respond(trace_id, response);
                        }
                    }
                    Err(e) => {
                        // Log error, do NOT crash the loop
                        audit_log.record(AuditEvent::new(
                            agent_id,
                            AuditEventKind::AgentExecutionCompleted {
                                stop_reason: "error".into(),
                                total_iterations: 0,
                            },
                        ));

                        // Send error response if caller is waiting
                        if let Some(trace_id) = trace_id {
                            let response = McpResponse::error(/* ... */);
                            let _ = router.respond(trace_id, response);
                        }

                        // Apply supervisor backoff if configured
                        // Continue loop — don't die
                    }
                }
            }
            cmd = command_rx.recv() => {
                match cmd {
                    Some(AgentCommand::Stop) => break,
                    Some(AgentCommand::Pause) => {
                        // Wait for Resume; messages buffer in message_rx
                        loop {
                            if let Some(AgentCommand::Resume) = command_rx.recv().await {
                                break;
                            }
                        }
                    }
                    Some(AgentCommand::Resume) => {} // Already running
                    None => break, // Command channel closed
                }
            }
        }
    }

    // Cleanup: transition state to Stopped, log audit event
}
```

### Error Recovery

- If `executor.run_with_history()` returns an error: log it, send error response to caller (if waiting), continue the loop.
- If the executor panics: the `tokio::task` catches it (wrap in `AssertUnwindSafe` + `catch_unwind`, or let tokio's default panic handler log it). The registry detects the task has exited via `JoinHandle` and can optionally restart per `SupervisorConfig`.
- The loop itself never panics on channel operations — all `recv()` calls handle `None` (channel closed) by breaking.

### Stop/Pause Semantics

- **Stop during idle:** immediate.
- **Stop during execution:** deferred until current turn completes. Acceptable for v1 — turns take seconds. True mid-execution cancellation requires abortable tasks (Phase E).
- **Pause:** messages buffer in `message_rx` (capacity 64). If the buffer fills while paused, `agent.run` callers get a `MailboxFull` error.

### API Behavior Changes

| Endpoint | Ephemeral Agent (unchanged) | Persistent Agent |
|----------|---------------------------|-----------------|
| `agent.spawn` | Creates process, waits for `agent.run` | Creates process, starts background loop |
| `agent.run` | Blocks, runs executor, returns result | Delivers message to inbox, returns `{"trace_id": "..."}` |
| `agent.stop` | Transitions to Stopped | Sends Stop command, awaits task exit |
| `agent.status` | Running/Stopped | Running (idle) / Running (processing) / Paused / Stopped |

### New Audit Events

```rust
AgentLoopStarted { lifecycle: String }
AgentLoopStopped { reason: String, messages_processed: u64 }
AgentMessageReceived { trace_id: Uuid, method: String }
```

### Tests

- Spawn persistent agent, verify Running state and task is alive
- Deliver message via `agent.run`, verify trace_id returned immediately
- Verify message reaches agent and is processed (check audit log)
- Stop persistent agent, verify clean shutdown
- Persistent agent survives executor error (doesn't crash loop)
- Ephemeral agent spawn/run unchanged (regression)
- Channel full → `MailboxFull` error returned to caller

---

## Sub-Spec 2: Request-Response IPC

### Goal

An agent can send a message to a persistent agent and block until it responds, with a timeout.

### Pending-Response Map (Not Oneshot in Message)

`McpMessage` stays serializable. The response channel lives in the router, not the message.

**New field on `MessageRouter`:**

```rust
pub struct MessageRouter {
    // ... existing fields ...
    pending_responses: DashMap<Uuid, oneshot::Sender<McpResponse>>,
}
```

### Send-and-Wait Flow

1. Caller invokes `send_and_wait(agent_id, recipient, message, timeout)`
2. `InProcessAgentServices` creates an `McpMessage` with a `trace_id`
3. Creates a `oneshot::channel()`
4. Stores the sender in `router.pending_responses` keyed by `trace_id`
5. Routes the message normally via `router.route()` (existing fire-and-forget delivery)
6. Awaits `oneshot::Receiver` with `tokio::time::timeout(duration)`
7. On response: return the value
8. On timeout: remove entry from `pending_responses`, return `Timeout` error

### Respond Flow

When the persistent agent loop finishes processing a message:

1. Builds an `McpResponse` with the result
2. Calls `router.respond(trace_id, response)`
3. `respond()` removes the entry from `pending_responses` and sends via `oneshot::Sender`
4. If no entry exists (caller timed out or was fire-and-forget): response is dropped, logged at debug level

### New Method on `MessageRouter`

```rust
impl MessageRouter {
    pub fn register_pending(&self, trace_id: Uuid, tx: oneshot::Sender<McpResponse>) {
        self.pending_responses.insert(trace_id, tx);
    }

    pub fn respond(&self, trace_id: Uuid, response: McpResponse) -> bool {
        if let Some((_, tx)) = self.pending_responses.remove(&trace_id) {
            tx.send(response).is_ok()
        } else {
            false // No one waiting (timed out or fire-and-forget)
        }
    }
}
```

### New Method on `AgentServices`

```rust
async fn send_and_wait(
    &self,
    agent_id: AgentId,
    recipient: AgentId,
    method: String,
    params: Value,
    timeout: Duration,
) -> Result<Value>;
```

Existing `send_message` is unchanged — still fire-and-forget.

### Failure Modes

| Condition | Error Returned |
|-----------|---------------|
| Recipient not found / stopped | `AgentNotFound` |
| Recipient mailbox full (64 messages buffered) | `MailboxFull` |
| Execution timeout | `Timeout` |
| Caller lacks `MessageSend` capability | `CapabilityDenied` |
| Caller drops before response | Response dropped, logged at debug |

### Tests

- Send-and-wait to persistent agent, verify response received with correct trace_id
- Timeout when persistent agent is slow (mock slow executor)
- Fire-and-forget (`send_message`) still works unchanged
- Capability check still applies to `send_and_wait`
- Response to timed-out caller is dropped gracefully (no panic)
- Send-and-wait to non-existent agent returns `AgentNotFound`

---

## Sub-Spec 3: Conversation Persistence

### Goal

Persistent agents maintain conversation history across messages and survive daemon restarts.

### Memory Model

- History loaded from disk **once** at agent startup (persistent loop init)
- Kept **in memory** as `Vec<Message>` inside the loop
- After each turn: **append** new messages to JSONL file on disk
- On daemon restart: load from disk again

Never loads from disk during normal operation. Disk is the durability layer, memory is the working set.

### SessionStore Trait

```rust
// In aaos-core or aaos-runtime
pub trait SessionStore: Send + Sync {
    fn load(&self, agent_id: &AgentId) -> Result<Vec<Message>>;
    fn append(&self, agent_id: &AgentId, messages: &[Message]) -> Result<()>;
    fn clear(&self, agent_id: &AgentId) -> Result<()>;
}
```

### JsonlSessionStore Implementation

```rust
pub struct JsonlSessionStore {
    data_dir: PathBuf, // e.g. /var/lib/aaos/sessions/
}
```

- `load()`: Read `{data_dir}/{agent_id}.jsonl`, deserialize each line as `Message`. Missing file or empty file returns empty vec.
- `append()`: Open file in append mode (create if needed), serialize each `Message` as one JSON line, write with newline.
- `clear()`: Truncate file.

**Single-writer invariant:** Each agent has exactly one persistent loop task. No concurrent writers. This is guaranteed by architecture (one loop per agent ID). Documented, not enforced with locks.

### Executor Changes

`AgentExecutor::run()` gains:
- Input: `prior_messages: Vec<Message>` — prepended to conversation before new message
- Output: `transcript_delta: Vec<Message>` — the new messages generated this turn (user input, assistant responses, tool calls/results)

The `aaos_llm::Message` enum needs `#[derive(Serialize, Deserialize)]` added.

### Conversation Length Management

Config field in manifest (under `memory`):

```yaml
memory:
  max_history_messages: 100
```

On load: if history exceeds `max_history_messages`, keep only the most recent N. Periodic compaction: after every 10th append, rewrite the JSONL file with only the kept messages (avoids unbounded file growth).

### Integration with Persistent Loop

```rust
// In persistent_agent_loop, after executor returns:
if let Ok(result) = execution_result {
    // Append new messages to in-memory history
    history.extend(result.transcript_delta.iter().cloned());

    // Trim if over limit
    let max = manifest.memory.max_history_messages.unwrap_or(100);
    if history.len() > max {
        history.drain(..history.len() - max);
    }

    // Persist to disk
    session_store.append(&agent_id, &result.transcript_delta)?;

    // Compact periodically
    turns_since_compact += 1;
    if turns_since_compact >= 10 {
        session_store.clear(&agent_id)?;
        session_store.append(&agent_id, &history)?;
        turns_since_compact = 0;
    }
}
```

### Tests

- Write messages via `append()`, read back via `load()`, verify content
- Append across multiple calls, verify ordering
- Load after simulated restart (new `JsonlSessionStore` instance)
- Empty/missing file returns empty vec
- Conversation carries context: persistent agent receives 3 messages, each builds on prior context
- History trimming: send 150 messages with max_history=100, verify only last 100 loaded
- Compaction: verify file size doesn't grow unboundedly

---

## Files Changed

### New Files

| File | Crate | Purpose |
|------|-------|---------|
| `persistent.rs` | `aaos-runtime` | `persistent_agent_loop()` function |
| `session.rs` | `aaos-runtime` | `SessionStore` trait + `JsonlSessionStore` |

### Modified Files

| File | Crate | Changes |
|------|-------|---------|
| `registry.rs` | `aaos-runtime` | Branch on `Lifecycle::Persistent` in `spawn()`, store `JoinHandle` |
| `process.rs` | `aaos-runtime` | Add `task_handle: Option<JoinHandle<()>>`, use `Option::take()` for channels |
| `router.rs` | `aaos-ipc` | Add `pending_responses` map, `register_pending()`, `respond()` methods |
| `services.rs` | `aaos-runtime` | Add `send_and_wait()` to `InProcessAgentServices`, inject `SessionStore` |
| `services.rs` | `aaos-core` | Add `send_and_wait()` to `AgentServices` trait |
| `executor.rs` | `aaos-llm` | Add `prior_messages` input, return `transcript_delta` |
| `types.rs` | `aaos-llm` | Add `Serialize`/`Deserialize` derives to `Message` enum |
| `audit.rs` | `aaos-core` | Add 3 new `AuditEventKind` variants |
| `server.rs` | `agentd` | Adjust `handle_agent_run()` for persistent agents, inject `SessionStore` |
| `manifest.rs` | `aaos-core` | Add `max_history_messages` to `MemoryConfig` |

### Not Changed

- `aaos-tools` — no tool changes
- `approval.rs` — approval system unchanged
- `scheduler.rs` — scheduler not activated in Phase B

---

## Out of Scope

- Inference scheduling / concurrent message processing (Phase E)
- Episodic memory / vector search (Phase C)
- Mid-execution cancellation / abortable tasks (Phase E)
- Message deduplication (trace_id exists, defer to Phase C)
- Multiple persistent agent groups / arbitration (Phase B.5 / NarrativeEngine integration)
- Supervision dashboard (Phase D)

---

## Success Criteria

1. **Persistent agent stays alive:** Spawn with `lifecycle: persistent`, send 10 messages over 10 seconds, agent processes all sequentially.
2. **Request-response works:** Agent A calls `send_and_wait` to persistent agent B, receives the correct response within timeout.
3. **Conversation persists:** Send 5 messages to persistent agent, restart daemon, send 6th — agent's response references content from earlier messages.
4. **Ephemeral agents unchanged:** All 111 existing tests pass without modification.
5. **Crash resilience:** Persistent agent loop survives an executor error, logs it, continues processing next message.
6. **Timeout handling:** `send_and_wait` with a 1-second timeout to a slow agent returns `Timeout` error cleanly.
7. **Backpressure:** Sending 100 messages rapidly to a persistent agent returns `MailboxFull` after channel capacity (64) is reached.
