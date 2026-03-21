# Agent Messaging & Human Approval Queue — Design Spec

**Date:** 2026-03-21
**Status:** Approved
**Scope:** Wire fire-and-forget messaging through existing router + approval queue with daemon API
**Depends on:** Execution loop spec (2026-03-20), Tools spec (2026-03-21)

## Context

The demo needs two remaining pieces: agents communicating via typed MCP messages, and a human-in-the-loop approval step that pauses the workflow. The messaging infrastructure (router, channels, McpMessage types) already exists in `aaos-ipc` but isn't wired into the agent lifecycle. The approval mechanism (`request_approval`) currently auto-approves.

## Design Decisions

1. **Fire-and-forget messaging only.** No request-response between agents. In Phase A, agents are ephemeral — they spawn, execute, and die. There are no running peers to ask questions to. `spawn_agent` handles request-response via spawn-execute-return. Fire-and-forget proves the IPC layer works and shows messages in the audit trail. Full request-response is Phase B (persistent agents).

2. **Approval via Unix socket API, not stdout.** Same interface a future web dashboard would use. Architecturally correct — not a throwaway mechanism.

3. **`ApprovalService` trait in `aaos-core`.** Dependency direction: trait in core, `ApprovalQueue` implementation in agentd, `NoOpApprovalService` default. Same pattern as `LlmClient`.

4. **Approval check in `InProcessAgentServices`, not `ToolInvocation`.** Capability enforcement ("can this agent use this tool?") and human oversight ("should this action proceed?") are separate concerns. `ToolInvocation` handles the former, `InProcessAgentServices` handles the latter before delegating to `ToolInvocation`.

5. **Approval-required tools declared in manifest.** New `approval_required` field lists tool names that need human approval before execution.

## Deliverables

### 1. Wire Messaging into Agent Lifecycle

**In `aaos-runtime`.**

`AgentRegistry` gets a reference to `Arc<MessageRouter>` at construction time. On spawn, it calls `router.register(agent_id)` and stores the returned receivers in `AgentProcess`. On stop, it calls `router.unregister(&agent_id)`.

**Receiver storage:** `AgentProcess` gains two new fields:
```rust
pub message_rx: Option<mpsc::Receiver<McpMessage>>,
pub response_rx: Option<mpsc::Receiver<McpResponse>>,
```
Set by the registry after calling `router.register()`. In Phase A (fire-and-forget), these receivers are stored but not actively read — messages buffer in the channel. In Phase B (persistent agents with message loops), the executor will consume them. Storing them now prevents the channels from being dropped (which would make senders fail).

**`send_message` trait signature change:** The trait method gains `agent_id` for sender identification:
```rust
async fn send_message(&self, agent_id: AgentId, message: Value) -> Result<Value>;
```

The `message` Value is NOT a full `McpMessage`. It's a simple envelope the agent constructs:
```json
{ "recipient": "<agent-id-string>", "method": "notify", "params": { ... } }
```

`InProcessAgentServices::send_message` implementation:
1. Parse `recipient` from the Value as an `AgentId`
2. Parse `method` and `params` from the Value
3. Construct a full `McpMessage::new(agent_id, recipient, method, params)`
4. Call `router.route(message).await`
5. Return `Ok(json!({"status": "delivered"}))` on success

This keeps the `AgentServices` trait free of `aaos-ipc` types while giving the implementation everything it needs.

**Registry constructor change:**

```rust
pub fn new(audit_log: Arc<dyn AuditLog>, router: Arc<MessageRouter>) -> Self
```

**Call sites that break (4 total):**
- `crates/agentd/src/server.rs` — `Server::new()`: pass the router (already constructed there)
- `crates/agentd/src/spawn_tool.rs` — test `setup()`: construct a test router
- `crates/aaos-runtime/src/services.rs` — test `setup()`: construct a test router
- `crates/aaos-runtime/src/registry.rs` — test `test_registry()`: construct a test router

Each test site needs: `let router = Arc::new(MessageRouter::new(audit_log.clone(), |_, _| true));` and pass it to `AgentRegistry::new(audit_log, router)`.

### 2. `ApprovalService` Trait

**In `aaos-core/src/services.rs`.**

```rust
#[async_trait]
pub trait ApprovalService: Send + Sync {
    async fn request(
        &self,
        agent_id: AgentId,
        agent_name: String,
        description: String,
        tool: Option<String>,
        input: Option<Value>,
    ) -> Result<ApprovalResult>;
}
```

**`NoOpApprovalService`** — always returns `Approved`. Default when no approval queue is configured.

```rust
pub struct NoOpApprovalService;

#[async_trait]
impl ApprovalService for NoOpApprovalService {
    async fn request(
        &self,
        _agent_id: AgentId,
        _agent_name: String,
        _description: String,
        _tool: Option<String>,
        _input: Option<Value>,
    ) -> Result<ApprovalResult> {
        Ok(ApprovalResult::Approved)
    }
}
```

### 3. `ApprovalQueue` Implementation

**In `agentd/src/approval.rs`.**

```rust
pub struct ApprovalQueue {
    pending: DashMap<Uuid, PendingApproval>,
}

struct PendingApproval {
    pub id: Uuid,
    pub agent_id: AgentId,
    pub agent_name: String,
    pub description: String,
    pub tool: Option<String>,
    pub input: Option<Value>,
    pub timestamp: DateTime<Utc>,
    response_tx: oneshot::Sender<ApprovalResult>,
}
```

**`ApprovalService` implementation:**

```rust
#[async_trait]
impl ApprovalService for ApprovalQueue {
    async fn request(
        &self,
        agent_id: AgentId,
        agent_name: String,
        description: String,
        tool: Option<String>,
        input: Option<Value>,
    ) -> Result<ApprovalResult> {
        let id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();

        self.pending.insert(id, PendingApproval {
            id,
            agent_id,
            agent_name,
            description,
            tool,
            input,
            timestamp: Utc::now(),
            response_tx: tx,
        });

        // Block until human responds or sender is dropped
        match rx.await {
            Ok(result) => Ok(result),
            Err(_) => {
                // Sender dropped (daemon shutting down, queue dropped)
                self.pending.remove(&id);
                Ok(ApprovalResult::Denied {
                    reason: "approval service unavailable".into(),
                })
            }
        }
    }
}
```

**`list` and `respond` methods:**

```rust
impl ApprovalQueue {
    pub fn new() -> Self {
        Self { pending: DashMap::new() }
    }

    /// List all pending approval requests (for the API).
    pub fn list(&self) -> Vec<ApprovalInfo> {
        self.pending.iter().map(|entry| {
            let p = entry.value();
            ApprovalInfo {
                id: p.id,
                agent_id: p.agent_id,
                agent_name: p.agent_name.clone(),
                description: p.description.clone(),
                tool: p.tool.clone(),
                input: p.input.clone(),
                timestamp: p.timestamp,
            }
        }).collect()
    }

    /// Respond to a pending approval request.
    pub fn respond(&self, id: Uuid, decision: ApprovalResult) -> Result<()> {
        match self.pending.remove(&id) {
            Some((_, pending)) => {
                let _ = pending.response_tx.send(decision);
                Ok(())
            }
            None => Err(CoreError::Ipc(format!("no pending approval with id {id}"))),
        }
    }
}

/// Serializable info about a pending approval (for API responses).
#[derive(Debug, Clone, Serialize)]
pub struct ApprovalInfo {
    pub id: Uuid,
    pub agent_id: AgentId,
    pub agent_name: String,
    pub description: String,
    pub tool: Option<String>,
    pub input: Option<Value>,
    pub timestamp: DateTime<Utc>,
}
```

### 4. Approval Check in `InProcessAgentServices`

**In `aaos-runtime/src/services.rs`.**

`InProcessAgentServices` gains an `Arc<dyn ApprovalService>` field and an approval-required tool list.

```rust
pub struct InProcessAgentServices {
    registry: Arc<AgentRegistry>,
    tool_invocation: Arc<ToolInvocation>,
    tool_registry: Arc<ToolRegistry>,
    audit_log: Arc<dyn AuditLog>,
    approval_service: Arc<dyn ApprovalService>,
}
```

**`invoke_tool` change:**

```rust
async fn invoke_tool(&self, agent_id: AgentId, tool: &str, input: Value) -> Result<Value> {
    // Check if this tool requires approval
    let manifest = self.registry.get_manifest(agent_id)?;
    if manifest.approval_required.contains(&tool.to_string()) {
        let result = self.approval_service.request(
            agent_id,
            manifest.name.clone(),
            format!("Agent '{}' wants to invoke tool '{}'", manifest.name, tool),
            Some(tool.to_string()),
            Some(input.clone()),
        ).await?;

        match result {
            ApprovalResult::Approved => {} // proceed
            ApprovalResult::Denied { reason } => {
                self.audit_log.record(AuditEvent::new(
                    agent_id,
                    AuditEventKind::HumanApprovalDenied { reason: reason.clone() },
                ));
                return Err(CoreError::CapabilityDenied {
                    agent_id,
                    capability: Capability::ToolInvoke { tool_name: tool.to_string() },
                    reason: format!("human denied: {reason}"),
                });
            }
            ApprovalResult::Timeout => {
                return Err(CoreError::CapabilityDenied {
                    agent_id,
                    capability: Capability::ToolInvoke { tool_name: tool.to_string() },
                    reason: "approval timed out".into(),
                });
            }
        }

        self.audit_log.record(AuditEvent::new(
            agent_id,
            AuditEventKind::HumanApprovalGranted,
        ));
    }

    let tokens = self.registry.get_tokens(agent_id)?;
    self.tool_invocation.invoke(agent_id, tool, input, &tokens).await
}
```

**`request_approval` on the trait delegates to `ApprovalService`:**

```rust
async fn request_approval(
    &self,
    agent_id: AgentId,
    description: String,
    _timeout: Duration,  // not used yet — blocks indefinitely for demo
) -> Result<ApprovalResult> {
    let manifest = self.registry.get_manifest(agent_id)?;
    self.approval_service.request(
        agent_id,
        manifest.name.clone(),
        description,
        None,
        None,
    ).await
}
```

### 5. Manifest Update

**In `aaos-core/src/manifest.rs`.**

Add `approval_required` field:

```rust
pub struct AgentManifest {
    // ... existing fields ...
    #[serde(default)]
    pub approval_required: Vec<String>,
}
```

Example manifest:
```yaml
name: research-agent
model: claude-haiku-4-5-20251001
system_prompt: "You are a research assistant."
capabilities:
  - web_search
  - "tool: web_fetch"
  - "tool: file_write"
  - "file_write: /data/output/*"
approval_required:
  - file_write
```

### 6. Daemon API Methods

**In `agentd/src/server.rs`.**

Two new JSON-RPC methods:

```
Method: "approval.list"
Params: {}
Returns: {
    "pending": [{
        "id": "<uuid>",
        "agent_id": "<uuid>",
        "agent_name": "research-agent",
        "description": "Agent 'research-agent' wants to invoke tool 'file_write'",
        "tool": "file_write",
        "input": { "path": "/data/output/report.md", "content": "..." },
        "timestamp": "2026-03-21T..."
    }]
}
```

```
Method: "approval.respond"
Params: { "id": "<uuid>", "decision": "approve" | "deny", "reason": "optional reason" }
Returns: { "ok": true }
Error: no pending approval with that ID
```

`Server` holds `Arc<ApprovalQueue>` and passes it to `InProcessAgentServices` at construction time.

### 7. Constructor Updates

**`AgentRegistry::new`** — add `router: Arc<MessageRouter>` parameter. Store it. Call `router.register()` in spawn (store receivers in `AgentProcess`), `router.unregister()` in stop.

**Call sites that break:** `agentd/server.rs:Server::new()`, `agentd/spawn_tool.rs:tests::setup()`, `aaos-runtime/services.rs:tests::setup()`, `aaos-runtime/registry.rs:tests::test_registry()`. Each needs a router instance.

**`InProcessAgentServices::new`** — add `approval_service: Arc<dyn ApprovalService>` parameter.

**Call sites that break:** `agentd/server.rs:execute_agent()`, `agentd/spawn_tool.rs:SpawnAgentTool::invoke()`, `aaos-runtime/services.rs:tests::setup()`. Each needs an approval service.

**`SpawnAgentTool`** — add `approval_service: Arc<dyn ApprovalService>` field. Passes it when constructing the child's `InProcessAgentServices`. This ensures child agents respect their own `approval_required` manifests.

**`Server::new`** — construct `ApprovalQueue`, pass to services and SpawnAgentTool. Pass router to registry.

**`Server::with_llm_client`** — same, plus pass approval_service to SpawnAgentTool.

**Test helpers:** use `NoOpApprovalService` for all test setups that don't test approval flow specifically.

## Build Order

1. Add `approval_required` to `AgentManifest`
2. Add `ApprovalService` trait + `NoOpApprovalService` to `aaos-core`. Update `AgentServices::send_message` signature to `send_message(&self, agent_id: AgentId, message: Value)`. Update all implementors.
3. Wire `MessageRouter` into `AgentRegistry` (register on spawn, store receivers in AgentProcess, unregister on stop). Update all 4 constructor call sites.
4. Wire `send_message` in `InProcessAgentServices` (parse recipient/method/params from Value, construct McpMessage, route)
5. Add `approval_service` to `InProcessAgentServices`, implement approval check in `invoke_tool`. Update all 3 constructor call sites. Thread approval_service into `SpawnAgentTool`.
6. Implement `ApprovalQueue` in agentd
7. Add `approval.list` and `approval.respond` API methods
8. Update `Server` constructors to wire everything together
9. End-to-end test: agent calls file_write → approval blocks → external client approves → file written

## What We Are NOT Building

- No request-response messaging between agents (Phase B — requires persistent agents)
- No approval timeout (demo blocks indefinitely — human is right there)
- No dynamic approval rules (manifest-based only)
- No approval modification ("modify-and-approve" — approve or deny only)
- No approval notification push (client must poll `approval.list`)

## Test Strategy

- **Messaging:** test that agents registered at spawn get messages routed, unregistered at stop
- **`ApprovalQueue`:** test request → list → respond → unblock flow, test sender-dropped graceful denial
- **`InProcessAgentServices` approval check:** test tool in `approval_required` blocks until approved, test tool NOT in list proceeds without approval, test denial returns error to agent
- **API methods:** test `approval.list` returns pending, test `approval.respond` unblocks
- **Integration:** `agent.spawn_and_run` with `approval_required: [file_write]`, file_write triggers approval, external `approval.respond` unblocks, file is written
