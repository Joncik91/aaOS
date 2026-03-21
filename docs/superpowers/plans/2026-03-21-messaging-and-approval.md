# Agent Messaging & Human Approval Queue Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire fire-and-forget messaging through the existing router and add a human approval queue that blocks agent execution until a human approves or denies via the daemon API.

**Architecture:** `AgentRegistry` registers agents with `MessageRouter` at spawn. `ApprovalService` trait in aaos-core, `ApprovalQueue` implementation in agentd. `InProcessAgentServices` checks `approval_required` manifest field before tool invocation and delegates to the approval service. Two new API methods: `approval.list` and `approval.respond`.

**Tech Stack:** Rust, tokio (oneshot channels for approval blocking), dashmap, serde_json

**Spec:** `docs/superpowers/specs/2026-03-21-messaging-and-approval-design.md`

---

## File Structure

### New Files
- `crates/agentd/src/approval.rs` — `ApprovalQueue`, `ApprovalInfo`, `ApprovalService` impl

### Modified Files
- `crates/aaos-core/src/manifest.rs` — add `approval_required: Vec<String>` field
- `crates/aaos-core/src/services.rs` — add `ApprovalService` trait, `NoOpApprovalService`, update `send_message` signature
- `crates/aaos-core/src/lib.rs` — re-export new types
- `crates/aaos-runtime/src/process.rs` — add message receiver fields to `AgentProcess`
- `crates/aaos-runtime/src/registry.rs` — add router to constructor, register/unregister on spawn/stop
- `crates/aaos-runtime/src/services.rs` — add approval_service + router, wire send_message and approval check
- `crates/aaos-runtime/Cargo.toml` — add `aaos-ipc` dependency
- `crates/agentd/src/server.rs` — add approval queue, new API methods, update constructors
- `crates/agentd/src/spawn_tool.rs` — add approval_service field
- `crates/agentd/src/main.rs` — add `mod approval;`
- `crates/agentd/Cargo.toml` — add `dashmap`, `chrono`, `uuid` if not present

---

### Task 1: Add `approval_required` to manifest and `ApprovalService` trait to core

**Files:**
- Modify: `crates/aaos-core/src/manifest.rs`
- Modify: `crates/aaos-core/src/services.rs`
- Modify: `crates/aaos-core/src/lib.rs`

- [ ] **Step 1: Add `approval_required` field to `AgentManifest`**

In `crates/aaos-core/src/manifest.rs`, add to the `AgentManifest` struct:

```rust
#[serde(default)]
pub approval_required: Vec<String>,
```

- [ ] **Step 2: Add test for manifest with approval_required**

Add to the tests module in `crates/aaos-core/src/manifest.rs`:

```rust
#[test]
fn parse_manifest_with_approval_required() {
    let yaml = r#"
name: test-agent
model: claude-haiku-4-5-20251001
system_prompt: "test"
approval_required:
  - file_write
  - spawn_agent
"#;
    let manifest = AgentManifest::from_yaml(yaml).unwrap();
    assert_eq!(manifest.approval_required, vec!["file_write", "spawn_agent"]);
}

#[test]
fn parse_manifest_without_approval_required() {
    let yaml = r#"
name: test-agent
model: claude-haiku-4-5-20251001
system_prompt: "test"
"#;
    let manifest = AgentManifest::from_yaml(yaml).unwrap();
    assert!(manifest.approval_required.is_empty());
}
```

- [ ] **Step 3: Add `ApprovalService` trait and `NoOpApprovalService` to `services.rs`**

In `crates/aaos-core/src/services.rs`, add:

```rust
/// Service for requesting human approval before sensitive actions.
/// Trait in core, implementations in agentd (ApprovalQueue) or test helpers (NoOpApprovalService).
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

/// Default approval service that auto-approves everything.
/// Used in tests and when no approval queue is configured.
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

- [ ] **Step 4: Update `AgentServices::send_message` signature**

In `crates/aaos-core/src/services.rs`, change:
```rust
async fn send_message(&self, message: Value) -> Result<Value>;
```
to:
```rust
async fn send_message(&self, agent_id: AgentId, message: Value) -> Result<Value>;
```

- [ ] **Step 5: Update re-exports in lib.rs**

Add to `crates/aaos-core/src/lib.rs`:
```rust
pub use services::{ApprovalService, NoOpApprovalService};
```

(Add `NoOpApprovalService` to the existing `services` re-export line.)

- [ ] **Step 6: Fix compilation errors from send_message signature change**

The `send_message` signature change breaks:
- `crates/aaos-runtime/src/services.rs` — `InProcessAgentServices::send_message`
- `crates/aaos-llm/src/executor.rs` — `MockAgentServices::send_message` in tests

Update both to add the `agent_id` parameter (unused for now):

In `crates/aaos-runtime/src/services.rs`:
```rust
async fn send_message(&self, _agent_id: AgentId, _message: Value) -> Result<Value> {
    Ok(serde_json::json!({"status": "delivered"}))
}
```

In `crates/aaos-llm/src/executor.rs` test mock:
```rust
async fn send_message(&self, _agent_id: AgentId, _message: Value) -> aaos_core::Result<Value> {
    Ok(serde_json::json!({"status": "delivered"}))
}
```

- [ ] **Step 7: Run tests**

Run: `cargo test --workspace`
Expected: All tests pass.

- [ ] **Step 8: Commit**

```bash
git add crates/aaos-core/ crates/aaos-runtime/src/services.rs crates/aaos-llm/src/executor.rs
git commit -m "feat(core): add ApprovalService trait, approval_required manifest field, update send_message signature"
```

---

### Task 2: Wire `MessageRouter` into `AgentRegistry`

**Files:**
- Modify: `crates/aaos-runtime/src/process.rs`
- Modify: `crates/aaos-runtime/src/registry.rs`
- Modify: `crates/aaos-runtime/Cargo.toml`

- [ ] **Step 1: Add `aaos-ipc` dependency to aaos-runtime**

Add to `crates/aaos-runtime/Cargo.toml` under `[dependencies]`:
```toml
aaos-ipc = { workspace = true }
```

- [ ] **Step 2: Add message receiver fields to `AgentProcess`**

In `crates/aaos-runtime/src/process.rs`, add imports and fields:

```rust
use aaos_ipc::{McpMessage, McpResponse};
use tokio::sync::mpsc;
```

Note: `tokio::sync::mpsc` is already imported for the command channel. Add the new fields to the `AgentProcess` struct:

```rust
pub message_rx: Option<tokio::sync::mpsc::Receiver<McpMessage>>,
pub response_rx: Option<tokio::sync::mpsc::Receiver<McpResponse>>,
```

Initialize both to `None` in `AgentProcess::new()`.

- [ ] **Step 3: Add router to `AgentRegistry` and register on spawn/stop**

In `crates/aaos-runtime/src/registry.rs`:

Add import:
```rust
use aaos_ipc::MessageRouter;
```

Add `router` field to `AgentRegistry`:
```rust
pub struct AgentRegistry {
    agents: DashMap<AgentId, AgentProcess>,
    audit_log: Arc<dyn AuditLog>,
    router: Arc<MessageRouter>,
}
```

Update constructor:
```rust
pub fn new(audit_log: Arc<dyn AuditLog>, router: Arc<MessageRouter>) -> Self {
    Self {
        agents: DashMap::new(),
        audit_log,
        router,
    }
}
```

In `spawn()`, after inserting the agent, register with router and store receivers:
```rust
// Register with message router
let (msg_rx, resp_rx) = self.router.register(id);
// Store receivers in the process
if let Some(mut entry) = self.agents.get_mut(&id) {
    entry.value_mut().message_rx = Some(msg_rx);
    entry.value_mut().response_rx = Some(resp_rx);
}
```

In `spawn_with_tokens()`, same pattern after insert.

In `stop()`, before removing from the map:
```rust
self.router.unregister(&id);
```

- [ ] **Step 4: Fix all test call sites**

Four test sites need updating to pass a router:

**`crates/aaos-runtime/src/registry.rs` — `test_registry()`:**
```rust
fn test_registry() -> (AgentRegistry, Arc<InMemoryAuditLog>) {
    let log = Arc::new(InMemoryAuditLog::new());
    let router = Arc::new(aaos_ipc::MessageRouter::new(log.clone(), |_, _| true));
    let registry = AgentRegistry::new(log.clone(), router);
    (registry, log)
}
```

**`crates/aaos-runtime/src/services.rs` — `setup()`:**
```rust
let router = Arc::new(aaos_ipc::MessageRouter::new(audit_log.clone(), |_, _| true));
let registry = Arc::new(AgentRegistry::new(audit_log.clone(), router));
```

**`crates/agentd/src/server.rs` — `Server::new()`:**
Already constructs a router — just pass it to `AgentRegistry::new`:
```rust
let registry = Arc::new(AgentRegistry::new(audit_log.clone(), router.clone()));
```
Note: the router is constructed AFTER registry currently. Reorder: construct router first, then registry with router.

**`crates/agentd/src/spawn_tool.rs` — test `setup()`:**
```rust
let router = Arc::new(aaos_ipc::MessageRouter::new(audit_log.clone(), |_, _| true));
let registry = Arc::new(AgentRegistry::new(audit_log.clone(), router));
```

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace`
Expected: All tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/aaos-runtime/ crates/agentd/src/server.rs crates/agentd/src/spawn_tool.rs
git commit -m "feat(runtime): wire MessageRouter into AgentRegistry lifecycle"
```

---

### Task 3: Wire `send_message` in `InProcessAgentServices`

**Files:**
- Modify: `crates/aaos-runtime/src/services.rs`

- [ ] **Step 1: Add router field and update constructor**

In `crates/aaos-runtime/src/services.rs`:

Add import:
```rust
use aaos_ipc::{McpMessage, MessageRouter};
```

Add `router` field to `InProcessAgentServices`:
```rust
pub struct InProcessAgentServices {
    registry: Arc<AgentRegistry>,
    tool_invocation: Arc<ToolInvocation>,
    tool_registry: Arc<ToolRegistry>,
    audit_log: Arc<dyn AuditLog>,
    router: Arc<MessageRouter>,
}
```

Update `new()`:
```rust
pub fn new(
    registry: Arc<AgentRegistry>,
    tool_invocation: Arc<ToolInvocation>,
    tool_registry: Arc<ToolRegistry>,
    audit_log: Arc<dyn AuditLog>,
    router: Arc<MessageRouter>,
) -> Self {
    Self { registry, tool_invocation, tool_registry, audit_log, router }
}
```

- [ ] **Step 2: Implement real `send_message`**

```rust
async fn send_message(&self, agent_id: AgentId, message: Value) -> Result<Value> {
    let recipient_str = message
        .get("recipient")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CoreError::Ipc("missing 'recipient' in message".into()))?;
    let recipient: AgentId = serde_json::from_value(serde_json::json!(recipient_str))
        .map_err(|e| CoreError::Ipc(format!("invalid recipient: {e}")))?;
    let method = message
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("notify")
        .to_string();
    let params = message.get("params").cloned().unwrap_or(serde_json::json!({}));

    let mcp_msg = McpMessage::new(agent_id, recipient, method, params);
    self.router.route(mcp_msg).await?;
    Ok(serde_json::json!({"status": "delivered"}))
}
```

- [ ] **Step 3: Fix test setup to pass router**

In the test `setup()` function, pass the router to `InProcessAgentServices::new`:
```rust
let services = InProcessAgentServices::new(
    registry,
    tool_invocation,
    tool_registry,
    audit_log.clone(),
    router,
);
```

- [ ] **Step 4: Fix call sites in agentd**

In `crates/agentd/src/server.rs` `execute_agent()`:
```rust
let services: Arc<dyn AgentServices> = Arc::new(InProcessAgentServices::new(
    self.registry.clone(),
    self.tool_invocation.clone(),
    self.tool_registry.clone(),
    self.audit_log.clone(),
    self.router.clone(),
));
```

In `crates/agentd/src/spawn_tool.rs` `invoke()`, same pattern — add `self.router.clone()` (needs router field added to SpawnAgentTool, or pass through. Simplest: add a `router` field to `SpawnAgentTool`):

Add `router: Arc<MessageRouter>` to `SpawnAgentTool` struct and `new()`. Update registration in `server.rs:with_llm_client`.

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace`
Expected: All tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/aaos-runtime/ crates/agentd/
git commit -m "feat(runtime): wire send_message through MessageRouter"
```

---

### Task 4: Add approval service to `InProcessAgentServices`

**Files:**
- Modify: `crates/aaos-runtime/src/services.rs`

- [ ] **Step 1: Add approval_service field**

Add to `InProcessAgentServices`:
```rust
approval_service: Arc<dyn ApprovalService>,
```

Update `new()` to accept it. Add import: `use aaos_core::{ApprovalService, NoOpApprovalService};`

- [ ] **Step 2: Implement approval check in `invoke_tool`**

Replace the existing `invoke_tool` with:

```rust
async fn invoke_tool(&self, agent_id: AgentId, tool: &str, input: Value) -> Result<Value> {
    // Check if this tool requires approval
    if let Ok(manifest) = self.registry.get_manifest(agent_id) {
        if manifest.approval_required.contains(&tool.to_string()) {
            let result = self.approval_service.request(
                agent_id,
                manifest.name.clone(),
                format!("Agent '{}' wants to invoke tool '{}'", manifest.name, tool),
                Some(tool.to_string()),
                Some(input.clone()),
            ).await?;

            match result {
                ApprovalResult::Approved => {
                    self.audit_log.record(AuditEvent::new(
                        agent_id,
                        AuditEventKind::HumanApprovalGranted,
                    ));
                }
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
        }
    }

    let tokens = self.registry.get_tokens(agent_id)?;
    self.tool_invocation.invoke(agent_id, tool, input, &tokens).await
}
```

- [ ] **Step 3: Update `request_approval` to delegate**

```rust
async fn request_approval(
    &self,
    agent_id: AgentId,
    description: String,
    _timeout: Duration,
) -> Result<ApprovalResult> {
    let name = self.registry.get_manifest(agent_id)
        .map(|m| m.name)
        .unwrap_or_else(|_| "unknown".into());
    self.approval_service.request(agent_id, name, description, None, None).await
}
```

- [ ] **Step 4: Fix test setup — use NoOpApprovalService**

In the test `setup()`:
```rust
let approval: Arc<dyn ApprovalService> = Arc::new(NoOpApprovalService);
let services = InProcessAgentServices::new(
    registry, tool_invocation, tool_registry, audit_log.clone(), router, approval,
);
```

- [ ] **Step 5: Add test for approval blocking**

```rust
#[tokio::test]
async fn invoke_tool_with_approval_required() {
    // Setup with NoOpApprovalService (auto-approves)
    let (services, agent_id, _log) = setup_with_approval();
    let result = services
        .invoke_tool(agent_id, "echo", serde_json::json!({"message": "hello"}))
        .await
        .unwrap();
    assert_eq!(result, serde_json::json!({"message": "hello"}));
}
```

Where `setup_with_approval()` creates an agent with `approval_required: ["echo"]` and uses `NoOpApprovalService`.

- [ ] **Step 6: Fix all call sites in agentd**

Update `execute_agent()` and `SpawnAgentTool::invoke()` to pass the approval service to `InProcessAgentServices::new`.

`SpawnAgentTool` needs an `approval_service: Arc<dyn ApprovalService>` field. Add it to the struct, `new()`, and the registration in `with_llm_client`.

- [ ] **Step 7: Run tests**

Run: `cargo test --workspace`
Expected: All tests pass.

- [ ] **Step 8: Commit**

```bash
git add crates/aaos-runtime/ crates/agentd/
git commit -m "feat(runtime): add approval check in InProcessAgentServices"
```

---

### Task 5: Implement `ApprovalQueue` in agentd

**Files:**
- Create: `crates/agentd/src/approval.rs`
- Modify: `crates/agentd/src/main.rs`

- [ ] **Step 1: Create `approval.rs`**

```rust
use std::sync::Arc;

use aaos_core::{AgentId, ApprovalResult, ApprovalService, CoreError, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::oneshot;
use uuid::Uuid;

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

impl ApprovalQueue {
    pub fn new() -> Self {
        Self {
            pending: DashMap::new(),
        }
    }

    pub fn list(&self) -> Vec<ApprovalInfo> {
        self.pending
            .iter()
            .map(|entry| {
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
            })
            .collect()
    }

    pub fn respond(&self, id: Uuid, decision: ApprovalResult) -> Result<()> {
        match self.pending.remove(&id) {
            Some((_, pending)) => {
                let _ = pending.response_tx.send(decision);
                Ok(())
            }
            None => Err(CoreError::Ipc(format!(
                "no pending approval with id {id}"
            ))),
        }
    }
}

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

        tracing::info!(
            approval_id = %id,
            agent = %agent_name,
            tool = ?tool,
            "approval requested — waiting for human response"
        );

        self.pending.insert(
            id,
            PendingApproval {
                id,
                agent_id,
                agent_name,
                description,
                tool,
                input,
                timestamp: Utc::now(),
                response_tx: tx,
            },
        );

        match rx.await {
            Ok(result) => Ok(result),
            Err(_) => {
                self.pending.remove(&id);
                Ok(ApprovalResult::Denied {
                    reason: "approval service unavailable".into(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn approval_flow() {
        let queue = Arc::new(ApprovalQueue::new());
        let queue_clone = queue.clone();

        let agent_id = AgentId::new();
        let handle = tokio::spawn(async move {
            queue_clone
                .request(agent_id, "test-agent".into(), "test".into(), Some("file_write".into()), None)
                .await
        });

        // Wait a moment for the request to be inserted
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Should be one pending
        let pending = queue.list();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].agent_name, "test-agent");
        assert_eq!(pending[0].tool, Some("file_write".into()));

        // Approve
        queue.respond(pending[0].id, ApprovalResult::Approved).unwrap();

        let result = handle.await.unwrap().unwrap();
        assert_eq!(result, ApprovalResult::Approved);

        // Pending should be empty
        assert!(queue.list().is_empty());
    }

    #[tokio::test]
    async fn approval_denied() {
        let queue = Arc::new(ApprovalQueue::new());
        let queue_clone = queue.clone();

        let handle = tokio::spawn(async move {
            queue_clone
                .request(AgentId::new(), "agent".into(), "test".into(), None, None)
                .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let pending = queue.list();
        queue
            .respond(pending[0].id, ApprovalResult::Denied { reason: "no".into() })
            .unwrap();

        let result = handle.await.unwrap().unwrap();
        assert!(matches!(result, ApprovalResult::Denied { .. }));
    }

    #[tokio::test]
    async fn respond_nonexistent() {
        let queue = ApprovalQueue::new();
        let result = queue.respond(Uuid::new_v4(), ApprovalResult::Approved);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn sender_dropped_returns_denied() {
        let queue = Arc::new(ApprovalQueue::new());
        let queue_clone = queue.clone();

        let handle = tokio::spawn(async move {
            queue_clone
                .request(AgentId::new(), "agent".into(), "test".into(), None, None)
                .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Drop the queue (drops all pending senders)
        drop(queue);

        let result = handle.await.unwrap().unwrap();
        assert!(matches!(result, ApprovalResult::Denied { .. }));
    }
}
```

- [ ] **Step 2: Add module to main.rs**

Add to `crates/agentd/src/main.rs`:
```rust
mod approval;
```

- [ ] **Step 3: Add dependencies to agentd Cargo.toml if needed**

Check that `dashmap`, `chrono`, `uuid` are available. They should be (dashmap via aaos-runtime, chrono/uuid via aaos-core). If not, add workspace deps.

- [ ] **Step 4: Run tests**

Run: `cargo test -p agentd`
Expected: All tests pass including 4 new approval tests.

- [ ] **Step 5: Commit**

```bash
git add crates/agentd/
git commit -m "feat(agentd): implement ApprovalQueue with oneshot blocking"
```

---

### Task 6: Add `approval.list` and `approval.respond` API methods + wire constructors

**Files:**
- Modify: `crates/agentd/src/server.rs`

- [ ] **Step 1: Add `ApprovalQueue` to `Server` struct**

Add field:
```rust
pub approval_queue: Arc<crate::approval::ApprovalQueue>,
```

- [ ] **Step 2: Update `Server::new()` constructor**

Reorder construction: build router first, then registry with router, then everything else. Add approval queue:

```rust
pub fn new() -> Self {
    let audit_log: Arc<dyn AuditLog> = Arc::new(InMemoryAuditLog::new());
    let approval_queue = Arc::new(crate::approval::ApprovalQueue::new());

    // Build router first (registry needs it)
    let registry_for_router = Arc::new(/* can't do this yet */);
    // Actually: build router with a temporary checker, then registry, then update.
    // Simpler: build router first with a no-op checker, build registry, then rebuild router.
    // Simplest: keep the current pattern but pass router to registry.
```

Actually, there's an ordering issue: the router's capability checker closure captures a clone of the registry, but the registry constructor now needs the router. This is a circular initialization.

**Solution:** Create the router with a placeholder checker, create the registry with that router, then update the capability checker. OR: use `Arc<RwLock<Option<...>>>` for the checker. OR: simplest for the prototype — create the router without a capability checker initially and add one later.

**Better solution:** The `MessageRouter::new` takes a closure. Create the router first with a dummy closure that always returns true, create the registry, then the actual capability checking happens via token validation in `InProcessAgentServices`, not in the router. For the prototype, the router's capability checker can always return true since the real enforcement happens at the `ToolInvocation`/`InProcessAgentServices` level.

Wait — looking at the current code in `server.rs`, the router IS created with a `registry_clone` reference:
```rust
let registry_clone = registry.clone();
let router = Arc::new(MessageRouter::new(
    audit_log.clone(),
    move |agent_id, cap| { registry_clone.check_capability(agent_id, cap).unwrap_or(false) },
));
```

The fix: create the `AgentRegistry` first WITHOUT the router (using a default/dummy), then create the router with the registry, then set the router on the registry. But that requires `AgentRegistry` to accept setting the router after construction.

**Simplest fix:** Make `AgentRegistry::new` take an `Option<Arc<MessageRouter>>` and provide a `set_router` method. Or: accept that for Phase A, the router in the registry is used only for register/unregister (which doesn't need the capability checker), and the router's checker is created after both exist.

**Actually the simplest fix:** Change construction order:
1. Create `audit_log`
2. Create `registry` with a dummy/no-op router
3. Create `router` with registry reference
4. Replace the registry's router (or just accept the dummy for register/unregister — those don't check capabilities)

Since `register()` and `unregister()` on the router don't use the capability checker (only `route()` does), we can:
1. Create a "registration-only" router first
2. Create registry with it
3. Create the "real" router with capability checking

But that means two routers. Messy.

**Best approach:** The registry doesn't need the router's capability checker — it only calls `register` and `unregister`. Create the router first with the capability checker as always-true, pass it to the registry, then after registry is created, store the real capability-checking router in the Server struct. The router passed to the registry is used for registration only.

Actually, looking more carefully: we can just create the router first, then the registry:

```rust
// Create audit log
let audit_log: Arc<dyn AuditLog> = Arc::new(InMemoryAuditLog::new());

// Create router (capability check will use the registry, but we create it with a lazy Arc)
let registry: Arc<AgentRegistry> = Arc::new_cyclic(|weak_registry| {
    // This doesn't work because AgentRegistry::new needs Arc<MessageRouter>
});
```

OK, this is getting complicated. Let me just use the simple approach: give `AgentRegistry` an `Option<Arc<MessageRouter>>` for the router field, defaulting to None. The spawn/stop methods only call register/unregister if the router is Some. Then set it after construction.

```rust
pub struct AgentRegistry {
    agents: DashMap<AgentId, AgentProcess>,
    audit_log: Arc<dyn AuditLog>,
    router: Option<Arc<MessageRouter>>,
}

impl AgentRegistry {
    pub fn new(audit_log: Arc<dyn AuditLog>) -> Self {
        Self { agents: DashMap::new(), audit_log, router: None }
    }

    pub fn set_router(&mut self, router: Arc<MessageRouter>) {
        self.router = Some(router);
    }
}
```

Then in `spawn()`, only register if router is Some. This keeps backward compatibility and avoids the circular init problem.

**Update the plan:** Change Task 2's approach to use `Option<Arc<MessageRouter>>` + `set_router()` instead of passing router in constructor. This avoids the circular initialization.

- [ ] **Step 3: Rebuild Server::new with correct ordering**

```rust
pub fn new() -> Self {
    let audit_log: Arc<dyn AuditLog> = Arc::new(InMemoryAuditLog::new());
    let approval_queue = Arc::new(crate::approval::ApprovalQueue::new());
    let registry = Arc::new(AgentRegistry::new(audit_log.clone()));
    let tool_registry = Arc::new(ToolRegistry::new());
    let validator = Arc::new(SchemaValidator::new());

    // Register built-in tools
    tool_registry.register(Arc::new(EchoTool));
    tool_registry.register(Arc::new(aaos_tools::WebFetchTool::new()));
    tool_registry.register(Arc::new(aaos_tools::FileReadTool));
    tool_registry.register(Arc::new(aaos_tools::FileWriteTool));

    let tool_invocation = Arc::new(ToolInvocation::new(
        tool_registry.clone(),
        audit_log.clone(),
    ));

    // Create message router with capability checking
    let registry_clone = registry.clone();
    let router = Arc::new(MessageRouter::new(
        audit_log.clone(),
        move |agent_id, cap| {
            registry_clone.check_capability(agent_id, cap).unwrap_or(false)
        },
    ));

    // Set router on registry for spawn/stop registration
    // Need to use Arc::get_mut or interior mutability
    // Simplest: use a method that takes &self with interior mutability
    registry.set_router(router.clone());

    Self {
        registry,
        tool_registry,
        tool_invocation,
        router,
        validator,
        audit_log,
        approval_queue,
        llm_client: None,
    }
}
```

Note: `set_router` needs to work through `&self` since registry is behind Arc. Use `RwLock<Option<Arc<MessageRouter>>>` or `once_cell::sync::OnceCell`.

**Use `OnceCell`:**
```rust
use std::sync::OnceLock;

pub struct AgentRegistry {
    agents: DashMap<AgentId, AgentProcess>,
    audit_log: Arc<dyn AuditLog>,
    router: OnceLock<Arc<MessageRouter>>,
}

impl AgentRegistry {
    pub fn new(audit_log: Arc<dyn AuditLog>) -> Self {
        Self { agents: DashMap::new(), audit_log, router: OnceLock::new() }
    }

    pub fn set_router(&self, router: Arc<MessageRouter>) {
        let _ = self.router.set(router);
    }
}
```

And in `spawn()`:
```rust
if let Some(router) = self.router.get() {
    let (msg_rx, resp_rx) = router.register(id);
    // store receivers...
}
```

- [ ] **Step 4: Update `with_llm_client` and `execute_agent`**

`with_llm_client` registers SpawnAgentTool with the approval queue:
```rust
pub fn with_llm_client(llm_client: Arc<dyn LlmClient>) -> Self {
    let mut server = Self::new();
    let spawn_tool = Arc::new(crate::spawn_tool::SpawnAgentTool::new(
        llm_client.clone(),
        server.registry.clone(),
        server.tool_registry.clone(),
        server.tool_invocation.clone(),
        server.audit_log.clone(),
        server.router.clone(),
        server.approval_queue.clone() as Arc<dyn ApprovalService>,
    ));
    server.tool_registry.register(spawn_tool);
    server.llm_client = Some(llm_client);
    server
}
```

`execute_agent` passes router and approval to `InProcessAgentServices`:
```rust
let services: Arc<dyn AgentServices> = Arc::new(InProcessAgentServices::new(
    self.registry.clone(),
    self.tool_invocation.clone(),
    self.tool_registry.clone(),
    self.audit_log.clone(),
    self.router.clone(),
    self.approval_queue.clone() as Arc<dyn ApprovalService>,
));
```

- [ ] **Step 5: Add API handlers**

Add to `handle_request` match:
```rust
"approval.list" => self.handle_approval_list(request.id.clone()),
"approval.respond" => self.handle_approval_respond(&request.params, request.id.clone()),
```

Implement handlers:
```rust
fn handle_approval_list(&self, id: serde_json::Value) -> JsonRpcResponse {
    let pending = self.approval_queue.list();
    JsonRpcResponse::success(id, json!({"pending": pending}))
}

fn handle_approval_respond(
    &self,
    params: &serde_json::Value,
    id: serde_json::Value,
) -> JsonRpcResponse {
    let approval_id = match params.get("id").and_then(|v| v.as_str()) {
        Some(s) => match uuid::Uuid::parse_str(s) {
            Ok(id) => id,
            Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, format!("invalid id: {e}")),
        },
        None => return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'id' parameter"),
    };

    let decision = match params.get("decision").and_then(|v| v.as_str()) {
        Some("approve") => ApprovalResult::Approved,
        Some("deny") => {
            let reason = params
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("denied by human")
                .to_string();
            ApprovalResult::Denied { reason }
        }
        Some(other) => {
            return JsonRpcResponse::error(
                id,
                INTERNAL_ERROR,
                format!("invalid decision: {other}. Use 'approve' or 'deny'"),
            )
        }
        None => return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'decision' parameter"),
    };

    match self.approval_queue.respond(approval_id, decision) {
        Ok(()) => JsonRpcResponse::success(id, json!({"ok": true})),
        Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}
```

- [ ] **Step 6: Run tests**

Run: `cargo test --workspace`
Expected: All tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/agentd/ crates/aaos-runtime/
git commit -m "feat(agentd): add approval.list and approval.respond API methods"
```

---

### Task 7: Final verification and end-to-end test

**Files:** None (read-only + manual testing)

- [ ] **Step 1: Run full test suite**

Run: `cargo test --workspace`
Expected: All tests pass.

- [ ] **Step 2: Clippy and fmt**

Run: `cargo clippy --workspace -- -D warnings`
Run: `cargo fmt --check` (fix with `cargo fmt` if needed)

- [ ] **Step 3: End-to-end approval test**

Start daemon:
```bash
ANTHROPIC_API_KEY="sk-..." cargo run -p agentd -- run --socket /tmp/agentd-test.sock &
sleep 3
```

Spawn and run agent with approval_required:
```bash
# This will BLOCK waiting for approval
echo '{"jsonrpc":"2.0","id":1,"method":"agent.spawn_and_run","params":{"manifest":"name: writer\nmodel: claude-haiku-4-5-20251001\nsystem_prompt: \"You are helpful. Be concise.\"\ncapabilities:\n  - \"tool: file_write\"\n  - \"file_write: /tmp/aaos-test/*\"\napproval_required:\n  - file_write\n","message":"Write hello to /tmp/aaos-test/approved.txt using file_write."}}' | socat -t120 - UNIX-CONNECT:/tmp/agentd-test.sock &
```

In another terminal, check for pending approvals:
```bash
echo '{"jsonrpc":"2.0","id":2,"method":"approval.list","params":{}}' | socat -t5 - UNIX-CONNECT:/tmp/agentd-test.sock
```

Approve it (using the ID from the list response):
```bash
echo '{"jsonrpc":"2.0","id":3,"method":"approval.respond","params":{"id":"<UUID>","decision":"approve"}}' | socat -t5 - UNIX-CONNECT:/tmp/agentd-test.sock
```

Verify the file was written:
```bash
cat /tmp/aaos-test/approved.txt
```

- [ ] **Step 4: Cleanup**

```bash
kill %1 %2 2>/dev/null
rm -f /tmp/agentd-test.sock
rm -rf /tmp/aaos-test
cargo fmt
git add -A
git commit -m "feat: complete messaging and approval — demo ready"
git push
```
