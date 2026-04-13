# aaOS Architecture

## Overview

aaOS is organized as a six-layer stack, each layer providing services to the one above it.

**Current state:** The system runs as a userspace prototype on Linux — proving the agent programming model before migrating to a real capability-based microkernel (see [Roadmap](roadmap.md), Phase F). The abstractions are designed to survive that migration: the `AgentServices` trait is the future syscall interface, and the `Tool` trait is the future driver model. Code written against these interfaces today will work unchanged on the real kernel.

## Layer Details

### 1. Hardware Abstraction Layer

Agents request compute capabilities (GPU time, network access), not device handles. Resources are allocated via capability tokens, enabling fair scheduling and budget enforcement.

**Status:** Future work. Currently relies on host OS for hardware access.

### 2. Agent Kernel (`aaos-runtime`)

The core of the system. Manages:

- **Agent Processes** — State machine: Starting → Running → Paused → Stopping → Stopped. Each process holds capability tokens, command/message channels, and an optional `JoinHandle` for persistent loop tasks.
- **Agent Registry** — Thread-safe process table (DashMap-based). Handles spawn, stop (sync and async), capability issuance, and persistent loop startup.
- **Persistent Agent Loop** — Agents with `lifecycle: persistent` run a background tokio task (`persistent_agent_loop`) that receives messages from a channel, executes them with conversation history via the LLM executor, persists the transcript, and responds via the router's pending-response map. Survives executor errors. Supports Pause/Resume/Stop commands.
- **Session Store** — `SessionStore` trait with `JsonlSessionStore` (JSONL files, one per agent) and `InMemorySessionStore` (for tests). History loaded once at loop startup, appended after each turn, compacted every 10 turns. Configurable via `max_history_messages`.
- **Scheduler** — Round-robin with priority support (implemented, not yet activated)
- **Supervisor** — Restart policies (always, on-failure, never) with exponential backoff

### 3. Agent Memory Layer

Three memory tiers:

- **Context Window** — Managed by the runtime, not the agent
- **Conversation Persistence** — JSONL session store keyed by agent ID. Persistent agents load history at startup and append after each turn. `run_with_history()` on the executor prepends prior messages and returns a transcript delta for storage.
- **Episodic Store** — Per-agent vector-indexed persistent memory (future work)
- **Shared Knowledge** — Semantic-first storage replacing the filesystem (future work)

### 4. Tool & Service Layer (`aaos-tools`)

Universal tool registry where every capability is:
- Registered with a JSON schema
- Discoverable by agents
- Invoked through capability-checked channels
- Logged to the audit trail

Built-in tools: `echo`, `web_fetch`, `file_read`, `file_write`, `spawn_agent`. External tools integrate via the Tool trait.

### 5. IPC Layer (`aaos-ipc`)

MCP-native inter-agent communication:

- **McpMessage** — JSON-RPC 2.0 envelope with aaOS metadata (sender, recipient, trace_id, capability token)
- **McpResponse** — Success/error response with responder metadata
- **MessageRouter** — Routes messages with capability validation. Supports both fire-and-forget (`route()`) and request-response (`register_pending()` / `respond()`) via a `DashMap<Uuid, oneshot::Sender<McpResponse>>` pending-response map.
- **SchemaValidator** — Validates payloads against registered schemas
- **`send_and_wait()`** — Method on `AgentServices` for request-response IPC. Creates a oneshot channel, registers it on the router, routes the message, and awaits the response with a configurable timeout. Capability-checked.

### 6. Human Supervision Layer

Web-based dashboard for monitoring agents. Deliberately last — the system must be functional without it.

**Status:** Future work.

## Capability Security Model

```
Agent Manifest declares capabilities
        ↓
Kernel issues CapabilityTokens at spawn
        ↓
Every operation validates token
        ↓
Denied operations logged to audit trail
```

Key properties:
- **No ambient authority** — Agents start with zero permissions
- **Unforgeable** — Tokens are UUID-identified, kernel-issued
- **Narrowable only** — Tokens can be constrained further, never escalated
- **Audited** — Every grant and denial is logged

## Audit Trail

Every action in aaOS produces an `AuditEvent`:

- Agent spawned/stopped
- Capability granted/denied
- Tool invoked (with input hash)
- Message sent/delivered
- Human approval requested/granted/denied
- Agent execution started/completed
- Agent loop started/stopped (persistent agents)
- Agent message received (persistent agents, with trace_id)

Events include trace IDs for request-level correlation and parent event IDs for causal tracing. 17 event kinds total.
