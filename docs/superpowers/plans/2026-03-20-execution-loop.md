# Execution Loop Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make agents execute — call LLMs, use tools, return results — through a uniform service interface.

**Architecture:** `AgentServices` trait in aaos-core provides the uniform interface. `InProcessAgentServices` in aaos-runtime implements it using existing registry/tools. New `aaos-llm` crate contains the LLM client and execution loop. agentd wires everything together and exposes `tool.invoke`, `agent.run`, and `agent.spawn_and_run` API methods.

**Tech Stack:** Rust, tokio, reqwest (HTTP client for Anthropic API), async-trait, serde_json

**Spec:** `docs/superpowers/specs/2026-03-20-execution-loop-design.md`

---

## File Structure

### New Files
- `crates/aaos-core/src/services.rs` — `AgentServices` trait, `TokenUsage`, `ApprovalResult`
- `crates/aaos-core/src/tool_definition.rs` — `ToolDefinition` (moved from aaos-tools)
- `crates/aaos-runtime/src/services.rs` — `InProcessAgentServices`
- `crates/aaos-llm/Cargo.toml` — new crate manifest
- `crates/aaos-llm/src/lib.rs` — crate root
- `crates/aaos-llm/src/error.rs` — `LlmError`, `LlmResult`
- `crates/aaos-llm/src/types.rs` — `CompletionRequest`, `CompletionResponse`, `Message`, `ContentBlock`, `LlmStopReason`
- `crates/aaos-llm/src/client.rs` — `LlmClient` trait
- `crates/aaos-llm/src/anthropic.rs` — `AnthropicClient`, `AnthropicConfig`
- `crates/aaos-llm/src/executor.rs` — `AgentExecutor`, `ExecutorConfig`, `ExecutionResult`, `ExecutionStopReason`

### Modified Files
- `crates/aaos-core/src/lib.rs` — add modules + re-exports
- `crates/aaos-core/src/audit.rs` — add `UsageReported`, `AgentExecutionStarted`, `AgentExecutionCompleted` variants
- `crates/aaos-core/Cargo.toml` — add `async-trait` dependency
- `crates/aaos-tools/src/tool.rs` — remove `ToolDefinition`, import from aaos-core
- `crates/aaos-tools/src/lib.rs` — re-export `ToolDefinition` from aaos-core
- `crates/aaos-runtime/src/lib.rs` — add `services` module + re-export
- `crates/aaos-runtime/src/registry.rs` — add `get_tokens()` and `get_manifest()` methods
- `crates/aaos-runtime/Cargo.toml` — add `aaos-tools`, `async-trait` dependencies
- `crates/aaos-tools/src/registry.rs` — update `ToolDefinition` import
- `crates/agentd/src/server.rs` — add `tool.invoke`, `agent.run`, `agent.spawn_and_run` handlers
- `crates/agentd/Cargo.toml` — add `aaos-llm`, `async-trait` dependencies
- `Cargo.toml` — add `aaos-llm` to workspace members, add `reqwest` to workspace deps

---

### Task 1: Move `ToolDefinition` to `aaos-core`

**Files:**
- Create: `crates/aaos-core/src/tool_definition.rs`
- Modify: `crates/aaos-core/src/lib.rs`
- Modify: `crates/aaos-tools/src/tool.rs`
- Modify: `crates/aaos-tools/src/lib.rs`
- Modify: `crates/aaos-tools/src/registry.rs`

- [ ] **Step 1: Create `tool_definition.rs` in aaos-core**

```rust
// crates/aaos-core/src/tool_definition.rs
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Definition of a tool's interface — its name, description, and input schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}
```

- [ ] **Step 2: Add module and re-export in aaos-core lib.rs**

Add to `crates/aaos-core/src/lib.rs`:
```rust
pub mod tool_definition;
pub use tool_definition::ToolDefinition;
```

- [ ] **Step 3: Update aaos-tools to use aaos-core's ToolDefinition**

In `crates/aaos-tools/src/tool.rs`, remove the `ToolDefinition` struct definition and replace with an import:
```rust
use aaos_core::ToolDefinition;
```

Keep the `Tool` trait and `EchoTool` — they reference `ToolDefinition` but don't own it.

- [ ] **Step 3b: Update aaos-tools registry.rs import**

In `crates/aaos-tools/src/registry.rs`, change:
```rust
use crate::tool::{Tool, ToolDefinition};
```
to:
```rust
use aaos_core::ToolDefinition;
use crate::tool::Tool;
```

- [ ] **Step 4: Update aaos-tools lib.rs re-export**

In `crates/aaos-tools/src/lib.rs`, change:
```rust
pub use tool::{EchoTool, Tool, ToolDefinition};
```
to:
```rust
pub use aaos_core::ToolDefinition;
pub use tool::{EchoTool, Tool};
```

- [ ] **Step 5: Run tests to verify nothing broke**

Run: `cargo test --workspace`
Expected: All 52 tests pass. No compilation errors.

- [ ] **Step 6: Commit**

```bash
git add -A crates/aaos-core/src/tool_definition.rs crates/aaos-core/src/lib.rs crates/aaos-tools/src/tool.rs crates/aaos-tools/src/lib.rs
git commit -m "refactor: move ToolDefinition from aaos-tools to aaos-core"
```

---

### Task 2: Add new `AuditEventKind` variants and `TokenUsage` to `aaos-core`

**Files:**
- Modify: `crates/aaos-core/src/audit.rs`
- Create: `crates/aaos-core/src/services.rs`
- Modify: `crates/aaos-core/src/lib.rs`
- Modify: `crates/aaos-core/Cargo.toml`

- [ ] **Step 1: Write test for new audit event variants**

Add to the `tests` module in `crates/aaos-core/src/audit.rs`:

```rust
#[test]
fn usage_reported_event_roundtrips_json() {
    let event = AuditEvent::new(
        AgentId::new(),
        AuditEventKind::UsageReported {
            input_tokens: 1500,
            output_tokens: 300,
        },
    );
    let json = serde_json::to_string(&event).unwrap();
    let parsed: AuditEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(event.id, parsed.id);
}

#[test]
fn execution_started_event_roundtrips_json() {
    let event = AuditEvent::new(
        AgentId::new(),
        AuditEventKind::AgentExecutionStarted {
            message_preview: "Analyze this data...".into(),
        },
    );
    let json = serde_json::to_string(&event).unwrap();
    let parsed: AuditEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(event.id, parsed.id);
}

#[test]
fn execution_completed_event_roundtrips_json() {
    let event = AuditEvent::new(
        AgentId::new(),
        AuditEventKind::AgentExecutionCompleted {
            stop_reason: "complete".into(),
            total_iterations: 3,
        },
    );
    let json = serde_json::to_string(&event).unwrap();
    let parsed: AuditEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(event.id, parsed.id);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p aaos-core`
Expected: FAIL — `UsageReported`, `AgentExecutionStarted`, `AgentExecutionCompleted` variants don't exist.

- [ ] **Step 3: Add the three new variants to `AuditEventKind`**

In `crates/aaos-core/src/audit.rs`, add to the `AuditEventKind` enum after the existing variants:

```rust
UsageReported {
    input_tokens: u64,
    output_tokens: u64,
},
AgentExecutionStarted {
    message_preview: String,
},
AgentExecutionCompleted {
    stop_reason: String,
    total_iterations: u32,
},
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p aaos-core`
Expected: All tests pass including the three new ones.

- [ ] **Step 5: Create `services.rs` with `TokenUsage`, `ApprovalResult`, and `AgentServices` trait**

Add `async-trait` to `crates/aaos-core/Cargo.toml`:
```toml
async-trait = { workspace = true }
```

Create `crates/aaos-core/src/services.rs`:

```rust
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent_id::AgentId;
use crate::error::Result;
use crate::tool_definition::ToolDefinition;

/// Token usage from a single LLM call or accumulated across a run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// Result of a human approval request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalResult {
    Approved,
    Denied { reason: String },
    Timeout,
}

/// Uniform interface for kernel services provided to agents.
///
/// Both internal agents (running inside agentd) and future external agents
/// (connecting via Unix socket) use this same interface. The in-process
/// implementation goes through the same capability checks and audit logging
/// that the socket implementation will.
#[async_trait]
pub trait AgentServices: Send + Sync {
    /// Invoke a tool on behalf of an agent, with full capability enforcement and audit logging.
    ///
    /// Tokens are looked up by agent_id from the registry, not passed per-call.
    /// This ensures checks are always against current state (revoked tokens fail immediately).
    ///
    /// NOTE: A future `invoke_tool_with_scope` variant may be needed for delegated
    /// invocations, where agent A invokes a tool on behalf of agent B with a restricted
    /// subset of capabilities. Not needed until orchestration layer (Phase 04).
    async fn invoke_tool(&self, agent_id: AgentId, tool: &str, input: Value) -> Result<Value>;

    /// Send a structured message to another agent.
    /// The message Value must be a valid MCP message envelope (JSON-RPC 2.0 with metadata).
    /// The implementation deserializes and routes it via the MessageRouter.
    ///
    /// Agent-to-agent messaging is deferred for Phase A. This exists on the trait to
    /// establish the interface for Phase B external agents.
    async fn send_message(&self, message: Value) -> Result<Value>;

    /// Request human approval. Blocks until approved, denied, or timeout.
    /// Semantically distinct from send_message — approval has blocking semantics
    /// with explicit timeout behavior.
    async fn request_approval(
        &self,
        agent_id: AgentId,
        description: String,
        timeout: Duration,
    ) -> Result<ApprovalResult>;

    /// Report token usage for cost tracking and budget enforcement.
    async fn report_usage(&self, agent_id: AgentId, usage: TokenUsage) -> Result<()>;

    /// List tools available to this agent (filtered by capabilities).
    /// Returns only tools the agent has capability tokens for.
    ///
    /// This is the PRIMARY mechanism for scoping tool access — the LLM never sees tools
    /// the agent can't use. Filtering at the schema level improves LLM tool selection
    /// performance. Capability enforcement at invocation time is the safety net.
    async fn list_tools(&self, agent_id: AgentId) -> Result<Vec<ToolDefinition>>;
}
```

- [ ] **Step 6: Add module and re-exports to lib.rs**

Add to `crates/aaos-core/src/lib.rs`:
```rust
pub mod services;
pub use services::{AgentServices, ApprovalResult, TokenUsage};
```

- [ ] **Step 7: Run full workspace tests**

Run: `cargo test --workspace`
Expected: All tests pass. No compilation errors.

- [ ] **Step 8: Commit**

```bash
git add crates/aaos-core/
git commit -m "feat(core): add AgentServices trait, TokenUsage, and new audit event variants"
```

---

### Task 3: Add `get_tokens()` and `get_manifest()` to `AgentRegistry`

**Files:**
- Modify: `crates/aaos-runtime/src/registry.rs`

- [ ] **Step 1: Write tests for `get_tokens` and `get_manifest`**

Add to the `tests` module in `crates/aaos-runtime/src/registry.rs`:

```rust
#[test]
fn get_tokens_returns_agent_capabilities() {
    let (registry, _log) = test_registry();
    let id = registry.spawn(test_manifest("agent-1")).unwrap();

    let tokens = registry.get_tokens(id).unwrap();
    // test_manifest declares web_search and file_read
    assert_eq!(tokens.len(), 2);
}

#[test]
fn get_tokens_nonexistent_agent() {
    let (registry, _log) = test_registry();
    let result = registry.get_tokens(AgentId::new());
    assert!(result.is_err());
}

#[test]
fn get_manifest_returns_agent_manifest() {
    let (registry, _log) = test_registry();
    let id = registry.spawn(test_manifest("agent-1")).unwrap();

    let manifest = registry.get_manifest(id).unwrap();
    assert_eq!(manifest.name, "agent-1");
}

#[test]
fn get_manifest_nonexistent_agent() {
    let (registry, _log) = test_registry();
    let result = registry.get_manifest(AgentId::new());
    assert!(result.is_err());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p aaos-runtime -- get_tokens`
Expected: FAIL — `get_tokens` method doesn't exist.

- [ ] **Step 3: Implement `get_tokens` and `get_manifest`**

Add to `AgentRegistry` impl block in `crates/aaos-runtime/src/registry.rs`:

```rust
/// Get a clone of the agent's capability tokens.
/// Acquires a DashMap read lock and clones the token vector.
pub fn get_tokens(&self, id: AgentId) -> Result<Vec<CapabilityToken>> {
    self.agents
        .get(&id)
        .map(|entry| entry.value().capabilities.clone())
        .ok_or(CoreError::AgentNotFound(id))
}

/// Get a clone of the agent's manifest.
pub fn get_manifest(&self, id: AgentId) -> Result<AgentManifest> {
    self.agents
        .get(&id)
        .map(|entry| entry.value().manifest.clone())
        .ok_or(CoreError::AgentNotFound(id))
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p aaos-runtime`
Expected: All tests pass including the four new ones.

- [ ] **Step 5: Commit**

```bash
git add crates/aaos-runtime/src/registry.rs
git commit -m "feat(runtime): add get_tokens() and get_manifest() to AgentRegistry"
```

---

### Task 4: Implement `InProcessAgentServices`

**Files:**
- Create: `crates/aaos-runtime/src/services.rs`
- Modify: `crates/aaos-runtime/src/lib.rs`
- Modify: `crates/aaos-runtime/Cargo.toml`

- [ ] **Step 1: Add dependencies to aaos-runtime Cargo.toml**

Add to `crates/aaos-runtime/Cargo.toml` `[dependencies]`:
```toml
aaos-tools = { workspace = true }
async-trait = { workspace = true }
```

Note: `aaos-ipc` is NOT needed for Phase A. The `send_message` method returns a stub response. Add `aaos-ipc` when wiring up the MessageRouter in Phase B.

- [ ] **Step 2: Write tests for `InProcessAgentServices`**

Create `crates/aaos-runtime/src/services.rs` starting with tests:

```rust
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use aaos_core::{
    AgentId, AgentManifest, AgentServices, ApprovalResult, AuditEvent, AuditEventKind,
    AuditLog, Capability, CoreError, InMemoryAuditLog, Result, TokenUsage, ToolDefinition,
};
use aaos_tools::{ToolInvocation, ToolRegistry};

use crate::registry::AgentRegistry;

/// In-process implementation of AgentServices.
///
/// Delegates to existing registry, tool_invocation, and router subsystems.
/// Same capability checks and audit logging as the future socket implementation.
pub struct InProcessAgentServices {
    registry: Arc<AgentRegistry>,
    tool_invocation: Arc<ToolInvocation>,
    tool_registry: Arc<ToolRegistry>,
    audit_log: Arc<dyn AuditLog>,
}

impl InProcessAgentServices {
    pub fn new(
        registry: Arc<AgentRegistry>,
        tool_invocation: Arc<ToolInvocation>,
        tool_registry: Arc<ToolRegistry>,
        audit_log: Arc<dyn AuditLog>,
    ) -> Self {
        Self {
            registry,
            tool_invocation,
            tool_registry,
            audit_log,
        }
    }
}

#[async_trait]
impl AgentServices for InProcessAgentServices {
    async fn invoke_tool(&self, agent_id: AgentId, tool: &str, input: Value) -> Result<Value> {
        let tokens = self.registry.get_tokens(agent_id)?;
        self.tool_invocation.invoke(agent_id, tool, input, &tokens).await
    }

    async fn send_message(&self, _message: Value) -> Result<Value> {
        // Phase A: messaging deferred. Return success with empty response.
        Ok(serde_json::json!({"status": "delivered"}))
    }

    async fn request_approval(
        &self,
        _agent_id: AgentId,
        _description: String,
        _timeout: Duration,
    ) -> Result<ApprovalResult> {
        // Phase A: no human supervision. Auto-approve everything.
        Ok(ApprovalResult::Approved)
    }

    async fn report_usage(&self, agent_id: AgentId, usage: TokenUsage) -> Result<()> {
        self.audit_log.record(AuditEvent::new(
            agent_id,
            AuditEventKind::UsageReported {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
            },
        ));
        Ok(())
    }

    async fn list_tools(&self, agent_id: AgentId) -> Result<Vec<ToolDefinition>> {
        let tokens = self.registry.get_tokens(agent_id)?;
        let all_tools = self.tool_registry.list();

        let filtered = all_tools
            .into_iter()
            .filter(|tool_def| {
                let required = Capability::ToolInvoke {
                    tool_name: tool_def.name.clone(),
                };
                tokens.iter().any(|t| t.permits(&required))
            })
            .collect();

        Ok(filtered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_tools::EchoTool;

    fn setup() -> (InProcessAgentServices, AgentId, Arc<InMemoryAuditLog>) {
        let audit_log = Arc::new(InMemoryAuditLog::new());
        let registry = Arc::new(AgentRegistry::new(audit_log.clone()));
        let tool_registry = Arc::new(ToolRegistry::new());
        tool_registry.register(Arc::new(EchoTool));

        let tool_invocation = Arc::new(ToolInvocation::new(
            tool_registry.clone(),
            audit_log.clone(),
        ));

        // Spawn an agent with tool:echo capability
        let manifest = AgentManifest::from_yaml(r#"
name: test-agent
model: claude-sonnet-4-20250514
system_prompt: "test"
capabilities:
  - "tool: echo"
  - web_search
"#).unwrap();
        let agent_id = registry.spawn(manifest).unwrap();

        let services = InProcessAgentServices::new(
            registry,
            tool_invocation,
            tool_registry,
            audit_log.clone(),
        );

        (services, agent_id, audit_log)
    }

    #[tokio::test]
    async fn invoke_tool_with_capability() {
        let (services, agent_id, _log) = setup();
        let result = services
            .invoke_tool(agent_id, "echo", serde_json::json!({"message": "hello"}))
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!({"message": "hello"}));
    }

    #[tokio::test]
    async fn invoke_tool_without_capability() {
        let (services, agent_id, _log) = setup();
        // Agent has tool:echo but not tool:nonexistent
        let result = services
            .invoke_tool(agent_id, "nonexistent", serde_json::json!({}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn invoke_tool_nonexistent_agent() {
        let (services, _agent_id, _log) = setup();
        let result = services
            .invoke_tool(AgentId::new(), "echo", serde_json::json!({}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn list_tools_filters_by_capability() {
        let (services, agent_id, _log) = setup();
        let tools = services.list_tools(agent_id).await.unwrap();
        // Agent has tool:echo capability, should see the echo tool
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
    }

    #[tokio::test]
    async fn report_usage_creates_audit_event() {
        let (services, agent_id, log) = setup();
        let initial_count = log.len();
        services
            .report_usage(agent_id, TokenUsage { input_tokens: 100, output_tokens: 50 })
            .await
            .unwrap();
        assert_eq!(log.len(), initial_count + 1);
    }

    #[tokio::test]
    async fn request_approval_auto_approves() {
        let (services, agent_id, _log) = setup();
        let result = services
            .request_approval(agent_id, "test action".into(), Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(result, ApprovalResult::Approved);
    }
}
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p aaos-runtime`
Expected: All tests pass (existing + 6 new).

- [ ] **Step 4: Add module and re-export in aaos-runtime lib.rs**

Add to `crates/aaos-runtime/src/lib.rs`:
```rust
pub mod services;
pub use services::InProcessAgentServices;
```

- [ ] **Step 5: Run full workspace tests**

Run: `cargo test --workspace`
Expected: All tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/aaos-runtime/
git commit -m "feat(runtime): implement InProcessAgentServices"
```

---

### Task 5: Add `tool.invoke` API method to agentd

**Files:**
- Modify: `crates/agentd/src/server.rs`

- [ ] **Step 1: Write tests for tool.invoke handler**

Add to the `tests` module in `crates/agentd/src/server.rs`:

```rust
#[tokio::test]
async fn tool_invoke_with_capability() {
    let server = Server::new();
    // Spawn an agent with tool:echo capability
    let manifest = r#"
name: tool-test
model: claude-sonnet-4-20250514
system_prompt: "test"
capabilities:
  - "tool: echo"
"#;
    let resp = server
        .handle_request(&make_request("agent.spawn", json!({"manifest": manifest})))
        .await;
    let agent_id = resp.result.unwrap()["agent_id"].as_str().unwrap().to_string();

    let resp = server
        .handle_request(&make_request(
            "tool.invoke",
            json!({"agent_id": agent_id, "tool": "echo", "input": {"message": "hello"}}),
        ))
        .await;
    assert!(resp.result.is_some());
    assert_eq!(
        resp.result.unwrap()["result"],
        json!({"message": "hello"})
    );
}

#[tokio::test]
async fn tool_invoke_without_capability() {
    let server = Server::new();
    // Spawn agent without tool capabilities
    let manifest = r#"
name: no-tools
model: claude-sonnet-4-20250514
system_prompt: "test"
capabilities:
  - web_search
"#;
    let resp = server
        .handle_request(&make_request("agent.spawn", json!({"manifest": manifest})))
        .await;
    let agent_id = resp.result.unwrap()["agent_id"].as_str().unwrap().to_string();

    let resp = server
        .handle_request(&make_request(
            "tool.invoke",
            json!({"agent_id": agent_id, "tool": "echo", "input": {"message": "hello"}}),
        ))
        .await;
    assert!(resp.error.is_some());
}

#[tokio::test]
async fn tool_invoke_nonexistent_agent() {
    let server = Server::new();
    let resp = server
        .handle_request(&make_request(
            "tool.invoke",
            json!({"agent_id": "00000000-0000-0000-0000-000000000000", "tool": "echo", "input": {}}),
        ))
        .await;
    assert!(resp.error.is_some());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p agentd -- tool_invoke`
Expected: FAIL — `tool.invoke` method not handled.

- [ ] **Step 3: Implement `tool.invoke` handler**

Add to `Server::handle_request` match in `crates/agentd/src/server.rs`:
```rust
"tool.invoke" => self.handle_tool_invoke(&request.params, request.id.clone()).await,
```

Add the handler method to the `impl Server` block:

```rust
async fn handle_tool_invoke(
    &self,
    params: &serde_json::Value,
    id: serde_json::Value,
) -> JsonRpcResponse {
    let agent_id_str = match params.get("agent_id").and_then(|a| a.as_str()) {
        Some(s) => s,
        None => {
            return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'agent_id' parameter")
        }
    };
    let agent_id: aaos_core::AgentId = match serde_json::from_value(json!(agent_id_str)) {
        Ok(id) => id,
        Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    };

    // Validate agent exists and is running
    match self.registry.get_info(agent_id) {
        Ok(info) => {
            if info.state != aaos_runtime::AgentState::Running {
                return JsonRpcResponse::error(
                    id,
                    INTERNAL_ERROR,
                    format!("agent is not running (state: {})", info.state),
                );
            }
        }
        Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }

    let tool_name = match params.get("tool").and_then(|t| t.as_str()) {
        Some(s) => s,
        None => {
            return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'tool' parameter")
        }
    };
    let input = params.get("input").cloned().unwrap_or(json!({}));

    // Get tokens and invoke
    match self.registry.get_tokens(agent_id) {
        Ok(tokens) => {
            match self.tool_invocation.invoke(agent_id, tool_name, input, &tokens).await {
                Ok(result) => JsonRpcResponse::success(id, json!({"result": result})),
                Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }
        Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}
```

Note: `get_tokens` was added to `AgentRegistry` in Task 3. Add this import at the top of `server.rs`:
```rust
use aaos_runtime::AgentState;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p agentd`
Expected: All tests pass (existing 3 + new 3).

- [ ] **Step 5: Commit**

```bash
git add crates/agentd/src/server.rs
git commit -m "feat(agentd): add tool.invoke API method"
```

---

### Task 6: Create `aaos-llm` crate — types and error

**Files:**
- Create: `crates/aaos-llm/Cargo.toml`
- Create: `crates/aaos-llm/src/lib.rs`
- Create: `crates/aaos-llm/src/error.rs`
- Create: `crates/aaos-llm/src/types.rs`
- Create: `crates/aaos-llm/src/client.rs`
- Modify: `Cargo.toml` (workspace root)

- [ ] **Step 1: Add crate to workspace and add reqwest to workspace deps**

In root `Cargo.toml`, add `"crates/aaos-llm"` to `[workspace] members` and add to `[workspace.dependencies]`:
```toml
reqwest = { version = "0.12", features = ["json"] }
```

Also add the internal crate:
```toml
aaos-llm = { path = "crates/aaos-llm" }
```

- [ ] **Step 2: Create `Cargo.toml`**

Create `crates/aaos-llm/Cargo.toml`:

```toml
[package]
name = "aaos-llm"
version = "0.0.0"
edition = "2021"
license = "Apache-2.0"
description = "LLM integration layer for aaOS — client, executor, and types"

[dependencies]
aaos-core = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
reqwest = { workspace = true }
async-trait = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
thiserror = { workspace = true }
```

- [ ] **Step 3: Create `error.rs`**

Create `crates/aaos-llm/src/error.rs`:

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("HTTP request failed: {0}")]
    HttpError(#[from] reqwest::Error),

    #[error("API returned error: {status} — {message}")]
    ApiError { status: u16, message: String },

    #[error("failed to parse API response: {0}")]
    ParseError(String),

    #[error("authentication failed — check API key")]
    AuthError,

    #[error("rate limited — retry after {retry_after_ms}ms")]
    RateLimited { retry_after_ms: u64 },

    #[error("model not supported: {model}")]
    UnsupportedModel { model: String },

    #[error("{0}")]
    Other(String),
}

pub type LlmResult<T> = std::result::Result<T, LlmError>;
```

- [ ] **Step 4: Create `types.rs`**

Create `crates/aaos-llm/src/types.rs`:

```rust
use aaos_core::{AgentId, TokenUsage, ToolDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Request to the LLM completion API.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub agent_id: AgentId,
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: u32,
}

/// Response from the LLM completion API.
#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: LlmStopReason,
    pub usage: TokenUsage,
}

/// A block of content in an LLM response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    ToolUse { id: String, name: String, input: Value },
}

/// Stop reason from the LLM API response.
/// Named `LlmStopReason` to distinguish from `aaos_core::StopReason` (agent lifecycle).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmStopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
}

/// A message in the conversation history.
#[derive(Debug, Clone)]
pub enum Message {
    User { content: String },
    Assistant { content: Vec<ContentBlock> },
    ToolResult { tool_use_id: String, content: Value, is_error: bool },
}
```

- [ ] **Step 5: Create `client.rs` with `LlmClient` trait**

Create `crates/aaos-llm/src/client.rs`:

```rust
use async_trait::async_trait;

use crate::error::LlmResult;
use crate::types::{CompletionRequest, CompletionResponse};

/// Abstraction over LLM inference providers.
///
/// The daemon holds an `Arc<dyn LlmClient>` and passes it to `AgentExecutor`.
/// In tests, this is mocked with scripted responses.
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(&self, request: CompletionRequest) -> LlmResult<CompletionResponse>;
}
```

- [ ] **Step 6: Create `lib.rs`**

Create `crates/aaos-llm/src/lib.rs`:

```rust
pub mod client;
pub mod error;
pub mod types;

pub use client::LlmClient;
pub use error::{LlmError, LlmResult};
pub use types::{
    CompletionRequest, CompletionResponse, ContentBlock, LlmStopReason, Message,
};
```

- [ ] **Step 7: Verify it compiles**

Run: `cargo build -p aaos-llm`
Expected: Compiles with no errors.

- [ ] **Step 8: Commit**

```bash
git add crates/aaos-llm/ Cargo.toml Cargo.lock
git commit -m "feat: add aaos-llm crate with types, error, and LlmClient trait"
```

---

### Task 7: Implement `AnthropicClient`

**Files:**
- Create: `crates/aaos-llm/src/anthropic.rs`
- Modify: `crates/aaos-llm/src/lib.rs`

- [ ] **Step 1: Write test for request construction and response parsing**

Create `crates/aaos-llm/src/anthropic.rs` with the implementation and inline tests. The tests mock HTTP using a local assertion on the serialization format — they do NOT call the real API.

```rust
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use aaos_core::{AgentId, TokenUsage, ToolDefinition};
use async_trait::async_trait;

use crate::client::LlmClient;
use crate::error::{LlmError, LlmResult};
use crate::types::{
    CompletionRequest, CompletionResponse, ContentBlock, LlmStopReason, Message,
};

const SUPPORTED_MODELS: &[&str] = &[
    "claude-sonnet-4-20250514",
    "claude-opus-4-20250514",
    "claude-haiku-4-5-20251001",
    "claude-opus-4-6-20250616",
    "claude-sonnet-4-6-20250725",
];

/// Configuration for the Anthropic API client.
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub api_key: String,
    pub base_url: String,
    pub default_max_tokens: u32,
}

impl AnthropicConfig {
    /// Load configuration from environment variables.
    /// API key from ANTHROPIC_API_KEY (required).
    /// Base URL from ANTHROPIC_BASE_URL (optional, defaults to https://api.anthropic.com).
    pub fn from_env() -> LlmResult<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| LlmError::AuthError)?;
        let base_url = std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
        Ok(Self {
            api_key,
            base_url,
            default_max_tokens: 4096,
        })
    }
}

/// Anthropic Messages API client.
pub struct AnthropicClient {
    config: AnthropicConfig,
    http: Client,
}

impl AnthropicClient {
    pub fn new(config: AnthropicConfig) -> Self {
        Self {
            config,
            http: Client::new(),
        }
    }

    fn validate_model(&self, model: &str) -> LlmResult<()> {
        if !SUPPORTED_MODELS.contains(&model) {
            return Err(LlmError::UnsupportedModel {
                model: model.to_string(),
            });
        }
        Ok(())
    }

    fn build_request_body(&self, request: &CompletionRequest) -> Value {
        let messages: Vec<Value> = request
            .messages
            .iter()
            .map(|msg| match msg {
                Message::User { content } => json!({
                    "role": "user",
                    "content": content,
                }),
                Message::Assistant { content } => {
                    let blocks: Vec<Value> = content
                        .iter()
                        .map(|block| match block {
                            ContentBlock::Text { text } => json!({
                                "type": "text",
                                "text": text,
                            }),
                            ContentBlock::ToolUse { id, name, input } => json!({
                                "type": "tool_use",
                                "id": id,
                                "name": name,
                                "input": input,
                            }),
                        })
                        .collect();
                    json!({ "role": "assistant", "content": blocks })
                }
                Message::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": content.to_string(),
                        "is_error": is_error,
                    }],
                }),
            })
            .collect();

        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();

        let mut body = json!({
            "model": request.model,
            "max_tokens": request.max_tokens,
            "system": request.system,
            "messages": messages,
        });

        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }

        body
    }

    fn parse_response(&self, status: u16, body: &Value) -> LlmResult<CompletionResponse> {
        if status == 401 {
            return Err(LlmError::AuthError);
        }
        if status == 429 {
            let retry_after = body
                .pointer("/error/message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown");
            return Err(LlmError::RateLimited {
                retry_after_ms: 60_000, // Default 60s if not parseable
            });
        }
        if status >= 400 {
            let message = body
                .pointer("/error/message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
                .to_string();
            return Err(LlmError::ApiError { status, message });
        }

        let content = body
            .get("content")
            .and_then(|c| c.as_array())
            .ok_or_else(|| LlmError::ParseError("missing 'content' array".into()))?;

        let blocks: Vec<ContentBlock> = content
            .iter()
            .filter_map(|block| {
                let block_type = block.get("type")?.as_str()?;
                match block_type {
                    "text" => Some(ContentBlock::Text {
                        text: block.get("text")?.as_str()?.to_string(),
                    }),
                    "tool_use" => Some(ContentBlock::ToolUse {
                        id: block.get("id")?.as_str()?.to_string(),
                        name: block.get("name")?.as_str()?.to_string(),
                        input: block.get("input")?.clone(),
                    }),
                    _ => None,
                }
            })
            .collect();

        let stop_reason = match body.get("stop_reason").and_then(|s| s.as_str()) {
            Some("end_turn") => LlmStopReason::EndTurn,
            Some("tool_use") => LlmStopReason::ToolUse,
            Some("max_tokens") => LlmStopReason::MaxTokens,
            Some("stop_sequence") => LlmStopReason::StopSequence,
            other => {
                return Err(LlmError::ParseError(format!(
                    "unknown stop_reason: {:?}",
                    other
                )))
            }
        };

        let usage = if let Some(u) = body.get("usage") {
            TokenUsage {
                input_tokens: u
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                output_tokens: u
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            }
        } else {
            TokenUsage::default()
        };

        Ok(CompletionResponse {
            content: blocks,
            stop_reason,
            usage,
        })
    }
}

#[async_trait]
impl LlmClient for AnthropicClient {
    async fn complete(&self, request: CompletionRequest) -> LlmResult<CompletionResponse> {
        self.validate_model(&request.model)?;

        let url = format!("{}/v1/messages", self.config.base_url);
        let body = self.build_request_body(&request);

        tracing::debug!(agent_id = %request.agent_id, model = %request.model, "calling LLM API");

        let response = self
            .http
            .post(&url)
            .header("x-api-key", &self.config.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = response.status().as_u16();
        let response_body: Value = response.json().await?;

        self.parse_response(status, &response_body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> AnthropicConfig {
        AnthropicConfig {
            api_key: "test-key".into(),
            base_url: "https://api.anthropic.com".into(),
            default_max_tokens: 4096,
        }
    }

    #[test]
    fn validate_supported_model() {
        let client = AnthropicClient::new(test_config());
        assert!(client.validate_model("claude-sonnet-4-20250514").is_ok());
    }

    #[test]
    fn validate_unsupported_model() {
        let client = AnthropicClient::new(test_config());
        let err = client.validate_model("gpt-4").unwrap_err();
        assert!(matches!(err, LlmError::UnsupportedModel { .. }));
    }

    #[test]
    fn build_request_body_basic() {
        let client = AnthropicClient::new(test_config());
        let request = CompletionRequest {
            agent_id: AgentId::new(),
            model: "claude-sonnet-4-20250514".into(),
            system: "You are helpful.".into(),
            messages: vec![Message::User {
                content: "Hello".into(),
            }],
            tools: vec![],
            max_tokens: 1024,
        };
        let body = client.build_request_body(&request);
        assert_eq!(body["model"], "claude-sonnet-4-20250514");
        assert_eq!(body["system"], "You are helpful.");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "Hello");
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn build_request_body_with_tools() {
        let client = AnthropicClient::new(test_config());
        let request = CompletionRequest {
            agent_id: AgentId::new(),
            model: "claude-sonnet-4-20250514".into(),
            system: "test".into(),
            messages: vec![Message::User { content: "hi".into() }],
            tools: vec![ToolDefinition {
                name: "echo".into(),
                description: "Echoes input".into(),
                input_schema: json!({"type": "object"}),
            }],
            max_tokens: 1024,
        };
        let body = client.build_request_body(&request);
        assert_eq!(body["tools"][0]["name"], "echo");
    }

    #[test]
    fn parse_text_response() {
        let client = AnthropicClient::new(test_config());
        let body = json!({
            "content": [{"type": "text", "text": "Hello!"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let resp = client.parse_response(200, &body).unwrap();
        assert_eq!(resp.content.len(), 1);
        assert!(matches!(&resp.content[0], ContentBlock::Text { text } if text == "Hello!"));
        assert_eq!(resp.stop_reason, LlmStopReason::EndTurn);
        assert_eq!(resp.usage.input_tokens, 10);
        assert_eq!(resp.usage.output_tokens, 5);
    }

    #[test]
    fn parse_tool_use_response() {
        let client = AnthropicClient::new(test_config());
        let body = json!({
            "content": [
                {"type": "text", "text": "Let me echo that."},
                {"type": "tool_use", "id": "call_1", "name": "echo", "input": {"message": "hi"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 20, "output_tokens": 15}
        });
        let resp = client.parse_response(200, &body).unwrap();
        assert_eq!(resp.content.len(), 2);
        assert!(matches!(&resp.content[1], ContentBlock::ToolUse { name, .. } if name == "echo"));
        assert_eq!(resp.stop_reason, LlmStopReason::ToolUse);
    }

    #[test]
    fn parse_auth_error() {
        let client = AnthropicClient::new(test_config());
        let body = json!({"error": {"message": "invalid api key"}});
        let err = client.parse_response(401, &body).unwrap_err();
        assert!(matches!(err, LlmError::AuthError));
    }

    #[test]
    fn parse_rate_limit_error() {
        let client = AnthropicClient::new(test_config());
        let body = json!({"error": {"message": "rate limited"}});
        let err = client.parse_response(429, &body).unwrap_err();
        assert!(matches!(err, LlmError::RateLimited { .. }));
    }

    #[test]
    fn parse_api_error() {
        let client = AnthropicClient::new(test_config());
        let body = json!({"error": {"message": "overloaded"}});
        let err = client.parse_response(529, &body).unwrap_err();
        assert!(matches!(err, LlmError::ApiError { status: 529, .. }));
    }
}
```

- [ ] **Step 2: Add module to lib.rs**

Add to `crates/aaos-llm/src/lib.rs`:
```rust
pub mod anthropic;
pub use anthropic::{AnthropicClient, AnthropicConfig};
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p aaos-llm`
Expected: All 9 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/aaos-llm/
git commit -m "feat(llm): implement AnthropicClient with request building and response parsing"
```

---

### Task 8: Implement `AgentExecutor`

**Files:**
- Create: `crates/aaos-llm/src/executor.rs`
- Modify: `crates/aaos-llm/src/lib.rs`

- [ ] **Step 1: Write tests for the execution loop using a mock LlmClient**

Create `crates/aaos-llm/src/executor.rs`:

```rust
use std::sync::Arc;

use aaos_core::{AgentId, AgentManifest, AgentServices, PromptSource, TokenUsage};
use serde_json::Value;

use crate::client::LlmClient;
use crate::error::{LlmError, LlmResult};
use crate::types::{
    CompletionRequest, CompletionResponse, ContentBlock, LlmStopReason, Message,
};

/// Configuration for the execution loop.
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// Maximum LLM API calls per execution. Default: 50.
    pub max_iterations: u32,
    /// Maximum total tokens (input + output) across all iterations. Default: 1_000_000.
    pub max_total_tokens: u64,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            max_iterations: 50,
            max_total_tokens: 1_000_000,
        }
    }
}

/// Result of an agent execution.
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub response: String,
    pub usage: TokenUsage,
    pub iterations: u32,
    pub stop_reason: ExecutionStopReason,
}

/// Why the execution loop stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionStopReason {
    Complete,
    MaxIterations,
    MaxTokens,
    Truncated,
    Error(String),
}

impl std::fmt::Display for ExecutionStopReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Complete => write!(f, "complete"),
            Self::MaxIterations => write!(f, "max_iterations"),
            Self::MaxTokens => write!(f, "max_tokens"),
            Self::Truncated => write!(f, "truncated"),
            Self::Error(msg) => write!(f, "error: {msg}"),
        }
    }
}

/// Drives an agent through the LLM inference loop.
pub struct AgentExecutor {
    llm: Arc<dyn LlmClient>,
    services: Arc<dyn AgentServices>,
    config: ExecutorConfig,
}

impl AgentExecutor {
    pub fn new(
        llm: Arc<dyn LlmClient>,
        services: Arc<dyn AgentServices>,
        config: ExecutorConfig,
    ) -> Self {
        Self {
            llm,
            services,
            config,
        }
    }

    /// Run an agent: call LLM, execute tool calls, feed results back, repeat.
    pub async fn run(
        &self,
        agent_id: AgentId,
        manifest: &AgentManifest,
        initial_message: &str,
    ) -> ExecutionResult {
        let system = match &manifest.system_prompt {
            PromptSource::Inline(s) => s.clone(),
            PromptSource::File(path) => match tokio::fs::read_to_string(path).await {
                Ok(content) => content,
                Err(e) => {
                    return ExecutionResult {
                        response: String::new(),
                        usage: TokenUsage::default(),
                        iterations: 0,
                        stop_reason: ExecutionStopReason::Error(format!(
                            "failed to read system prompt: {e}"
                        )),
                    };
                }
            },
        };

        // Get tools filtered by agent's capabilities
        let tools = match self.services.list_tools(agent_id).await {
            Ok(t) => t,
            Err(e) => {
                return ExecutionResult {
                    response: String::new(),
                    usage: TokenUsage::default(),
                    iterations: 0,
                    stop_reason: ExecutionStopReason::Error(format!(
                        "failed to list tools: {e}"
                    )),
                };
            }
        };

        let mut messages = vec![Message::User {
            content: initial_message.to_string(),
        }];
        let mut cumulative_usage = TokenUsage::default();
        let mut iterations: u32 = 0;
        let mut last_text = String::new();

        loop {
            // Call LLM
            let request = CompletionRequest {
                agent_id,
                model: manifest.model.clone(),
                system: system.clone(),
                messages: messages.clone(),
                tools: tools.clone(),
                max_tokens: 4096,
            };

            let response = match self.llm.complete(request).await {
                Ok(r) => r,
                Err(e) => {
                    return ExecutionResult {
                        response: last_text,
                        usage: cumulative_usage,
                        iterations,
                        stop_reason: ExecutionStopReason::Error(e.to_string()),
                    };
                }
            };

            iterations += 1;

            // Report and accumulate usage
            let _ = self
                .services
                .report_usage(agent_id, response.usage.clone())
                .await;
            cumulative_usage.input_tokens += response.usage.input_tokens;
            cumulative_usage.output_tokens += response.usage.output_tokens;

            // Check token budget
            if cumulative_usage.total() > self.config.max_total_tokens {
                // Extract any text from this response before stopping
                for block in &response.content {
                    if let ContentBlock::Text { text } = block {
                        last_text = text.clone();
                    }
                }
                return ExecutionResult {
                    response: last_text,
                    usage: cumulative_usage,
                    iterations,
                    stop_reason: ExecutionStopReason::MaxTokens,
                };
            }

            // Handle stop reason
            match response.stop_reason {
                LlmStopReason::EndTurn | LlmStopReason::StopSequence => {
                    // Extract text from response
                    for block in &response.content {
                        if let ContentBlock::Text { text } = block {
                            last_text = text.clone();
                        }
                    }
                    return ExecutionResult {
                        response: last_text,
                        usage: cumulative_usage,
                        iterations,
                        stop_reason: ExecutionStopReason::Complete,
                    };
                }
                LlmStopReason::MaxTokens => {
                    for block in &response.content {
                        if let ContentBlock::Text { text } = block {
                            last_text = text.clone();
                        }
                    }
                    return ExecutionResult {
                        response: last_text,
                        usage: cumulative_usage,
                        iterations,
                        stop_reason: ExecutionStopReason::Truncated,
                    };
                }
                LlmStopReason::ToolUse => {
                    // Collect tool_use blocks
                    let tool_uses: Vec<_> = response
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::ToolUse { id, name, input } => {
                                Some((id.clone(), name.clone(), input.clone()))
                            }
                            _ => None,
                        })
                        .collect();

                    // Also collect any text
                    for block in &response.content {
                        if let ContentBlock::Text { text } = block {
                            last_text = text.clone();
                        }
                    }

                    // Append assistant message
                    messages.push(Message::Assistant {
                        content: response.content.clone(),
                    });

                    // Execute each tool call sequentially
                    for (tool_use_id, tool_name, tool_input) in tool_uses {
                        match self
                            .services
                            .invoke_tool(agent_id, &tool_name, tool_input)
                            .await
                        {
                            Ok(result) => {
                                messages.push(Message::ToolResult {
                                    tool_use_id,
                                    content: result,
                                    is_error: false,
                                });
                            }
                            Err(e) => {
                                messages.push(Message::ToolResult {
                                    tool_use_id,
                                    content: Value::String(e.to_string()),
                                    is_error: true,
                                });
                            }
                        }
                    }

                    // Check iteration limit
                    if iterations >= self.config.max_iterations {
                        return ExecutionResult {
                            response: last_text,
                            usage: cumulative_usage,
                            iterations,
                            stop_reason: ExecutionStopReason::MaxIterations,
                        };
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    // Mock LLM client that returns scripted responses
    struct MockLlmClient {
        responses: Mutex<Vec<LlmResult<CompletionResponse>>>,
    }

    impl MockLlmClient {
        fn new(responses: Vec<LlmResult<CompletionResponse>>) -> Self {
            Self {
                responses: Mutex::new(responses),
            }
        }
    }

    #[async_trait]
    impl LlmClient for MockLlmClient {
        async fn complete(&self, _request: CompletionRequest) -> LlmResult<CompletionResponse> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Err(LlmError::Other("no more scripted responses".into()))
            } else {
                responses.remove(0)
            }
        }
    }

    // Mock AgentServices that tracks calls
    struct MockAgentServices {
        tool_results: Mutex<Vec<aaos_core::Result<Value>>>,
        tools: Vec<aaos_core::ToolDefinition>,
        usage_reports: Mutex<Vec<TokenUsage>>,
    }

    impl MockAgentServices {
        fn new(
            tool_results: Vec<aaos_core::Result<Value>>,
            tools: Vec<aaos_core::ToolDefinition>,
        ) -> Self {
            Self {
                tool_results: Mutex::new(tool_results),
                tools,
                usage_reports: Mutex::new(vec![]),
            }
        }
    }

    #[async_trait]
    impl AgentServices for MockAgentServices {
        async fn invoke_tool(
            &self,
            _agent_id: AgentId,
            _tool: &str,
            _input: Value,
        ) -> aaos_core::Result<Value> {
            let mut results = self.tool_results.lock().unwrap();
            if results.is_empty() {
                Err(aaos_core::CoreError::ToolNotFound("no more results".into()))
            } else {
                results.remove(0)
            }
        }

        async fn send_message(&self, _message: Value) -> aaos_core::Result<Value> {
            Ok(serde_json::json!({"status": "delivered"}))
        }

        async fn request_approval(
            &self,
            _agent_id: AgentId,
            _description: String,
            _timeout: std::time::Duration,
        ) -> aaos_core::Result<aaos_core::ApprovalResult> {
            Ok(aaos_core::ApprovalResult::Approved)
        }

        async fn report_usage(
            &self,
            _agent_id: AgentId,
            usage: TokenUsage,
        ) -> aaos_core::Result<()> {
            self.usage_reports.lock().unwrap().push(usage);
            Ok(())
        }

        async fn list_tools(
            &self,
            _agent_id: AgentId,
        ) -> aaos_core::Result<Vec<aaos_core::ToolDefinition>> {
            Ok(self.tools.clone())
        }
    }

    fn test_manifest() -> AgentManifest {
        AgentManifest::from_yaml(r#"
name: test-agent
model: claude-sonnet-4-20250514
system_prompt: "You are a test assistant."
capabilities:
  - "tool: echo"
"#).unwrap()
    }

    #[tokio::test]
    async fn simple_text_response() {
        let llm = Arc::new(MockLlmClient::new(vec![Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text: "Hello!".into(),
            }],
            stop_reason: LlmStopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
            },
        })]));
        let services = Arc::new(MockAgentServices::new(vec![], vec![]));
        let executor = AgentExecutor::new(llm, services, ExecutorConfig::default());

        let result = executor.run(AgentId::new(), &test_manifest(), "Hi").await;
        assert_eq!(result.response, "Hello!");
        assert_eq!(result.iterations, 1);
        assert_eq!(result.stop_reason, ExecutionStopReason::Complete);
        assert_eq!(result.usage.input_tokens, 10);
        assert_eq!(result.usage.output_tokens, 5);
    }

    #[tokio::test]
    async fn tool_use_then_text() {
        let llm = Arc::new(MockLlmClient::new(vec![
            // First response: tool call
            Ok(CompletionResponse {
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "echo".into(),
                    input: serde_json::json!({"message": "test"}),
                }],
                stop_reason: LlmStopReason::ToolUse,
                usage: TokenUsage { input_tokens: 20, output_tokens: 10 },
            }),
            // Second response: text
            Ok(CompletionResponse {
                content: vec![ContentBlock::Text {
                    text: "Done!".into(),
                }],
                stop_reason: LlmStopReason::EndTurn,
                usage: TokenUsage { input_tokens: 30, output_tokens: 5 },
            }),
        ]));
        let services = Arc::new(MockAgentServices::new(
            vec![Ok(serde_json::json!({"message": "test"}))],
            vec![],
        ));
        let executor = AgentExecutor::new(llm, services, ExecutorConfig::default());

        let result = executor.run(AgentId::new(), &test_manifest(), "Echo something").await;
        assert_eq!(result.response, "Done!");
        assert_eq!(result.iterations, 2);
        assert_eq!(result.stop_reason, ExecutionStopReason::Complete);
        assert_eq!(result.usage.input_tokens, 50);
        assert_eq!(result.usage.output_tokens, 15);
    }

    #[tokio::test]
    async fn tool_error_fed_back_to_llm() {
        let llm = Arc::new(MockLlmClient::new(vec![
            // First: tool call
            Ok(CompletionResponse {
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "broken".into(),
                    input: serde_json::json!({}),
                }],
                stop_reason: LlmStopReason::ToolUse,
                usage: TokenUsage { input_tokens: 10, output_tokens: 5 },
            }),
            // Second: LLM sees the error and responds with text
            Ok(CompletionResponse {
                content: vec![ContentBlock::Text {
                    text: "Tool failed, here's my answer anyway.".into(),
                }],
                stop_reason: LlmStopReason::EndTurn,
                usage: TokenUsage { input_tokens: 15, output_tokens: 10 },
            }),
        ]));
        let services = Arc::new(MockAgentServices::new(
            vec![Err(aaos_core::CoreError::ToolNotFound("broken".into()))],
            vec![],
        ));
        let executor = AgentExecutor::new(llm, services, ExecutorConfig::default());

        let result = executor.run(AgentId::new(), &test_manifest(), "Do something").await;
        assert_eq!(result.stop_reason, ExecutionStopReason::Complete);
        assert_eq!(result.iterations, 2);
        assert!(result.response.contains("Tool failed"));
    }

    #[tokio::test]
    async fn max_iterations_limit() {
        // Create an LLM that always returns tool calls
        let mut responses = Vec::new();
        for i in 0..5 {
            responses.push(Ok(CompletionResponse {
                content: vec![ContentBlock::ToolUse {
                    id: format!("call_{i}"),
                    name: "echo".into(),
                    input: serde_json::json!({}),
                }],
                stop_reason: LlmStopReason::ToolUse,
                usage: TokenUsage { input_tokens: 10, output_tokens: 5 },
            }));
        }
        let llm = Arc::new(MockLlmClient::new(responses));

        let mut tool_results = Vec::new();
        for _ in 0..5 {
            tool_results.push(Ok(serde_json::json!({"ok": true})));
        }
        let services = Arc::new(MockAgentServices::new(tool_results, vec![]));

        let config = ExecutorConfig {
            max_iterations: 3,
            max_total_tokens: 1_000_000,
        };
        let executor = AgentExecutor::new(llm, services, config);

        let result = executor.run(AgentId::new(), &test_manifest(), "Loop forever").await;
        assert_eq!(result.stop_reason, ExecutionStopReason::MaxIterations);
        assert_eq!(result.iterations, 3);
    }

    #[tokio::test]
    async fn max_tokens_budget() {
        let llm = Arc::new(MockLlmClient::new(vec![
            Ok(CompletionResponse {
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "echo".into(),
                    input: serde_json::json!({}),
                }],
                stop_reason: LlmStopReason::ToolUse,
                usage: TokenUsage { input_tokens: 500, output_tokens: 600 },
            }),
        ]));
        let services = Arc::new(MockAgentServices::new(
            vec![Ok(serde_json::json!({}))],
            vec![],
        ));
        let config = ExecutorConfig {
            max_iterations: 50,
            max_total_tokens: 100, // Very low budget
        };
        let executor = AgentExecutor::new(llm, services, config);

        let result = executor.run(AgentId::new(), &test_manifest(), "Expensive").await;
        assert_eq!(result.stop_reason, ExecutionStopReason::MaxTokens);
    }

    #[tokio::test]
    async fn llm_api_error_terminates() {
        let llm = Arc::new(MockLlmClient::new(vec![Err(LlmError::AuthError)]));
        let services = Arc::new(MockAgentServices::new(vec![], vec![]));
        let executor = AgentExecutor::new(llm, services, ExecutorConfig::default());

        let result = executor.run(AgentId::new(), &test_manifest(), "Hello").await;
        assert!(matches!(result.stop_reason, ExecutionStopReason::Error(_)));
        assert_eq!(result.iterations, 0);
    }

    #[tokio::test]
    async fn truncated_on_llm_max_tokens() {
        let llm = Arc::new(MockLlmClient::new(vec![Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text: "Partial resp...".into(),
            }],
            stop_reason: LlmStopReason::MaxTokens,
            usage: TokenUsage { input_tokens: 10, output_tokens: 4096 },
        })]));
        let services = Arc::new(MockAgentServices::new(vec![], vec![]));
        let executor = AgentExecutor::new(llm, services, ExecutorConfig::default());

        let result = executor.run(AgentId::new(), &test_manifest(), "Write a lot").await;
        assert_eq!(result.stop_reason, ExecutionStopReason::Truncated);
        assert_eq!(result.response, "Partial resp...");
    }

    #[tokio::test]
    async fn usage_reported_each_iteration() {
        let llm = Arc::new(MockLlmClient::new(vec![
            Ok(CompletionResponse {
                content: vec![ContentBlock::ToolUse {
                    id: "c1".into(), name: "echo".into(), input: serde_json::json!({}),
                }],
                stop_reason: LlmStopReason::ToolUse,
                usage: TokenUsage { input_tokens: 10, output_tokens: 5 },
            }),
            Ok(CompletionResponse {
                content: vec![ContentBlock::Text { text: "done".into() }],
                stop_reason: LlmStopReason::EndTurn,
                usage: TokenUsage { input_tokens: 20, output_tokens: 3 },
            }),
        ]));
        let services = Arc::new(MockAgentServices::new(
            vec![Ok(serde_json::json!({}))],
            vec![],
        ));
        let executor = AgentExecutor::new(llm, services.clone(), ExecutorConfig::default());

        executor.run(AgentId::new(), &test_manifest(), "hi").await;
        let reports = services.usage_reports.lock().unwrap();
        assert_eq!(reports.len(), 2);
    }
}
```

- [ ] **Step 2: Add module to lib.rs**

Add to `crates/aaos-llm/src/lib.rs`:
```rust
pub mod executor;
pub use executor::{AgentExecutor, ExecutionResult, ExecutionStopReason, ExecutorConfig};
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p aaos-llm`
Expected: All tests pass (9 from anthropic + 8 from executor = 17).

- [ ] **Step 4: Commit**

```bash
git add crates/aaos-llm/
git commit -m "feat(llm): implement AgentExecutor with execution loop"
```

---

### Task 9: Wire `agent.run` and `agent.spawn_and_run` into agentd

**Files:**
- Modify: `crates/agentd/Cargo.toml`
- Modify: `crates/agentd/src/server.rs`

**Depends on:** Tasks 3 (`get_manifest`), 4 (`InProcessAgentServices`), 5 (`tool.invoke`), 8 (`AgentExecutor`)

- [ ] **Step 1: Add aaos-llm dependency to agentd**

Add to `crates/agentd/Cargo.toml` `[dependencies]`:
```toml
aaos-llm = { workspace = true }
async-trait = { workspace = true }
```

- [ ] **Step 2: Write tests for `agent.run` and `agent.spawn_and_run`**

These tests use a mock LlmClient injected into the server. To support this, the `Server::new()` method needs to accept an optional `Arc<dyn LlmClient>`. Modify the `Server` struct and constructor:

In `crates/agentd/src/server.rs`, add a field and modify:

```rust
use aaos_llm::{LlmClient, AgentExecutor, ExecutorConfig, ExecutionStopReason};
use aaos_core::AgentServices;
use aaos_runtime::InProcessAgentServices;
```

Add to `Server` struct:
```rust
pub llm_client: Option<Arc<dyn LlmClient>>,
```

Add a second constructor:
```rust
/// Create a server with a specific LLM client (for testing).
pub fn with_llm_client(llm_client: Arc<dyn LlmClient>) -> Self {
    let mut server = Self::new();
    server.llm_client = Some(llm_client);
    server
}
```

Set `llm_client: None` in the existing `new()`.

Add test in `tests` module:

```rust
use aaos_llm::{
    CompletionResponse, ContentBlock, LlmClient, LlmError, LlmResult,
    LlmStopReason, CompletionRequest,
};
use aaos_core::TokenUsage;
use async_trait::async_trait;
use std::sync::Mutex;

struct MockLlm {
    responses: Mutex<Vec<LlmResult<CompletionResponse>>>,
}

impl MockLlm {
    fn text(text: &str) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(vec![Ok(CompletionResponse {
                content: vec![ContentBlock::Text { text: text.into() }],
                stop_reason: LlmStopReason::EndTurn,
                usage: TokenUsage { input_tokens: 10, output_tokens: 5 },
            })]),
        })
    }
}

#[async_trait]
impl LlmClient for MockLlm {
    async fn complete(&self, _req: CompletionRequest) -> LlmResult<CompletionResponse> {
        self.responses.lock().unwrap().remove(0)
    }
}

#[tokio::test]
async fn agent_spawn_and_run() {
    let server = Server::with_llm_client(MockLlm::text("I'm alive!"));
    let manifest = r#"
name: runner
model: claude-sonnet-4-20250514
system_prompt: "You are helpful."
capabilities:
  - "tool: echo"
"#;
    let resp = server
        .handle_request(&make_request(
            "agent.spawn_and_run",
            json!({"manifest": manifest, "message": "Hello"}),
        ))
        .await;
    let result = resp.result.unwrap();
    assert!(result.get("agent_id").is_some());
    assert_eq!(result["response"], "I'm alive!");
    assert_eq!(result["stop_reason"], "complete");
    assert_eq!(result["iterations"], 1);
}

#[tokio::test]
async fn agent_run_existing() {
    let server = Server::with_llm_client(MockLlm::text("Running!"));
    // First spawn
    let manifest = r#"
name: existing
model: claude-sonnet-4-20250514
system_prompt: "You are helpful."
"#;
    let resp = server
        .handle_request(&make_request("agent.spawn", json!({"manifest": manifest})))
        .await;
    let agent_id = resp.result.unwrap()["agent_id"].as_str().unwrap().to_string();

    // Then run
    let resp = server
        .handle_request(&make_request(
            "agent.run",
            json!({"agent_id": agent_id, "message": "Do something"}),
        ))
        .await;
    let result = resp.result.unwrap();
    assert_eq!(result["response"], "Running!");
}
```

- [ ] **Step 3: Implement `agent.run` and `agent.spawn_and_run` handlers**

Add to the `handle_request` match:
```rust
"agent.run" => self.handle_agent_run(&request.params, request.id.clone()).await,
"agent.spawn_and_run" => {
    self.handle_agent_spawn_and_run(&request.params, request.id.clone()).await
}
```

Add the handler methods. These need to build `InProcessAgentServices` and `AgentExecutor`:

```rust
async fn handle_agent_run(
    &self,
    params: &serde_json::Value,
    id: serde_json::Value,
) -> JsonRpcResponse {
    let agent_id_str = match params.get("agent_id").and_then(|a| a.as_str()) {
        Some(s) => s,
        None => return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'agent_id' parameter"),
    };
    let agent_id: aaos_core::AgentId = match serde_json::from_value(json!(agent_id_str)) {
        Ok(id) => id,
        Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    };
    let message = match params.get("message").and_then(|m| m.as_str()) {
        Some(s) => s,
        None => return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'message' parameter"),
    };

    // Get manifest from registry
    let manifest = match self.registry.get_info(agent_id) {
        Ok(info) => {
            if info.state != aaos_runtime::AgentState::Running {
                return JsonRpcResponse::error(
                    id,
                    INTERNAL_ERROR,
                    format!("agent is not running (state: {})", info.state),
                );
            }
            // We need the full manifest — get_info only returns AgentInfo.
            // Use get_manifest instead.
            match self.registry.get_manifest(agent_id) {
                Ok(m) => m,
                Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }
        Err(e) => return JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    };

    self.execute_agent(agent_id, &manifest, message, id).await
}

async fn handle_agent_spawn_and_run(
    &self,
    params: &serde_json::Value,
    id: serde_json::Value,
) -> JsonRpcResponse {
    let message = match params.get("message").and_then(|m| m.as_str()) {
        Some(s) => s.to_string(),
        None => return JsonRpcResponse::error(id, INTERNAL_ERROR, "missing 'message' parameter"),
    };

    // Spawn first
    let spawn_resp = self.handle_agent_spawn(params, json!(null)).await;
    let agent_id_str = match spawn_resp.result {
        Some(ref v) => match v.get("agent_id").and_then(|a| a.as_str()) {
            Some(s) => s.to_string(),
            None => return JsonRpcResponse::error(id, INTERNAL_ERROR, "spawn failed"),
        },
        None => {
            return JsonRpcResponse::error(
                id,
                INTERNAL_ERROR,
                spawn_resp
                    .error
                    .map(|e| e.message)
                    .unwrap_or_else(|| "spawn failed".into()),
            )
        }
    };
    let agent_id: aaos_core::AgentId =
        serde_json::from_value(json!(agent_id_str)).unwrap();

    let manifest = self.registry.get_manifest(agent_id).unwrap();
    let mut result = self.execute_agent(agent_id, &manifest, &message, id).await;

    // Inject agent_id into the result
    if let Some(ref mut v) = result.result {
        v["agent_id"] = json!(agent_id_str);
    }
    result
}

async fn execute_agent(
    &self,
    agent_id: aaos_core::AgentId,
    manifest: &aaos_core::AgentManifest,
    message: &str,
    id: serde_json::Value,
) -> JsonRpcResponse {
    let llm = match &self.llm_client {
        Some(client) => client.clone(),
        None => {
            return JsonRpcResponse::error(id, INTERNAL_ERROR, "no LLM client configured");
        }
    };

    let services: Arc<dyn AgentServices> = Arc::new(InProcessAgentServices::new(
        self.registry.clone(),
        self.tool_invocation.clone(),
        self.tool_registry.clone(),
        self.audit_log.clone(),
    ));

    // Emit execution started audit event
    self.audit_log.record(aaos_core::AuditEvent::new(
        agent_id,
        aaos_core::AuditEventKind::AgentExecutionStarted {
            message_preview: message.chars().take(100).collect(),
        },
    ));

    let executor = AgentExecutor::new(llm, services, ExecutorConfig::default());
    let result = executor.run(agent_id, manifest, message).await;

    // Emit execution completed audit event
    self.audit_log.record(aaos_core::AuditEvent::new(
        agent_id,
        aaos_core::AuditEventKind::AgentExecutionCompleted {
            stop_reason: result.stop_reason.to_string(),
            total_iterations: result.iterations,
        },
    ));

    JsonRpcResponse::success(
        id,
        json!({
            "response": result.response,
            "usage": {
                "input_tokens": result.usage.input_tokens,
                "output_tokens": result.usage.output_tokens,
            },
            "iterations": result.iterations,
            "stop_reason": result.stop_reason.to_string(),
        }),
    )
}
```

**Note:** `get_manifest()` was added to `AgentRegistry` in Task 3 alongside `get_tokens()`.

- [ ] **Step 4: Run tests**

Run: `cargo test -p agentd`
Expected: All tests pass (existing 6 + new 2 = 8).

- [ ] **Step 5: Run full workspace tests**

Run: `cargo test --workspace`
Expected: All tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/agentd/ crates/aaos-runtime/src/registry.rs Cargo.toml Cargo.lock
git commit -m "feat(agentd): add agent.run and agent.spawn_and_run API methods"
```

---

### Task 10: Final verification

**Files:** None (read-only)

- [ ] **Step 1: Run full test suite**

Run: `cargo test --workspace`
Expected: All tests pass (52 existing + ~30 new).

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: No warnings.

- [ ] **Step 3: Run fmt check**

Run: `cargo fmt --check`
Expected: No formatting issues. If any, run `cargo fmt` first.

- [ ] **Step 4: Verify dependency graph is acyclic**

Run: `cargo tree -p agentd --depth 1`
Expected output should show:
```
agentd
├── aaos-core
├── aaos-ipc
├── aaos-llm
├── aaos-runtime
├── aaos-tools
└── ...
```

Verify `aaos-llm` does NOT depend on `aaos-runtime` or `aaos-tools`:
Run: `cargo tree -p aaos-llm --depth 1`
Expected: only `aaos-core` as internal dependency.

- [ ] **Step 5: Commit any final fixes**

If clippy/fmt required changes:
```bash
git add -A
git commit -m "chore: fix clippy warnings and formatting"
```
