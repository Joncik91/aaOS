# Build Retrospective

## Phase A: 48 Hours

The original design estimated 3–5 people and 3 months to reach a working demo. What happened: 1 person, 2 Claude sessions, 48 hours.

### What Was Built

A working agent-first operating system prototype: 6 Rust crates, ~4000 lines, 111 tests. Agent kernel with capability-based security, tool registry with two-level enforcement, LLM execution loop, agent orchestration with capability narrowing, human-in-the-loop approval queue, and MCP-native message routing. End-to-end verified against the real Anthropic API.

### Why It Was Faster

**Continuous design-build-validate loop.** No context-switching overhead between design and implementation. Architecture session produced a spec, implementation session built it, tests verified it, cycle repeated. Each iteration was 30–60 minutes, not days.

**Two sessions checking each other's work.** The architecture session caught design issues before they became code:

- Cap'n Proto dropped — MCP is JSON-RPC 2.0; using a different serialization format would fight the protocol stack. The original brief pattern-matched on "serious OS needs serious serialization" without thinking about what the actual wire format looked like.
- Firecracker deferred — can't meaningfully isolate agents that don't execute yet. Isolation is a Phase B concern.
- Circular dependency in `AgentServices` trait placement — placing it in `aaos-core` would have forced core to depend on `aaos-ipc` and `aaos-tools`, creating cycles. Fixed by using `serde_json::Value` for messaging and moving `ToolDefinition` to core.
- Approval queue dependency direction — the trait goes in core, the implementation in agentd. Caught before any code was written.

**Human provided vision and routing, AI did detailed design and all implementation.** The human decided what to build and in what order. The AI designed the interfaces, wrote the specs, wrote the code, wrote the tests, and debugged the issues. The human reviewed designs and made judgment calls. This division eliminated the bottleneck: human creativity is slow but irreplaceable for direction; AI implementation is fast and consistent for execution.

**Subagent-driven development.** Fresh subagent per task, isolated context, spec-then-plan-then-build pipeline, review after each task. The orchestrating session coordinated without accumulating implementation details.

### What the AI Got Wrong

- **Cap'n Proto in the original brief.** Pattern-matched on convention without analyzing the actual protocol requirements.
- **`AgentServices` trait placement.** Initially placed in `aaos-core`, which would have created circular dependencies. Caught by the spec reviewer before implementation began.
- **`ToolInvocation` context passing.** The initial execution loop design didn't include `InvocationContext` — path-based capability checking wasn't possible without it. Emerged during the tools brainstorm as a necessary addition.
- **File write append flush.** The append test failed because `tokio::fs::OpenOptions` didn't flush before the test read the file. One-line fix, caught by TDD.
- **Unused imports.** Subagents occasionally left module-level imports that were only used in tests. Caught by clippy in cleanup passes.

### What Required Human Judgment

- **Build sequencing.** "Build A first, ship A, then open the socket for B" — internal execution before external protocol. The AI would have built both simultaneously.
- **Knowing when the brief was wrong.** Dropping Cap'n Proto and deferring Firecracker required recognizing that the original design document had made incorrect assumptions.
- **Approval via Unix socket API, not stdout.** The easy path was printing to stdout. The architecturally correct path was the same JSON-RPC API that a future dashboard would use. The human chose the path that wouldn't need replacement.
- **Fire-and-forget messaging for Phase A.** Request-response messaging requires persistent agents with message processing loops. The human recognized this dependency and scoped messaging to fire-and-forget, which proves the IPC layer works without requiring infrastructure that doesn't exist yet.
- **Excluding `shell_exec`.** The AI designed it as one of the initial tools. The human identified it as a capability escape hatch that bypasses every other constraint in the system, and excluded it from scope.
- **Docker isolation.** The development server runs production systems. The human mandated Docker for all aaOS development after the first real API test ran on bare metal.

### The Pattern

The 48-hour build wasn't fast because corners were cut. It was fast because the design-build-validate loop had no idle time. Every hour was either designing, implementing, testing, or reviewing. The human never waited for the AI. The AI never waited for a decision. Specs were written before code. Tests were written before implementation. Reviews happened after every task.

The bottleneck in traditional software development isn't typing speed — it's context switching, design ambiguity, and waiting for feedback. When the architect and implementer share a continuous conversation and the feedback loop is measured in minutes, three months of estimated work compresses into two days.

---

## Phase B: Persistent Agents & Request-Response IPC

Built in a single Claude session. Design spec, implementation plan, 10 tasks executed via subagent-driven development, compilation fixes, and live API verification.

### What Was Built

Persistent agents, request-response IPC, and conversation persistence. The codebase grew from ~4000 to ~8000 lines, 111 tests to 141. Three sub-specs implemented:

1. **Persistent agent lifecycle.** Agents declared as `lifecycle: persistent` run a tokio background task (`persistent_agent_loop`) that processes messages sequentially from a channel, maintains conversation history in memory, and responds via a pending-response map on the router. Pause/Resume/Stop commands work. The loop survives executor errors without crashing.

2. **Request-response IPC.** `DashMap<Uuid, oneshot::Sender<McpResponse>>` on `MessageRouter`. Callers register a oneshot channel keyed by trace_id, route the message, and await the response with a timeout. `send_and_wait()` on the `AgentServices` trait with capability enforcement.

3. **Conversation persistence.** `SessionStore` trait with `JsonlSessionStore` (one JSONL file per agent). History loaded once at loop startup, appended after each turn, compacted every 10 turns. `max_history_messages` config for trimming. `run_with_history()` on `AgentExecutor` accepts prior messages and returns a transcript delta for storage.

### What the AI Got Wrong

- **Persistent loop not wired to spawn.** The plan had `start_persistent_loop()` as a separate method, but `spawn_from_yaml()` in the server never called it. Messages were delivered to the channel but nobody was consuming them. Session store was empty. Caught by the live API smoke test — the most important test we ran.
- **`&AgentId` vs `AgentId`.** `AgentId` is `Copy`, but the persistent loop passed it by reference to `run_with_history()` which takes it by value. One-character fix, caught by the compiler.
- **Binary crate can't be imported by integration tests.** `agentd` was a `[[bin]]`-only crate. Integration tests couldn't reference `agentd::server::Server`. Fixed by adding a `[lib]` target and `lib.rs` re-exporting the modules.
- **Unused imports.** Subagents left `AgentServices`, `SessionStore`, `Mutex` imports that weren't used in the final code. Cleaned up after first compilation.

### What Required Human Judgment

- **"Did you implement all 3 sub-specs?"** The human asked for explicit verification against the design spec before accepting the work as done. The AI had reported completion but the human demanded a checklist.
- **"What about e2e Phase A and B?"** The AI tested Phase B in isolation. The human recognized that Phase A + Phase B integration hadn't been verified and asked for a combined test.
- **"Use the key from NarrativeEngine."** The human connected the prior session's API testing approach to Phase B, ensuring the same verification standard applied. Without this, Phase B would have shipped without real API validation — and the spawn wiring bug would have been missed.
- **Ordering: design spec reviewed by Qwen + GPT-5.4 before implementation.** The human insisted on peer review of the Phase B design before any code was written. Both reviewers caught real issues (don't embed oneshot in McpMessage, load history once not per-message, executor must return transcript delta).

### The Pattern (Evolved)

Phase B used the same design-build-validate loop as Phase A, but with a key addition: **subagent-driven development**. Each of the 10 implementation tasks was dispatched to a fresh subagent with isolated context, then the result was verified. This kept the orchestrating session's context clean for coordination while subagents handled the mechanical implementation.

The live API smoke test proved its worth immediately — it caught the most serious bug in the implementation (persistent loop never starting). Mock tests passed because they tested the loop in isolation. The integration test exposed the wiring gap between components. This validates the principle: **mock tests verify logic, live tests verify integration.**
