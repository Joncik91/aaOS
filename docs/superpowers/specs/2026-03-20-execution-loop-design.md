# Execution Loop & Tool Invocation API — Design Spec

**Date:** 2026-03-20
**Status:** Approved
**Scope:** Phase 01/02 completion — agent execution loop, tool.invoke API, LLM integration

## Context

The aaOS codebase has working infrastructure: agent process model, capability tokens, tool registry, audit trail, MCP message routing, and a JSON-RPC daemon. None of it executes agents. Agents are spawned into "Running" state but don't think, don't call tools, and don't communicate. This spec bridges that gap.

## Design Decisions (Agreed)

1. **agentd owns the LLM client (Phase A).** The daemon calls the inference API, parses tool_use blocks, executes them through the tool registry, feeds results back. One process, one event loop, complete observability. External agents (Phase B, month 4+) connect via Unix socket later, yielding Hybrid (C) without a rewrite.

2. **Internal agents use the same interface external agents will.** The execution loop calls `invoke_tool` through a trait, not through a shortcut. The in-process implementation goes through the same capability checks and audit logging that the future socket implementation will. No special-casing for internal agents.

3. **Cap'n Proto dropped.** MCP is JSON-RPC 2.0. One wire format, one thing to debug. The product brief has been updated.

4. **Firecracker/gVisor deferred.** Can't isolate agents that don't execute. Get them running first.

## Type Relocation

To keep the dependency graph clean, two types move to `aaos-core` before implementation begins:

- **`ToolDefinition`** moves from `aaos-tools` to `aaos-core`. It is a pure data struct (name, description, input_schema) with no logic dependencies. Both `aaos-tools` and `aaos-llm` need it, and placing it in core avoids a dependency from `aaos-llm` to `aaos-tools`.

- **`McpMessage` and `McpResponse`** stay in `aaos-ipc`. The `AgentServices` trait uses opaque request/response types for messaging to avoid the circular dependency (see below).

## Deliverables

### 1. `AgentServices` Trait (in `aaos-core`)

The uniform interface for kernel services, consumed by both the execution loop and (future) external agents.

The trait lives in `aaos-core` because both `aaos-runtime` (the in-process implementation) and `aaos-llm` (the consumer) need it, and `aaos-core` is the only crate both depend on. To avoid `aaos-core` depending on `aaos-ipc`, the `send_message` method uses `serde_json::Value` for the message envelope rather than the concrete `McpMessage` type. The `InProcessAgentServices` implementation deserializes this into `McpMessage` internally. This preserves the dependency direction at the cost of one serde roundtrip per message — acceptable given that messaging is not in the hot path for Phase A.

```rust
#[async_trait]
pub trait AgentServices: Send + Sync {
    /// Invoke a tool on behalf of an agent, with full capability enforcement and audit logging.
    ///
    /// Tokens are looked up by agent_id from the registry, not passed per-call.
    /// This ensures checks are always against current state (revoked tokens fail immediately).
    ///
    /// Implementation note: get_tokens() clones the full token vector from the DashMap on
    /// each call. This is a per-call allocation proportional to the agent's capability count.
    /// Acceptable for the prototype; can be optimized with a token cache if profiling shows
    /// this is a bottleneck.
    ///
    /// NOTE: A future `invoke_tool_with_scope` variant may be needed for delegated
    /// invocations, where agent A invokes a tool on behalf of agent B with a restricted
    /// subset of capabilities. Not needed until orchestration layer (Phase 04).
    async fn invoke_tool(&self, agent_id: AgentId, tool: &str, input: Value) -> Result<Value>;

    /// Send a structured message to another agent.
    /// The message Value must be a valid MCP message envelope (JSON-RPC 2.0 with metadata).
    /// The implementation deserializes and routes it via the MessageRouter.
    ///
    /// Agent-to-agent messaging is deferred for Phase A — the execution loop does not
    /// currently use this method. It exists on the trait to establish the interface for
    /// Phase B external agents. The InProcessAgentServices implementation will validate
    /// and route the message, but no agent is listening on the receiving end yet.
    async fn send_message(&self, message: Value) -> Result<Value>;

    /// Request human approval. Blocks until approved, denied, or timeout.
    /// Semantically distinct from send_message — approval has blocking semantics
    /// with explicit timeout behavior. An approval request is routed to the supervisor,
    /// not to another agent.
    async fn request_approval(
        &self,
        agent_id: AgentId,
        description: String,
        timeout: Duration,
    ) -> Result<ApprovalResult>;

    /// Report token usage for cost tracking and budget enforcement.
    /// For internal agents, agentd calls this automatically after each LLM response.
    /// For external agents (Phase B), this is an honesty-based reporting mechanism
    /// that can later be made mandatory or verifiable.
    async fn report_usage(&self, agent_id: AgentId, usage: TokenUsage) -> Result<()>;

    /// List tools available to this agent (filtered by capabilities).
    /// Returns only tools the agent has capability tokens for.
    ///
    /// Implementation: calls get_tokens(agent_id) once, then iterates all registered tools
    /// checking each against the token set locally. This is O(tools * tokens) but avoids
    /// N separate DashMap lookups.
    ///
    /// This is the PRIMARY mechanism for scoping tool access — the LLM never sees tools
    /// the agent can't use. Capability enforcement at invocation time is the safety net.
    /// Filtering at the schema level is better for LLM performance: agents presented with
    /// only their permitted tools make better tool selection decisions and don't waste
    /// reasoning tokens on tools they can't invoke.
    async fn list_tools(&self, agent_id: AgentId) -> Result<Vec<ToolDefinition>>;
}
```

Supporting types (also in `aaos-core`):

```rust
pub struct TokenUsage {
    pub input_tokens: u64,   // u64 for safe cumulative tracking across long runs
    pub output_tokens: u64,
}

pub enum ApprovalResult {
    Approved,
    Denied { reason: String },
    Timeout,
}
```

### 2. `InProcessAgentServices` (in `aaos-runtime`)

The first implementation. Holds `Arc` references to registry, tool_invocation, and router. Delegates directly — no socket hop, same guarantees.

- `invoke_tool`: looks up tokens via `registry.get_tokens(agent_id)`, calls `ToolInvocation::invoke`
- `send_message`: deserializes `Value` into `McpMessage`, delegates to `MessageRouter::route`, serializes response back to `Value`
- `request_approval`: placeholder returning `Approved` for now (human supervision is Phase 05)
- `report_usage`: logs to audit trail as an `AuditEventKind::UsageReported` event. For the prototype, no per-agent accumulator — audit events are the record of truth. A `UsageTracker` with per-agent counters is a follow-up optimization when budget enforcement is implemented.
- `list_tools`: calls `get_tokens(agent_id)` once, then iterates `ToolRegistry::list()` and keeps only tools where the token set contains a matching `ToolInvoke` capability

### 3. `AgentRegistry::get_tokens()` (in `aaos-runtime`)

New method on the existing registry:

```rust
pub fn get_tokens(&self, id: AgentId) -> Result<Vec<CapabilityToken>>
```

Returns a clone of the agent's capability tokens. Acquires a DashMap read lock and clones the token vector. This is a per-call allocation proportional to the agent's capability count — acceptable for the prototype.

### 4. `tool.invoke` API Method (in `agentd`)

Exposed on the daemon's JSON-RPC API before the execution loop exists. Enables manual testing of capability enforcement, audit logging, and tool execution.

```
Method: "tool.invoke"
Params: {
    "agent_id": "<uuid>",
    "tool": "<name>",
    "input": { ... }
}
Returns: { "result": <value> }
Error: capability denial details, tool not found, agent not found, agent not running
```

Handler flow:
1. Look up agent in registry via `get_info(agent_id)` — validate exists and `state == Running`
2. Get capability tokens via `registry.get_tokens(agent_id)`
3. Call `ToolInvocation::invoke(agent_id, tool, input, tokens)`
4. Return result or structured error

### 5. New Crate: `aaos-llm`

The LLM integration layer. Depends on `aaos-core` (types, traits, `AgentServices`, `ToolDefinition`). Does NOT depend on `aaos-runtime`, `aaos-ipc`, or `aaos-tools`. `aaos-runtime` does NOT depend on `aaos-llm`.

Dependency graph (each arrow is one direction only):
```
aaos-core (traits + types, including ToolDefinition and AgentServices)
  <- aaos-ipc (MCP messaging)
  <- aaos-tools (tool registry, invocation) — re-exports ToolDefinition from core
  <- aaos-runtime (process management, InProcessAgentServices)
  <- aaos-llm (executor, LLM client)
  <- agentd (wiring)
```

#### LlmClient Trait

```rust
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(&self, request: CompletionRequest) -> LlmResult<CompletionResponse>;
}
```

#### Error Type

`aaos-llm` defines its own error type rather than extending `CoreError`:

```rust
#[derive(Debug, thiserror::Error)]
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

#### Request/Response Types

```rust
pub struct CompletionRequest {
    pub agent_id: AgentId,  // For logging, cost attribution, routing
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: u32,
}

pub struct CompletionResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: LlmStopReason,  // Named to avoid collision with aaos-core::StopReason
    pub usage: TokenUsage,
}

pub enum ContentBlock {
    Text { text: String },
    ToolUse { id: String, name: String, input: Value },
}

/// Stop reason from the LLM API response.
/// Named `LlmStopReason` to distinguish from `aaos-core::StopReason` (agent lifecycle).
pub enum LlmStopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
}

pub enum Message {
    User { content: String },
    Assistant { content: Vec<ContentBlock> },
    ToolResult { tool_use_id: String, content: Value, is_error: bool },
}
```

#### AnthropicClient

First (and only needed) `LlmClient` implementation. HTTP client calling the Anthropic Messages API.

Configuration:
```rust
pub struct AnthropicConfig {
    pub api_key: String,
    pub base_url: String,  // default: "https://api.anthropic.com"
    pub default_max_tokens: u32,  // default: 4096
}
```

**Loading:** API key from `ANTHROPIC_API_KEY` environment variable. Base URL defaults to `https://api.anthropic.com`, overridable via daemon config file or `ANTHROPIC_BASE_URL` env var. The daemon constructs `AnthropicConfig` at startup and passes it to `AnthropicClient::new()`.

**Model validation:** `AnthropicClient::complete()` validates the model string against a known list of Anthropic models before making the HTTP call. Unknown models return `LlmError::UnsupportedModel` with a clear error message. The known model list is a constant in the client, not a registry — it's updated when the crate is updated.

Uses `reqwest` for HTTP. Maps Anthropic API response format to `CompletionResponse`.

#### AgentExecutor

The execution loop.

```rust
pub struct ExecutorConfig {
    pub max_iterations: u32,       // default: 50, catches infinite loops
    pub max_total_tokens: u64,     // default: configurable, catches expensive loops
}

pub struct AgentExecutor {
    llm: Arc<dyn LlmClient>,
    services: Arc<dyn AgentServices>,
    config: ExecutorConfig,
}

pub struct ExecutionResult {
    pub response: String,           // Final text response
    pub usage: TokenUsage,          // Cumulative across all iterations
    pub iterations: u32,            // How many LLM calls were made
    pub stop_reason: ExecutionStopReason,
}

pub enum ExecutionStopReason {
    Complete,           // LLM returned end_turn with no tool calls
    MaxIterations,      // Hit iteration limit
    MaxTokens,          // Hit token budget
    Truncated,          // LLM returned max_tokens stop reason (response may be incomplete)
    Error(String),      // LLM API error (not tool error — those are fed back)
}
```

`AgentExecutor::run(agent_id, manifest, initial_message)` entry point:

1. Resolve system prompt from manifest (inline string or read from file path)
2. Get available tools via `services.list_tools(agent_id)` — already filtered by capabilities
3. Build initial messages: `[User { content: initial_message }]`
4. **Loop:**
   a. Call `llm.complete()` with current messages + tools
   b. Call `services.report_usage()` with this turn's token counts
   c. Check cumulative token budget — if exceeded, stop with `MaxTokens`
   d. Parse response content blocks and `LlmStopReason`:
      - `LlmStopReason::EndTurn` with text only → done, return `Complete`
      - `LlmStopReason::MaxTokens` → return `Truncated` with whatever text was generated (response is incomplete)
      - `LlmStopReason::StopSequence` → treat as `Complete` (stop sequence hit is a normal termination)
      - `LlmStopReason::ToolUse` → execute each tool call **sequentially** via `services.invoke_tool()`:
        - On success: append `ToolResult { is_error: false, content: result }`
        - On failure (capability denied, tool not found, execution error, timeout): append `ToolResult { is_error: true, content: error_description }` — **do not terminate the loop**. The LLM gets the error and can adapt.
      - Append assistant message + tool results to message history
      - Increment iteration counter — if exceeded, stop with `MaxIterations`
   e. Continue loop
5. Return `ExecutionResult`

**Tool calls are executed sequentially.** The Anthropic API can return multiple tool_use blocks per response. Parallel execution is a meaningful performance optimization but introduces concurrency questions around capability tokens, audit event ordering, and partial failure semantics (if tool 2 of 3 fails, do you send all results or just failures?). Sequential is simpler, correct, and fast enough for the demo. This is a noted future optimization, not a gap.

**Failed tool calls are fed back to the LLM, not terminal.** The loop only hard-terminates on iteration/budget limits or LLM API errors (where there's no agent to feed the error back to). This matches how the Anthropic API expects tool results to work and enables agents to recover from transient failures without human intervention.

### 6. `agent.run` and `agent.spawn_and_run` API Methods (in `agentd`)

```
Method: "agent.run"
Params: { "agent_id": "<uuid>", "message": "..." }
Returns: {
    "response": "...",
    "usage": { "input_tokens": N, "output_tokens": N },
    "iterations": N,
    "stop_reason": "complete" | "max_iterations" | "max_tokens" | "truncated" | "error"
}
```

```
Method: "agent.spawn_and_run"
Params: { "manifest": "<yaml>", "message": "..." }
Returns: {
    "agent_id": "<uuid>",
    "response": "...",
    "usage": { ... },
    "iterations": N,
    "stop_reason": "..."
}
```

The handler builds an `AgentExecutor` with:
- `AnthropicClient` (configured from daemon config — API key from `ANTHROPIC_API_KEY` env var, base URL from config or `ANTHROPIC_BASE_URL` env var)
- `InProcessAgentServices` (holding refs to registry, tool_invocation, router)
- Limits from manifest metadata or daemon defaults

Calls `executor.run()`, returns the result. Agent stays in registry after completion.

**Agent accumulation note:** Agents are not automatically removed after `agent.run` completes. They accumulate in the registry until explicitly stopped via `agent.stop`. A TTL-based cleanup or auto-stop-after-completion policy is future work.

**Production note:** `agent.spawn_and_run` currently accepts raw YAML in params. This is acceptable for the prototype (single caller). The production version should accept a path to a manifest file that the daemon reads from a trusted directory, or validate the YAML against a strict schema before parsing. Do not ship raw YAML parsing from untrusted socket input to production.

## New Dependencies

- `reqwest` (with `json` feature) — HTTP client for Anthropic API, used in `aaos-llm`

## Audit Trail Additions

New `AuditEventKind` variants:
- `UsageReported { input_tokens: u64, output_tokens: u64 }` — emitted after each LLM call
- `AgentExecutionStarted { message_preview: String }` — emitted when `agent.run` begins
- `AgentExecutionCompleted { stop_reason: String, total_iterations: u32 }` — emitted when execution loop finishes

## What We Are NOT Building

Explicit scope boundaries for this spec:

- **No streaming.** Batch responses only. Streaming adds complexity to the loop, the API, and the audit trail for minimal demo value.
- **No conversation persistence.** Each `agent.run` call is a cold start — messages are not stored between calls. The demo should be designed around single-shot tasks (research a topic, analyze a file), not multi-turn workflows.
- **No parallel tool execution.** Sequential only. See rationale above.
- **No scheduled agents.** Cron triggering is future work.
- **No external agent socket protocol.** Phase B, month 4+.
- **No supervisor restart integration.** Agents don't crash yet because there's no long-running loop to crash.
- **No multi-turn memory across runs.** Episodic store is declared in manifests but not implemented.
- **No agent-to-agent messaging in the execution loop.** The `send_message` method exists on the trait for Phase B, but no agent listens for messages yet. The demo is single-agent tool use, not multi-agent coordination.
- **No per-agent usage accumulation.** Usage is recorded as audit events. A `UsageTracker` with running counters for budget enforcement is future work.

## Test Strategy

- **`AgentServices` trait:** test `InProcessAgentServices` against registry/tools directly — capability enforcement, audit logging, tool filtering
- **`tool.invoke` API:** integration tests via `Server::handle_request` — invoke with/without capability, nonexistent tool, nonexistent agent, agent not in Running state
- **Execution loop:** mock `LlmClient` that returns scripted responses (text, tool_use, errors). Test iteration limits, token budget, tool failure recovery, all `LlmStopReason` variants, stop conditions. No real API calls in tests.
- **`AnthropicClient`:** tested manually against the real API. Unit tests mock HTTP responses to verify request construction and response parsing, including error responses (rate limiting, auth failures, malformed responses).
- **End-to-end:** `agent.spawn_and_run` with a mock LlmClient, verifying the full path from manifest to response.
