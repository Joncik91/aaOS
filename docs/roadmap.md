# Roadmap

The prototype demonstrates that agent-first OS abstractions work: capability-based security, structured IPC, tool execution with two-level enforcement, agent orchestration with capability narrowing, and human-in-the-loop approval. Everything below builds on this foundation.

## Phase B: Persistent Agents & Request-Response IPC

The current model is ephemeral: agents spawn, execute a task, and die. This limits agents to single-shot work. Persistent agents change the model fundamentally.

**Persistent agent lifecycle.** Agents that run continuously, maintaining state across interactions. The `Lifecycle::Persistent` manifest field already exists but isn't implemented. A persistent agent has a message processing loop: it waits for incoming messages, processes them, and responds. The `AgentProcess` already stores message receivers (`message_rx`, `response_rx`) — they're allocated at spawn but never consumed.

**Request-response messaging.** The current router delivers messages fire-and-forget. Request-response requires the sender to block until the recipient responds. Implementation: attach a `oneshot::Sender` to each message (same pattern as the approval queue). The recipient's message loop processes the message and sends a response. The sender awaits it with a timeout.

**Conversation persistence.** Each `agent.run` call currently starts with an empty message history. Persistent agents need their conversation stored and resumed. This means a session store (SQLite or filesystem-backed) keyed by agent ID, with message history that survives daemon restarts.

**What this enables:** Agents that remember context across interactions. An architect agent that maintains its understanding of the system design. A monitoring agent that tracks patterns over time. Multi-agent workflows where peers communicate directly instead of through the orchestrator.

## Phase C: Agent Memory System

Three memory tiers, replacing the flat filesystem as the primary storage abstraction.

**Managed context windows.** The runtime manages what's in the agent's context window, not the agent itself. When the window fills, the runtime summarizes older messages and pages them out — like virtual memory for attention. The agent sees a coherent conversation; the runtime handles the compression transparently.

**Episodic store.** Per-agent vector-indexed persistent memory, queryable via a `memory_query` tool. An agent stores facts, observations, and decisions. Later, it queries by meaning: "What did I learn about the authentication module?" returns relevant memories ranked by similarity. Implementation: embedded vector database (LanceDB or Qdrant) scoped by agent ID, governed by capability tokens.

**Shared knowledge graph.** The native storage abstraction is meaning, not location. Content is indexed by what it is, not where it lives. Agents query a semantic graph: "Find all files related to capability enforcement" returns relevant code, docs, and prior analysis — without the agent knowing paths. Path-based access remains for compatibility, but the native interface is semantic.

**What this enables:** Agents that learn from experience. A reviewer that remembers past code patterns and their outcomes. An architect that builds a progressively deeper understanding of the system. Shared knowledge that compounds across agents and sessions.

## Phase D: Supervision Dashboard

A web-based UI for humans to observe, steer, and intervene — the "desktop environment" for the agent OS.

**Activity monitor.** Real-time view of all running agents, their states, current tools in use, and token consumption. Like `htop` for agents.

**Audit trail viewer.** Every action is already logged. The dashboard makes it navigable: search by agent, filter by event type, trace from any action back to root cause through parent events and trace IDs.

**Approval queue UI.** The `approval.list` / `approval.respond` API already exists. The dashboard wraps it: see pending requests with full context (agent name, tool, input, file path), approve or deny with one click.

**Policy editor.** System-wide rules that apply across agents: token budget limits, auto-deny patterns (never approve writes to certain paths), rate limiting, model restrictions. Policies are enforced by the kernel, configured through the dashboard.

**Architecture:** A thin client over the existing Unix socket API. The daemon already serves all the data — the dashboard just presents it. No new backend logic required, only a frontend.

## Phase E: Inference Scheduling & Local Models

Treat LLM inference as a schedulable resource, like CPU time.

**Local model support.** Integrate Ollama or vLLM as `LlmClient` implementations alongside `AnthropicClient`. The manifest's `model` field determines which provider handles the request. Different agents can use different models: cheap local models for routine tasks, powerful API models for complex reasoning.

**Inference scheduling.** Multiple agents competing for inference time need a scheduler. The existing `RoundRobinScheduler` (implemented but dormant) becomes the inference queue. Priority-based scheduling: a critical agent gets inference before a background scanner. Budget enforcement: per-agent token limits, per-session cost caps.

**KV cache management.** For local models, the KV cache is the equivalent of virtual memory. A persistent agent's cache should survive between turns. The runtime manages cache allocation, eviction, and sharing — agents with overlapping context (same codebase, same docs) can share cache entries.

**What this enables:** Cost-effective agent fleets. A team of 20 agents where 15 run on a local 7B model and 5 use Claude for the hard decisions. GPU/NPU allocation as a kernel concern, not an application concern.

## Phase F: Real Kernel Migration

Move from userspace abstractions on Linux to a real capability-based microkernel.

**Target kernels.** Redox OS (Rust-native, capability-based, active development) or seL4 (formally verified). The agent syscall API is already defined by the `AgentServices` trait — the migration replaces the implementation, not the interface.

**What changes.** Capability tokens become kernel objects, not userspace UUIDs. Agent isolation uses hardware-enforced process boundaries, not Docker containers. The audit trail is a kernel subsystem, not an application-level log. IPC uses kernel message passing, not Unix sockets.

**What stays the same.** The `AgentServices` trait. The `Tool` trait. The manifest format. The API methods. Everything above the kernel — the entire agent programming model — is unchanged. Applications (agent manifests, tools, orchestration logic) work identically. This is the Android pattern: the app model is the product, the kernel is an implementation detail.

**Prerequisites.** Phases B through E must be battle-tested before this begins. The abstractions need to prove themselves under real workloads before being baked into a kernel where changes are expensive.
