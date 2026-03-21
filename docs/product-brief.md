# aaOS — Architecture & Design

An operating system where AI agents are the native processes and humans are supervisors.

- **Status:** Working Prototype
- **Date:** March 2026

## The Problem

Every existing operating system assumes its primary user is a human. The entire stack — from the desktop environment down to the permission model — is built around a person sitting at a keyboard.

AI agents don't work this way. They don't need GUIs. They don't navigate filesystems by path. They don't understand Unix permissions. They communicate in structured tool calls, not raw text pipes. Every agent framework today forces agents to operate as guests inside a human's operating system, fighting abstractions that were never designed for them.

The result: container orchestrators, sandboxing layers, custom IPC protocols, agent frameworks, supervision dashboards — all recreating what an OS should provide natively.

## The Idea

Build an operating system for AI agents first, humans second. Agents are the primary processes. Humans are supervisors who observe, steer, and intervene.

- The boot sequence initializes an agent runtime, not a display server
- The process model manages agents (model + prompt + capabilities + memory), not executables
- Security uses unforgeable capability tokens, not user/group/other permissions
- IPC is typed, schema-validated structured messages (MCP), not raw byte pipes
- The "desktop" is a supervision dashboard for humans monitoring agents

## What Exists Today

The prototype runs as userspace abstractions on Linux. Docker didn't build a new OS — it built new abstractions (containers) using existing Linux primitives. aaOS follows the same pattern: prove the model first.

### Working System

**Agent Kernel (`aaos-runtime`):**
- Agent process model with state machine (Starting → Running → Paused → Stopping → Stopped)
- Thread-safe agent registry (DashMap-based process table)
- Capability token issuance, validation, and narrowing
- Message router registration on spawn, cleanup on stop

**Capability Security:**
- Unforgeable UUID-identified tokens issued at spawn
- Path-based glob matching for file operations
- Two-level enforcement: first-pass tool access check, then path-specific validation inside tools
- Capability narrowing for child agents: parent's tokens are the ceiling, child gets its declared (tighter) scope
- Every grant and denial logged to audit trail

**Tool & Service Layer (`aaos-tools`):**
- Universal tool registry with JSON schema definitions
- 5 tools: `echo`, `web_fetch`, `file_read`, `file_write`, `spawn_agent`
- `InvocationContext` carries agent_id + filtered tokens to every tool
- `InProcessAgentServices` — uniform interface (trait in core, implementation in runtime)

**LLM Integration (`aaos-llm`):**
- `LlmClient` trait with `AnthropicClient` implementation
- `AgentExecutor` — the execution loop: call LLM → parse tool_use → execute → feed results → repeat
- Iteration and token budget limits, usage reporting
- Failed tool calls fed back to LLM as errors (not terminal)

**Agent Orchestration:**
- `spawn_agent` tool: parent spawns child with narrowed capabilities
- Capability narrowing validates each child capability against parent tokens
- Ephemeral child lifecycle with cleanup guard

**Human Supervision:**
- `ApprovalQueue` with oneshot channels for blocking semantics
- `approval.list` / `approval.respond` API: human-readable pending requests
- `approval_required` manifest field: per-tool approval configuration
- Agent execution blocks until human responds

**Messaging:**
- MCP-native message router with capability-checked routing
- Fire-and-forget delivery with audit logging
- Agents registered at spawn, unregistered at stop

**Daemon (`agentd`):**
- JSON-RPC 2.0 API over Unix socket
- 10 API methods
- Docker-isolated development environment

**Numbers:** 6 Rust crates, ~4000 lines, 111 tests, end-to-end verified against real Anthropic API.

## Architecture

Six-layer stack, each layer providing services to the one above:

| Layer | Function | Status |
|-------|----------|--------|
| Human Supervision | Monitoring, auditing, intervention, policy | Approval queue working. Dashboard future. |
| Orchestration | Task graphs, delegation, resource negotiation | spawn_agent with capability narrowing working. |
| Tool & Service Layer | Universal tool registry, MCP-native IPC | Complete. 5 tools, capability-checked, audit-logged. |
| Agent Memory | Working memory, episodic store, shared knowledge | Declared in manifests, not implemented. |
| Agent Kernel | Process model, scheduling, capability security, IPC | Complete. Registry, tokens, router, approval. |
| Hardware Abstraction | GPU/NPU/network as capability-allocated resources | Future. |

## Design Principles

### 1. Agent-Native, Human-Optional
The OS boots into an agent runtime. The human supervision UI is a service that attaches remotely. The system is productive from the moment agents start.

### 2. Capability-Based Security
No ambient authority. Every agent starts with zero permissions and receives only specific, unforgeable capability tokens. Capabilities are granted at spawn and can only be narrowed, never escalated.

### 3. Structured Communication
Every interface uses typed, schema-validated messages via MCP (JSON-RPC 2.0). No raw text pipes. Everything is parseable, validatable, and auditable.

### 4. Observable by Default
Every action is logged. Every decision has a trace. Every resource consumption is metered. A kernel guarantee, not an add-on.

### 5. Reversibility as a Primitive
Side effects are captured transactionally where possible. Any agent action can be undone unless explicitly marked as irreversible.

## The Agent Process Model

An agent process is a declared bundle defined by a manifest:

```yaml
name: research-agent
model: claude-haiku-4-5-20251001
system_prompt: "You are a helpful research assistant."
capabilities:
  - web_search
  - "file_read: /data/project/*"
  - "file_write: /data/output/*"
  - "spawn_child: [summarizer]"
  - "tool: web_fetch"
  - "tool: file_write"
  - "tool: spawn_agent"
memory:
  context_window: 128k
  episodic_store: 512MB
lifecycle: on-demand
approval_required:
  - file_write
```

The kernel manages this bundle: spawning it, granting capabilities as unforgeable tokens, checking approval before sensitive actions, and logging every action to the audit trail.

## Traditional OS vs Agent-First OS

| Concept | Traditional OS | Agent-First OS |
|---------|---------------|----------------|
| Process | Executable binary with PID | Agent bundle: model + prompt + capabilities + memory |
| Filesystem | Hierarchical paths (`/home/user/`) | Semantic knowledge graph with path compatibility |
| Permissions | User/group/other (rwx) | Unforgeable capability tokens, narrowable only |
| IPC | Pipes, sockets (raw bytes) | Typed MCP messages with schema validation |
| Memory | RAM pages + swap | Context window + episodic store + shared knowledge |
| Shell | Bash — human types commands | Orchestrator — agents declare intents |
| Desktop | GUI for human interaction | Supervision dashboard for monitoring agents |
| Daemon | Background service process | Persistent agent with long-running lifecycle |
| Package Mgr | apt/npm — install software | Agent registry — deploy agent bundles with capabilities |
| Logs | Optional, per-application | Kernel-guaranteed audit trail of every action |

## Roadmap

**Phase B: Persistent Agents & Real IPC**
Agents that run continuously with message processing loops. Request-response messaging between peers. Conversation persistence across runs.

**Phase C: Memory System**
Managed context windows (runtime handles summarization and paging). Per-agent episodic stores (vector-indexed, queryable via syscall). Shared knowledge graph.

**Phase D: Supervision Dashboard**
Web-based UI — activity monitor, audit trail viewer, approval queue, policy editor. A thin client over the existing Unix socket API.

**Phase E: Inference Scheduling**
Local model support via Ollama/vLLM. GPU/NPU scheduling as a kernel concern. KV cache management as the equivalent of virtual memory.

**Phase F: Real Kernel Migration**
Push agent abstractions into a capability-based microkernel (Redox OS or seL4). Only after everything above is battle-tested.

## Build Retrospective: 48 Hours

The original brief estimated 3–5 people, 3 months. What happened: 1 person, 2 Claude sessions, 48 hours.

### Why It Was Faster

**Continuous design-build-validate loop.** No context-switching overhead. Each iteration was 30-60 minutes, not days.

**Two sessions checking each other's work.** Architecture session caught design issues before they became code: Cap'n Proto dropped (MCP is JSON-RPC 2.0), Firecracker deferred (can't isolate agents that don't execute), circular dependency fixed (trait placement, messaging types), approval queue dependency direction corrected.

**Human provided vision and routing, AI did design and implementation.** Human decided what to build and in what order. AI designed interfaces, wrote specs, wrote code, wrote tests, debugged issues. Human reviewed designs and made judgment calls.

### What the AI Got Wrong

- Cap'n Proto in the original brief — pattern-matched on "serious serialization" without thinking about the protocol stack
- `AgentServices` trait placement — would have created circular dependencies. Caught by spec reviewer.
- File write append flush — test caught a missing tokio flush. One-line fix.
- Unused imports across crates — caught by clippy in cleanup passes.

### What Required Human Judgment

- Sequencing: internal execution first, external socket later
- Dropping Cap'n Proto and deferring Firecracker — knowing when the brief was wrong
- Approval via Unix socket API, not stdout — architecturally correct vs. throwaway
- Fire-and-forget messaging for Phase A — recognizing request-response requires persistent agents
- `shell_exec` as a god-mode escape hatch — knowing what NOT to build
- Docker isolation for development — protecting production systems

## License

[Apache License 2.0](LICENSE)
