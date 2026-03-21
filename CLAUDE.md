# aaOS Development Guide

## CRITICAL: Always Use Docker

**ALL builds, tests, and daemon runs MUST happen inside Docker.** Never run cargo directly on the host.

## Build Commands

```bash
docker compose run --rm test                    # Run all tests
docker compose up daemon                        # Start agentd (set ANTHROPIC_API_KEY)
docker compose run --rm dev bash                # Interactive dev shell
docker compose build                            # Rebuild after Cargo.toml changes
```

## Architecture

Rust workspace with 6 crates in `crates/`:

| Crate | Responsibility |
|-------|---------------|
| **aaos-core** | Types, traits, capability model, audit events, AgentServices trait |
| **aaos-runtime** | Agent process lifecycle, registry, InProcessAgentServices |
| **aaos-ipc** | MCP message router with capability validation |
| **aaos-tools** | Tool registry, invocation, built-in tools (echo, web_fetch, file_read, file_write) |
| **aaos-llm** | LlmClient trait, AnthropicClient, AgentExecutor (execution loop) |
| **agentd** | Daemon binary, Unix socket API, SpawnAgentTool, ApprovalQueue |

### Dependency Graph (one direction only)
```
aaos-core
  <- aaos-ipc
  <- aaos-tools
  <- aaos-runtime
  <- aaos-llm
  <- agentd
```

## Key Concepts

**Capability Tokens:** Unforgeable, UUID-identified, issued at spawn, narrowable only. `CapabilityToken::permits()` checks every operation. Path-based glob matching for file operations.

**Two-Level Enforcement:** `ToolInvocation` checks `ToolInvoke { tool_name }` (can the agent use this tool?). Inside `invoke()`, path-gated tools check specific paths against `FileRead`/`FileWrite` tokens.

**InvocationContext:** Carries `agent_id` + filtered capability tokens into every `Tool::invoke()` call. Tokens are pre-filtered by `matches_tool_capability` — file_read only sees FileRead tokens.

**Capability Narrowing:** `SpawnAgentTool` issues NEW tokens with the child's declared scope (does NOT use `narrow()` which only narrows Constraints). Parent's tokens are the ceiling.

**Approval Queue:** `ApprovalQueue` uses `oneshot::channel` for blocking. Agent execution pauses until human responds via `approval.respond` API.

## Conventions

- `thiserror` for library error types, `anyhow` in the binary
- All public types derive `Debug, Clone, Serialize, Deserialize` where possible
- Every agent action produces an `AuditEvent`
- Capability tokens are validated on every operation
- TDD: write tests before implementation

## Design Docs

| Document | What it covers |
|----------|---------------|
| [Architecture & Design](docs/product-brief.md) | Detailed architecture, design principles, OS comparison, build retrospective |
| [Execution Loop Spec](docs/superpowers/specs/2026-03-20-execution-loop-design.md) | AgentServices trait, LlmClient, AgentExecutor, tool.invoke API |
| [Tools & Orchestration Spec](docs/superpowers/specs/2026-03-21-tools-and-orchestration-design.md) | Real tools, InvocationContext, capability narrowing, spawn_agent |
| [Messaging & Approval Spec](docs/superpowers/specs/2026-03-21-messaging-and-approval-design.md) | Router wiring, ApprovalService trait, ApprovalQueue, approval API |

## Workflow

- Enter plan mode for non-trivial tasks (3+ steps or architectural decisions)
- Write specs before implementation
- Use subagents for focused, isolated tasks
- Never mark a task complete without proving it works
- Simplicity first — only touch what's necessary
