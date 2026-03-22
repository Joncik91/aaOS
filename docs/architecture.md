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

- **Agent Processes** — State machine: Starting → Running → Paused → Stopping → Stopped
- **Agent Registry** — Thread-safe process table (DashMap-based)
- **Scheduler** — Round-robin with priority support
- **Supervisor** — Restart policies (always, on-failure, never) with exponential backoff

### 3. Agent Memory Layer

Three memory tiers:

- **Context Window** — Managed by the runtime, not the agent
- **Episodic Store** — Per-agent vector-indexed persistent memory
- **Shared Knowledge** — Semantic-first storage replacing the filesystem

**Status:** Manifest declares memory config. Implementation of actual memory management is future work.

### 4. Tool & Service Layer (`aaos-tools`)

Universal tool registry where every capability is:
- Registered with a JSON schema
- Discoverable by agents
- Invoked through capability-checked channels
- Logged to the audit trail

Built-in tools: `echo` (for testing). External tools integrate via the Tool trait.

### 5. IPC Layer (`aaos-ipc`)

MCP-native inter-agent communication:

- **McpMessage** — JSON-RPC 2.0 envelope with aaOS metadata
- **MessageRouter** — Routes messages with capability validation
- **SchemaValidator** — Validates payloads against registered schemas

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

Events include trace IDs for request-level correlation and parent event IDs for causal tracing.
