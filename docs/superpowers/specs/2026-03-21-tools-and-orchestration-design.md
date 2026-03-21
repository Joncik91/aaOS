# Real Tools & Agent Orchestration — Design Spec

**Date:** 2026-03-21
**Status:** Approved
**Scope:** Four real tools (web_fetch, file_read, file_write, spawn_agent) and the capability narrowing model for agent orchestration
**Depends on:** Execution loop spec (2026-03-20)

## Context

The execution loop works end-to-end — agents call the LLM, execute tools, loop. But the only tool is `echo`. Agents can't do useful work. This spec adds real tools and the ability for agents to spawn child agents with narrowed capabilities.

## Design Decisions

1. **`InvocationContext` passed to `Tool::invoke`.** Tools that need to check path-specific capabilities (file_read, file_write) receive the agent's relevant tokens. The `Tool` trait signature changes from `invoke(input)` to `invoke(input, ctx)`.

2. **Filtered tokens in context.** `ToolInvocation` filters tokens to only those matching the current tool before passing to `InvocationContext`. file_read only sees FileRead tokens.

3. **`SpawnAgentTool` holds its own server references (Option B).** It's the one tool that needs the full server context. Injected at construction, not through `InvocationContext`. The `Tool` trait doesn't bloat.

4. **No `shell_exec`.** It's a capability escape hatch that bypasses every other constraint. Not in this spec. If built later for developer convenience, labeled as god-mode.

5. **Capability narrowing for spawn_agent:** parent's tokens are the ceiling. Child manifest declares what it wants, each declared capability must match a parent token. `CapabilityToken::narrow()` produces child tokens. No matching parent token → error.

6. **Future note — delegation constraints:** The current model is "parent can delegate anything it holds, child manifest determines what it asks for." A future model might add delegation constraints to `SpawnChild` itself, e.g. `spawn_child: { delegatable: [file_read] }`. Not now.

## Deliverables

### 1. `InvocationContext` and `Tool` Trait Change

**In `aaos-core`** (since `Tool` trait may need to move, or `InvocationContext` at minimum):

Actually, `Tool` trait lives in `aaos-tools/src/tool.rs`. `InvocationContext` goes there too.

```rust
/// Context passed to a tool during invocation.
pub struct InvocationContext {
    pub agent_id: AgentId,
    /// Capability tokens relevant to this tool (pre-filtered by ToolInvocation).
    pub tokens: Vec<CapabilityToken>,
}
```

`Tool::invoke` signature changes:
```rust
async fn invoke(&self, input: Value, ctx: &InvocationContext) -> Result<Value>;
```

`EchoTool` updated to accept and ignore the context.

`ToolInvocation::invoke` signature stays the same: `invoke(agent_id, tool_name, input, tokens)`. Internal implementation updated to:
1. Do the existing first-pass capability check (`ToolInvoke { tool_name }`)
2. Filter tokens to only those matching the tool's capability type (FileRead tokens for file_read, etc.)
3. Build `InvocationContext { agent_id, tokens: filtered_tokens }`
4. Call `tool.invoke(input, &ctx)`

Call sites (`InProcessAgentServices`, `Server::handle_tool_invoke`) remain unchanged — they still pass the full token set, and `ToolInvocation` handles the filtering internally.

**`matches_tool_capability` is a hardcoded mapping** from tool name to capability type. This is intentional for the prototype — adding a `fn capability_type()` method to the `Tool` trait is a future refinement. Hardcoded is simpler and avoids changing the trait twice (once for context, once for capability type).

### 2. `web_fetch` Tool

**In `aaos-tools`.**

```rust
// Input
{ "url": "https://example.com", "max_bytes": 50000 }

// Output
{ "status": 200, "content_type": "text/html", "body": "..." }
```

- Capability: `WebSearch` (binary — you have it or you don't)
- Note: naming mismatch with tool name. When a `WebSearch` search tool is added later, split into `WebFetch` and `WebSearch` capabilities. Flagged, not built now.
- Uses `reqwest` (add to aaos-tools dependencies)
- `max_bytes` default: 50000 (~50KB), caps response body
- Follows redirects (up to 5)
- Timeout: 30 seconds
- Returns status, content_type, body as string
- Ignores `ctx.tokens` (WebSearch is binary, already checked by first-pass)

### 3. `file_read` Tool

**In `aaos-tools`.**

```rust
// Input
{ "path": "/data/project/notes.md" }

// Output
{ "content": "...", "size_bytes": 1234 }
```

- Capability: `FileRead { path_glob }`
- **Two-level capability check:**
  1. First pass (ToolInvocation): agent has `ToolInvoke { tool_name: "file_read" }` — can use this tool
  2. Inside `invoke()`: checks `ctx.tokens` for a `FileRead` token where `glob_matches(granted_glob, requested_path)` returns true
- Returns file content as UTF-8 string, plus size in bytes
- Binary files: returns error with message "binary files not yet supported" (future: base64)
- Max read: 1MB
- Errors: path denied (capability), file not found (ENOENT), not a regular file, too large, not UTF-8

### 4. `file_write` Tool

**In `aaos-tools`.**

```rust
// Input
{ "path": "/data/output/report.md", "content": "...", "append": false }

// Output
{ "bytes_written": 1234 }
```

- Capability: `FileWrite { path_glob }`
- Same two-level check as file_read
- `append`: true = append, false = overwrite. Default: false.
- Creates parent directories if they don't exist (side effect of write, not separately gated — noted design choice)
- Max write: 1MB
- Errors: path denied, write failed (permissions, disk full)

### 5. `spawn_agent` Tool

**In `aaos-tools` (or `agentd` — see note below).**

```rust
// Input
{
  "manifest": "name: researcher\nmodel: claude-sonnet-4-20250514\n...",
  "message": "Research this topic..."
}

// Output
{
  "agent_id": "uuid",
  "response": "Here are my findings...",
  "usage": { "input_tokens": 500, "output_tokens": 200 },
  "iterations": 3,
  "stop_reason": "complete"
}
```

- Capability: `SpawnChild { allowed_agents }`
- Checks `child_manifest.name` is in `allowed_agents` list (or list contains `"*"` for any)
- **Capability narrowing** (does NOT use `CapabilityToken::narrow()` — that only narrows Constraints):
  1. Parse child manifest capability declarations into `Capability` values
  2. Retrieve parent's full token set via `self.registry.get_tokens(ctx.agent_id)` (NOT from `ctx.tokens` — see token filtering section)
  3. For each child capability, find a parent token where `parent_token.permits(&child_capability)` returns true — this validates the parent holds a scope that covers the child's request
  4. Issue a **new** `CapabilityToken` for the child with the child's declared (tighter) capability and default Constraints. The child gets its own declared scope, not the parent's broader scope.
  5. If no matching parent token → error: "parent lacks {capability}, cannot delegate to child"

  Example: parent holds `FileRead { path_glob: "/data/*" }`, child declares `file_read: /data/project/*`. Step 3 calls `permits()` which calls `glob_matches("/data/*", "/data/project/*")` → true. Step 4 issues child token with `FileRead { path_glob: "/data/project/*" }`.

- **`SpawnAgentTool` struct holds its own references:**
  ```rust
  pub struct SpawnAgentTool {
      llm: Arc<dyn LlmClient>,
      registry: Arc<AgentRegistry>,
      tool_registry: Arc<ToolRegistry>,
      tool_invocation: Arc<ToolInvocation>,
      audit_log: Arc<dyn AuditLog>,
  }
  ```
  Note: the child's `InProcessAgentServices` shares the parent's `ToolRegistry` and `ToolInvocation` instances. This is intentional — all agents see the same set of registered tools. Capability tokens determine what each agent can access.

- `ctx.agent_id` is the parent's agent ID. The tool uses it to look up the parent's full tokens for narrowing.
- Constructs `InProcessAgentServices` scoped to the **child's** agent_id
- Runs child via `AgentExecutor`, returns result
- **Child cleanup:** child agent is stopped and removed from registry after execution (ephemeral). Uses a cleanup guard (Drop or explicit finally block) to ensure removal even if the executor panics or the parent is stopped. Zombies must not accumulate.
- **Critical invariant:** child's tool invocations are capability-checked against the child's narrowed tokens, not the parent's. This happens naturally because `InProcessAgentServices` calls `registry.get_tokens(child_id)` which returns the child's tokens. Required test: "child cannot invoke tool that parent has but child manifest didn't request."
- **Recursion depth:** not limited in this spec. A child with `spawn_child: [*]` can spawn grandchildren. Flagged as future work — add a `max_spawn_depth` limit when needed.
- **Child model validation:** child manifest's `model` field is passed to the LLM client as-is. Validation (is the model available/configured?) happens at call time in `AnthropicClient::complete()`, which returns `UnsupportedModel` error.

**Location note:** `SpawnAgentTool` depends on `LlmClient` (from `aaos-llm`) and `AgentExecutor`. Since `aaos-tools` must NOT depend on `aaos-llm` (dependency direction), `SpawnAgentTool` lives in `agentd` (or a new thin crate). The cleanest option: define it in `agentd/src/spawn_tool.rs` and register it alongside EchoTool in `Server::new()`. This keeps `aaos-tools` clean and puts the integration logic in the integration layer.

### 6. Token Filtering in `ToolInvocation`

Before calling `tool.invoke()`, `ToolInvocation` filters the agent's token list:

```rust
let relevant_tokens: Vec<CapabilityToken> = tokens
    .iter()
    .filter(|t| matches_tool_capability(&t.capability, tool_name))
    .cloned()
    .collect();
```

Where `matches_tool_capability` maps tool names to capability types:
- `"file_read"` → keeps `FileRead` tokens
- `"file_write"` → keeps `FileWrite` tokens
- `"web_fetch"` → keeps `WebSearch` tokens
- `"spawn_agent"` → keeps `SpawnChild` tokens only
- Default → keeps all tokens (unknown tools get everything, first-pass already validated)

**`spawn_agent` and token access:** `InvocationContext.tokens` contains only `SpawnChild` tokens (filtered like every other tool). The tool uses `ctx.tokens` to validate the spawn permission (is child name in `allowed_agents`?). For capability narrowing, `SpawnAgentTool` calls `self.registry.get_tokens(ctx.agent_id)` directly to get the parent's full token set. This is possible because `SpawnAgentTool` holds its own `Arc<AgentRegistry>`. No special case in the filtering logic.

## New Dependencies

- `reqwest` added to `aaos-tools/Cargo.toml` (for web_fetch)
- `aaos-llm` added to `agentd/Cargo.toml` (already there)

## Capability Declaration Parsing Update

The existing `parse_capability_declaration` in `AgentRegistry` needs to handle `spawn_child` declarations:

```yaml
capabilities:
  - "spawn_child: [researcher, summarizer]"
```

This parses to `Capability::SpawnChild { allowed_agents: vec!["researcher", "summarizer"] }`.

Currently `parse_capability_declaration` doesn't handle this syntax. It needs a new branch:
```rust
} else if let Some(agents) = s.strip_prefix("spawn_child:") {
    // Parse "[researcher, summarizer]" or "researcher"
    let agents = agents.trim().trim_matches(|c| c == '[' || c == ']');
    let list: Vec<String> = agents.split(',').map(|a| a.trim().to_string()).collect();
    Some(Capability::SpawnChild { allowed_agents: list })
}
```

## Test Strategy

- **InvocationContext + trait change:** update all existing tool tests to pass context, verify EchoTool still works
- **web_fetch:** test with a real URL (httpbin.org or similar), test timeout, test truncation
- **file_read:** test reading a file within glob, test path denied (outside glob), test file not found, test too large
- **file_write:** test write within glob, test path denied, test append vs overwrite, test parent dir creation
- **spawn_agent:** test happy path (parent spawns child, child runs, result returned), test capability narrowing (child gets subset), test denied (child requests capability parent doesn't have), test child can't escalate (child can't call tool it wasn't granted)
- **Integration:** `agent.spawn_and_run` with an orchestrator manifest that uses `spawn_agent` to spawn a child that uses `file_write`

## Build Order

1. `InvocationContext` + `Tool` trait change + update EchoTool + update ToolInvocation
2. `web_fetch` — simplest real tool, no path checking
3. `file_read` — introduces path-based capability checking pattern
4. `file_write` — same pattern as file_read
5. `spawn_child` capability parsing in AgentRegistry
6. `spawn_agent` tool — integration of everything
7. End-to-end test: orchestrator → child agent → tool use → result
