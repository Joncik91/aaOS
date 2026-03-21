# Build Retrospective: 48 Hours

The original design estimated 3–5 people and 3 months to reach a working demo. What happened: 1 person, 2 Claude sessions, 48 hours.

## What Was Built

A working agent-first operating system prototype: 6 Rust crates, ~6000 lines, 111 tests. Agent kernel with capability-based security, tool registry with two-level enforcement, LLM execution loop, agent orchestration with capability narrowing, human-in-the-loop approval queue, and MCP-native message routing. End-to-end verified against the real Anthropic API.

## Why It Was Faster

**Continuous design-build-validate loop.** No context-switching overhead between design and implementation. Architecture session produced a spec, implementation session built it, tests verified it, cycle repeated. Each iteration was 30–60 minutes, not days.

**Two sessions checking each other's work.** The architecture session caught design issues before they became code:

- Cap'n Proto dropped — MCP is JSON-RPC 2.0; using a different serialization format would fight the protocol stack. The original brief pattern-matched on "serious OS needs serious serialization" without thinking about what the actual wire format looked like.
- Firecracker deferred — can't meaningfully isolate agents that don't execute yet. Isolation is a Phase B concern.
- Circular dependency in `AgentServices` trait placement — placing it in `aaos-core` would have forced core to depend on `aaos-ipc` and `aaos-tools`, creating cycles. Fixed by using `serde_json::Value` for messaging and moving `ToolDefinition` to core.
- Approval queue dependency direction — the trait goes in core, the implementation in agentd. Caught before any code was written.

**Human provided vision and routing, AI did detailed design and all implementation.** The human decided what to build and in what order. The AI designed the interfaces, wrote the specs, wrote the code, wrote the tests, and debugged the issues. The human reviewed designs and made judgment calls. This division eliminated the bottleneck: human creativity is slow but irreplaceable for direction; AI implementation is fast and consistent for execution.

**Subagent-driven development.** Fresh subagent per task, isolated context, spec-then-plan-then-build pipeline, review after each task. The orchestrating session coordinated without accumulating implementation details.

## What the AI Got Wrong

- **Cap'n Proto in the original brief.** Pattern-matched on convention without analyzing the actual protocol requirements.
- **`AgentServices` trait placement.** Initially placed in `aaos-core`, which would have created circular dependencies. Caught by the spec reviewer before implementation began.
- **`ToolInvocation` context passing.** The initial execution loop design didn't include `InvocationContext` — path-based capability checking wasn't possible without it. Emerged during the tools brainstorm as a necessary addition.
- **File write append flush.** The append test failed because `tokio::fs::OpenOptions` didn't flush before the test read the file. One-line fix, caught by TDD.
- **Unused imports.** Subagents occasionally left module-level imports that were only used in tests. Caught by clippy in cleanup passes.

## What Required Human Judgment

- **Build sequencing.** "Build A first, ship A, then open the socket for B" — internal execution before external protocol. The AI would have built both simultaneously.
- **Knowing when the brief was wrong.** Dropping Cap'n Proto and deferring Firecracker required recognizing that the original design document had made incorrect assumptions.
- **Approval via Unix socket API, not stdout.** The easy path was printing to stdout. The architecturally correct path was the same JSON-RPC API that a future dashboard would use. The human chose the path that wouldn't need replacement.
- **Fire-and-forget messaging for Phase A.** Request-response messaging requires persistent agents with message processing loops. The human recognized this dependency and scoped messaging to fire-and-forget, which proves the IPC layer works without requiring infrastructure that doesn't exist yet.
- **Excluding `shell_exec`.** The AI designed it as one of the initial tools. The human identified it as a capability escape hatch that bypasses every other constraint in the system, and excluded it from scope.
- **Docker isolation.** The development server runs production systems. The human mandated Docker for all aaOS development after the first real API test ran on bare metal.

## The Pattern

The 48-hour build wasn't fast because corners were cut. It was fast because the design-build-validate loop had no idle time. Every hour was either designing, implementing, testing, or reviewing. The human never waited for the AI. The AI never waited for a decision. Specs were written before code. Tests were written before implementation. Reviews happened after every task.

The bottleneck in traditional software development isn't typing speed — it's context switching, design ambiguity, and waiting for feedback. When the architect and implementer share a continuous conversation and the feedback loop is measured in minutes, three months of estimated work compresses into two days.
