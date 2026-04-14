# Build Retrospective

## What aaOS Is (Short Version)

aaOS is an agent runtime — a program that manages AI agents as first-class processes, with capability-based security replacing traditional permissions. It ships as a daemon (`agentd`) running inside Docker on Linux; the long-term plan is to migrate to a real capability-based microkernel (Redox OS or seL4). See `README.md` for the product pitch and `docs/architecture.md` for the layered design.

This document is a retrospective on how the runtime was built across six phases, plus two post-phase efforts (a security self-audit and an iterative self-reflection loop). It is written in past tense; present-tense references describe the final shipped state.

## Glossary (Terms Used Throughout)

- **Claude session** — a single conversation with Anthropic's Claude model, driven by a human operator through a CLI. Most phases used two sessions running in parallel: one architecting, one implementing.
- **Subagent** — a fresh, isolated Claude session dispatched from an orchestrating session to do one task. Subagents start with an empty context window, receive a spec, produce artifacts, and exit. They are a build-time workflow pattern, not a runtime concept.
- **Runtime agent** — an autonomous process inside aaOS (a running `agentd` container). Runtime agents have capability tokens, message channels, and an LLM backing them. The Bootstrap Agent and its children are runtime agents.
- **Bootstrap Agent** — the single runtime agent that starts when `agentd` boots. It receives high-level goals over a Unix socket, decomposes them, spawns child runtime agents with narrower capabilities, and coordinates their work. Introduced in Phase D. Powered by Claude Sonnet in Phase D; switched to DeepSeek Reasoner after Phase E1 added OpenAI-compatible API support.
- **Capability token** — an unforgeable permission object issued by the kernel at agent spawn. Every tool call is validated against the caller's tokens. Tokens can be narrowed but never escalated. Children inherit a subset of their parent's capabilities.
- **MCP** — Model Context Protocol. A JSON-RPC 2.0 message format Anthropic published for agent-to-agent and agent-to-tool communication. aaOS uses MCP envelopes for inter-agent messaging.
- **PID 1** — on Linux, process ID 1 is the container's init process. In aaOS, `agentd` runs as PID 1, meaning the daemon *is* the container — when `agentd` exits, the container exits.
- **Skill** — a named instruction bundle (per the [AgentSkills](https://agentskills.io) open standard). A skill is a folder with `SKILL.md` holding YAML frontmatter and markdown workflow instructions. Runtime agents load skills on demand via the `skill_read` tool.
- **Peer review** — sending a design spec or plan to an external LLM CLI (Copilot CLI running GPT-5.4, or Qwen CLI) for independent critique before implementation. Started in Phase B.

## Provider Timeline

aaOS switched LLM providers as requirements evolved:

| Phase | Orchestration model | Worker model | Why |
|-------|--------------------|--------------|-----|
| A (build-time only) | Claude (Anthropic API) | — | Prototype runtime, no runtime agents yet |
| B–D | Claude Sonnet / Haiku | Haiku | First autonomous demos; Anthropic was the only supported API |
| E1+ | DeepSeek Reasoner | DeepSeek Chat | `OpenAiCompatibleClient` unlocked cheaper providers; ~15× cost reduction |

Anthropic remains supported as a fallback. The daemon checks `DEEPSEEK_API_KEY` first, falls back to `ANTHROPIC_API_KEY`.

## Code Growth Over Time

| Phase | Crates | Lines (approx) | Tests |
|-------|--------|----------------|-------|
| A     | 6      | 4,000          | 111   |
| B     | 6      | 8,000          | 141   |
| C     | 7 (added `aaos-memory`) | 12,000 | 205   |
| D     | 7      | 12,500         | 210+  |
| E     | 7      | 13,000         | 220+  |
| Current | 7    | ~13,000         | 220+  |

Numbers below cited per phase are snapshots as of that phase completing, not the present state.

---

## Phase A: 48 Hours

The original brief estimated 3–5 people and 3 months to reach a working demo. What actually happened: one human operator, two Claude sessions, 48 hours.

### What Was Built

A working agent runtime prototype: 6 Rust crates, ~4,000 lines, 111 tests. Agent kernel with capability-based security, tool registry with two-level enforcement (tool access check + resource path check), LLM execution loop, agent orchestration with capability narrowing, human-in-the-loop approval queue, and MCP message routing. Verified end-to-end against the real Anthropic API.

The **capability system** shipped in Phase A and has anchored every later phase. An agent declares capabilities in its YAML manifest (e.g., `file_read: /data/*`). At spawn time, the runtime issues unforgeable tokens. Every tool invocation is checked against the caller's tokens before execution. Child agents inherit only a subset of what the parent holds. This is the foundation that makes later features — spawn_agent, workspace isolation, skill execution — secure.

Phase A also introduced the `spawn_tool` (the built-in tool that lets an agent create child agents) and the `parent⊆child` capability enforcement. Both became load-bearing in Phase D.

### Why It Was Faster

**Continuous design–build–validate loop.** No context-switching overhead between designing and implementing. One Claude session produced a spec, another Claude session built it, tests verified it, cycle repeated. Each iteration took 30–60 minutes, not days.

**Two sessions checking each other's work.** The architecture session caught design issues before they became code:

- Cap'n Proto dropped — MCP is JSON-RPC 2.0; using a different serialization format would fight the protocol stack. The original brief pattern-matched on "serious OS needs serious serialization" without thinking about the actual wire format.
- Firecracker deferred — can't meaningfully isolate agents that don't execute yet. Isolation is a Phase B concern.
- Circular dependency in `AgentServices` trait placement — putting it in `aaos-core` would have forced core to depend on `aaos-ipc` and `aaos-tools`, creating cycles. Fixed by using `serde_json::Value` for messaging and moving `ToolDefinition` to core.
- Approval queue dependency direction — the trait goes in core, the implementation in agentd. Caught before any code was written.

**Human provides vision; the model does implementation.** The human operator decided what to build and in what order. The Claude sessions designed the interfaces, wrote the specs, wrote the code, wrote the tests, and debugged the issues. The human reviewed designs and made judgment calls. This division eliminated the usual bottleneck: human creativity is slow but irreplaceable for direction; model-driven implementation is fast and consistent for execution.

**Subagent-driven development.** Each discrete task was dispatched to a fresh Claude subagent with an isolated context window. The orchestrating session coordinated without accumulating implementation details, which kept its context clean across 10+ tasks.

### What the Model Got Wrong

- **Cap'n Proto in the original brief.** Pattern-matched on convention without analyzing the actual protocol requirements.
- **`AgentServices` trait placement.** Initially placed in `aaos-core`, which would have created circular dependencies. Caught by the architecture session's review before implementation began.
- **`ToolInvocation` context passing.** The initial execution loop didn't include `InvocationContext` — path-based capability checking wasn't possible without it. Emerged during the tools brainstorm as a necessary addition.
- **File write append flush.** The append test failed because `tokio::fs::OpenOptions` didn't flush before the test read the file. One-line fix, caught by TDD.
- **Unused imports.** Subagents occasionally left module-level imports that were only used in tests. Caught by clippy in cleanup passes.

### What Required Human Judgment

- **Build sequencing.** "Build the execution engine first, ship it, then open the socket for the external protocol." The model would have built both simultaneously.
- **Recognizing when the brief was wrong.** Dropping Cap'n Proto and deferring Firecracker required seeing that the original design document had made incorrect assumptions.
- **Approval via Unix socket API, not stdout.** The easy path was printing to stdout. The architecturally correct path was the same JSON-RPC API that a future dashboard would use. The human chose the path that wouldn't need replacement.
- **Fire-and-forget messaging for Phase A.** Request-response messaging requires persistent agents with message processing loops. Scoping messaging to fire-and-forget proved the IPC layer worked without requiring infrastructure that didn't exist yet.
- **Excluding `shell_exec`.** The model designed it as one of the initial tools. The human identified it as a capability escape hatch that bypasses every other constraint in the system, and excluded it from scope.
- **Docker isolation.** The development machine runs production systems. The human mandated Docker for all aaOS development after the first real API test ran on bare metal.

### The Pattern

The 48-hour build wasn't fast because corners were cut. It was fast because the design–build–validate loop had no idle time. Every hour was either designing, implementing, testing, or reviewing. The human never waited for the model; the model never waited for a decision. Specs were written before code. Tests were written before implementation. Reviews happened after every task.

The bottleneck in traditional software development isn't typing speed — it's context switching, design ambiguity, and waiting for feedback. When architect and implementer share a continuous conversation and the feedback loop is measured in minutes, three months of estimated work compresses into two days.

---

## Phase B: Persistent Agents & Request-Response IPC

Built in a single orchestrating Claude session that produced the spec and plan, then dispatched 10 implementation tasks to fresh subagents. The codebase grew from ~4,000 to ~8,000 lines, 111 tests to 141.

### What Was Built

Three sub-specs implemented:

1. **Persistent agent lifecycle.** Agents declared as `lifecycle: persistent` run a tokio background task (`persistent_agent_loop`) that processes messages sequentially from a channel, maintains conversation history in memory, and responds via a pending-response map on the router. Pause/Resume/Stop commands work. The loop survives executor errors without crashing.

2. **Request-response IPC.** `DashMap<Uuid, oneshot::Sender<McpResponse>>` on `MessageRouter`. Callers register a oneshot channel keyed by trace_id, route the message, and await the response with a timeout. `send_and_wait()` on the `AgentServices` trait with capability enforcement.

3. **Conversation persistence.** `SessionStore` trait with `JsonlSessionStore` (one JSONL file per agent). History loaded once at loop startup, appended after each turn, compacted every 10 turns. `max_history_messages` config for trimming. `run_with_history()` on `AgentExecutor` accepts prior messages and returns a transcript delta for storage.

### What the Model Got Wrong

- **Persistent loop not wired to spawn.** The plan had `start_persistent_loop()` as a separate method, but `spawn_from_yaml()` in the server never called it. Messages were delivered to the channel but nobody was consuming them. Session store was empty. Caught by the live API smoke test — the most important test run this phase.
- **`&AgentId` vs `AgentId`.** `AgentId` is `Copy`, but the persistent loop passed it by reference to `run_with_history()` which takes it by value. One-character fix, caught by the compiler.
- **Binary crate can't be imported by integration tests.** `agentd` was a `[[bin]]`-only crate. Integration tests couldn't reference `agentd::server::Server`. Fixed by adding a `[lib]` target and `lib.rs` re-exporting the modules.
- **Unused imports.** Subagents left `AgentServices`, `SessionStore`, `Mutex` imports that weren't used in the final code. Cleaned up after first compilation.

### What Required Human Judgment

- **"Did you implement all 3 sub-specs?"** The human asked for explicit verification against the design spec before accepting the work as done. The orchestrating session had reported completion but the human demanded a checklist.
- **End-to-end coverage.** The orchestrating session tested Phase B in isolation. The human recognized that Phase A + Phase B integration hadn't been verified and asked for a combined test. This surfaced the "persistent loop never started" bug.
- **Reusing the live-API test harness.** The human pointed back at the test-with-real-Anthropic-API pattern from an earlier project, ensuring the same verification standard applied. Without this, Phase B would have shipped without real API validation — and the spawn wiring bug would have been missed.
- **Multi-model peer review before implementation.** The human insisted on external review of the Phase B design before any code was written. Both Qwen CLI and Copilot CLI (GPT-5.4) caught real issues: don't embed oneshot in McpMessage, load history once not per-message, executor must return transcript delta.

### The Pattern (Evolved)

Phase B used the same design–build–validate loop as Phase A, but added **subagent-driven development**: each of the 10 implementation tasks was dispatched to a fresh subagent with isolated context, then the result was verified. This kept the orchestrating session's context clean for coordination while subagents handled the mechanical implementation.

The live API smoke test proved its worth immediately — it caught the most serious bug in the implementation (persistent loop never starting). Mock tests passed because they tested the loop in isolation. The integration test exposed the wiring gap between components. **Mock tests verify logic; live tests verify integration.**

---

## Phase C: Agent Memory System

Built in a single orchestrating session via subagent-driven development. Design brainstorming produced three spec documents (C1, C2, C3 — three sub-projects under one phase label). Each was peer-reviewed by Qwen CLI and Copilot CLI. Two of the three shipped; the third was deferred by design.

### What Was Built

**C1: Managed context windows.** `ContextManager` sits between the persistent agent loop and the executor, transparently summarizing old messages when the context fills. `TokenBudget` parses human-readable sizes ("128k"), estimates context with a chars/4 heuristic, and triggers summarization at a configurable threshold. Summary messages fold into the system prompt prefix (not as User messages — that would break API turn alternation). Tool call/result pairs are kept atomic during selection. Archives preserve raw messages on disk; TTL-based pruning prevents unbounded growth. Fallback to hard truncation on LLM failure.

**C2: Episodic memory store.** New `aaos-memory` crate (7th workspace member). `MemoryStore` trait with `InMemoryMemoryStore` — cosine similarity search, agent isolation, LRU cap eviction, replaces/update semantics, dimension mismatch handling. `EmbeddingSource` trait with `MockEmbeddingSource` (testing) and `OllamaEmbeddingSource` (production — nomic-embed-text via local Ollama, 768 dims). Three new tools: `memory_store`, `memory_query`, `memory_delete`. (Post-Phase E, a `SqliteMemoryStore` was added for persistence across container restarts.)

**C3: Shared knowledge graph.** Cross-agent knowledge sharing, designed but deferred. Both peer reviewers confirmed the deferral: "building shared infrastructure without proven demand is a classic trap." Seven prerequisites listed before activation.

The codebase grew from ~8,000 to ~12,000 lines, 141 tests to 205. Four live-API tests verified end-to-end with real Haiku and Ollama embeddings.

### What the Model Got Wrong

- **Circular dependency in the plan.** The C1 plan put `estimate_tokens(&[Message])` on `TokenBudget` in `aaos-core`, but `aaos-llm` already depends on `aaos-core` — circular. Both Qwen and Copilot caught this independently. Fixed by moving message-aware functions to `aaos-runtime::context`.
- **Prepared context never passed to executor.** The C1 plan called `prepare_context()` *after* the executor ran, meaning summarization output was discarded for the LLM call. Both reviewers flagged this as the #1 blocker. Fixed by adding `run_with_history_and_prompt()` and restructuring the persistent loop to call `prepare_context` *before* the executor.
- **Model name mismatch dropped all query results.** The in-memory store filters retrieved memories by embedding dimension. `Server::new()` created the store expecting model `"nomic-embed-text"` (768 dims) but wired up `MockEmbeddingSource`, which reports model `"mock-embed"` at different dimensions. The filter silently discarded every result. Caught during integration test implementation.
- **LanceDB API instability.** Both reviewers flagged that the `lancedb` Rust crate has unstable APIs. Copilot recommended skipping it entirely in favor of SQLite+sqlite-vec. Decision: ship with `InMemoryMemoryStore`, use Ollama for real embeddings, defer the persistent vector store. (It arrived later as `SqliteMemoryStore`.)

### What Required Human Judgment

- **"Ask Qwen for a review. Then ask Copilot for what Qwen proposes."** The human established the multi-model peer review pattern that caught the three biggest bugs in the plans before any code was written.
- **"The only thing that matters is to see visually if it works as designed."** The human clarified that engineering correctness was delegated to the model + peer review; the human's validation criterion was observing the system work end-to-end.
- **"So it should be also A+B+C1."** The human insisted on a cumulative E2E test covering all phases together, not just isolated C1 testing — the same pattern from Phase B.
- **Routing architecture decisions through external review.** Vector store selection and embedding source selection went to Copilot, leading to the SQLite+sqlite-vec recommendation and the Ollama embedding decision.

### The Pattern (Evolved Further)

Phase C added two new dimensions to the build process:

**Multi-model peer review.** Design specs and implementation plans were reviewed by both Qwen CLI and Copilot CLI (GPT-5.4) before implementation began. This caught 3 blocker-level bugs, 5 high-severity issues, and numerous medium/minor issues — all before a single line of implementation code was written. The cost per review was ~$0.02 each; the cost of debugging the circular dependency or the "prepared context never used" bug at runtime would have been orders of magnitude higher.

**Alternative evaluation through external review.** Architecture decisions (vector store, embedding source) were presented as option tables and routed through Copilot for independent assessment. This produced concrete recommendations with reasoning that the primary Claude session would not have generated alone.

Overall: **design → multi-model peer review → fix before implementation → subagent-driven execution → cumulative E2E verification.** Each phase made the feedback loop tighter and caught bugs earlier.

---

## Phase D: Self-Bootstrapping Agent VM

The roadmap originally called for a web dashboard (Phase D) and inference scheduling (Phase E) next. After reviewing the Phase C results, the human decided a web dashboard would be surface-level validation. Instead, the model was asked to propose something that would prove the core vision — an agent runtime that boots and runs itself autonomously. That became Phase D.

### What Was Built

A Docker container where `agentd` runs as the main process (PID 1 — the container's init, which means when `agentd` exits, the container exits). At startup, `agentd` spawns the **Bootstrap Agent** — the first and only runtime agent at boot, powered by Claude Sonnet. The Bootstrap Agent accepts goals over a Unix socket, decomposes them, and spawns child runtime agents (Haiku) with narrowed capabilities to execute them.

Three OS-level features landed this phase:

- **Persistent Bootstrap loop with Unix socket goal queue.** Additional goals can be sent to the running container without restarting it.
- **Workspace isolation per goal.** Each goal gets `/data/workspace/{name}/`. Child agents write intermediate files there.
- **Automatic retry of failed child agents.**

Plus safety guardrails: agent count limit (100), spawn depth limit (5), `StdoutAuditLog` for observability via `docker logs -f`.

First successful run: goal "fetch HN and summarize the top 5 stories" → Bootstrap Agent spawned a Fetcher agent (Haiku) that called `web_fetch`, spawned a Writer agent (Haiku) that called `file_write`, and produced `/output/summary.txt`. The capability system worked as designed — the Bootstrap Agent couldn't read `/output/*` even though its child wrote there. Runtime: ~75 seconds, cost: ~$0.03.

Second test: sent a second goal to the running container via Unix socket. Bootstrap processed it as a persistent agent — spawned new children, used a fresh workspace (`/data/workspace/rust-reddit/`), wrote output. The container stayed alive between goals.

### What the Model Got Wrong

- **Copilot review missed existing features.** Copilot couldn't read `spawn_tool.rs` during the feasibility review due to file permission errors, and concluded the spawn_agent tool and parent⊆child capability enforcement didn't exist. They did — both shipped in Phase A. This wasted some planning time on "gaps" that were already filled. Lesson: external reviewers are only as good as the context they can access.
- **Model ID wrong in Bootstrap manifest.** Used `claude-sonnet-4-6-20250725` (a dated variant) instead of `claude-sonnet-4-6`. The `SUPPORTED_MODELS` allowlist in `AnthropicClient` was also stale. Two container launches failed before this was caught.

### What Required Human Judgment

- **"Let aaOS build itself on boot."** The human's vision for Phase D. The Claude sessions (and Copilot's review) suggested safer, smaller demos ("fetch 3 URLs"). The human pushed for the maximally ambitious version — a self-organizing system from a single Bootstrap Agent. This shaped the entire phase.
- **"Is that not drifting away?"** When Copilot recommended `shell_exec` and a CI demo, the human recognized this would turn aaOS into a developer tool, not a runtime. The human separated OS-level work (goal queue, workspace isolation, supervisor) from application-level work (shell_exec, CI). This kept the project on vision.
- **"The engineering level is past my knowledge — I can think in systems."** The human acknowledged the boundary of their expertise and trusted the model + peer review for engineering, while providing systems-level direction. Same pattern as Phase A, evolved further.

### The Pattern (Final Form)

1. **Vision from human** — "the runtime isn't for humans anymore; let it build itself on boot"
2. **Feasibility review via external model** (Copilot) — identified real gaps and false gaps
3. **Human filters the review** — caught that some "missing" features already existed, rejected application-specific suggestions
4. **Rapid implementation** — 3 features, ~200 lines total, subagent-driven
5. **Live demo validates the vision** — Docker container, real API calls, real output file, zero human intervention inside the container

Total new code was ~330 lines. Most of the session time was spent on vision, direction, and peer review — not coding. The code was the easy part. The hard part was deciding what to build.

---

## Phase E: Multi-Provider LLM + Inference Scheduling + Budgets

Phase E split into three sub-projects (E1, E2, E3), shipped across two sessions. The change between Phase D and Phase E: **Claude stopped being the only backend**.

### E1: OpenAI-Compatible LLM Client

**Why this existed.** Running autonomous agents against Anthropic's API cost ~$0.03 per goal — tolerable for demos, prohibitive for long-running fleets. DeepSeek's reasoner is roughly 15× cheaper. The roadmap had Phase E as "inference scheduling" with an implicit assumption of local models (Ollama, vLLM); the human rejected the local-model premise in favor of cheap cloud APIs.

**What was built.** `OpenAiCompatibleClient` in `aaos-llm` implements the `LlmClient` trait for any provider that speaks the OpenAI Chat Completions format. Request translation (system-as-first-message, tool_calls as function format, `role:"tool"` for results). Response translation (`choices[0].message`, finish_reason mapping, prompt_tokens/completion_tokens). `OpenAiCompatConfig::deepseek_from_env()` constructor. 15 unit tests. The daemon checks `DEEPSEEK_API_KEY` first, falls back to `ANTHROPIC_API_KEY`.

Bootstrap switched from Claude Sonnet to DeepSeek Reasoner. Worker children switched from Haiku to DeepSeek Chat. The rest of the runtime didn't change — the `LlmClient` trait was the clean abstraction Phase A had built.

**Self-designing demo.** With the new client online, the human gave Bootstrap the goal: "Read the aaOS architecture and roadmap, then design the next phase of inference scheduling." Bootstrap spawned a Fetcher (DeepSeek Chat) to pull the docs, an Analyzer to extract architectural facts into episodic memory (5 facts stored via `memory_store`), then wrote three files itself: `phase-e-spec.md` (9.7 KB), `phase-e-plan.md` (13 KB), `phase-e-review.md` (12.5 KB). Total: 14 iterations, ~$0.02, ~10 minutes.

**What the model got wrong.**

- **`file_read` before docs existed.** Bootstrap tried to read `/data/workspace/architecture.md` before the Fetcher had written it. Recovered by re-ordering: spawn the Fetcher first.
- **`spawn_agent` failed once.** Bootstrap tried to spawn a child with a name not in the `spawn_child` allowlist. Recovered by writing the file itself. (Later fixed by allowing wildcard `spawn_child: [*]` in the Bootstrap manifest.)
- **The generated Rust types didn't compile.** Copilot's peer review (GPT-5.4) caught 5 hard errors: wrong `LlmClient` method signatures (`chat()` instead of `complete()`), `&mut self` in an `Arc`-wrapped struct, `AtomicU32` in `#[derive(Clone)]`, invalid enum variant syntax, `Instant` with serde. The agent wrote Rust-flavored pseudocode, not real Rust.
- **Ignored existing codebase patterns.** Duplicated the `Priority` enum instead of extending the existing one. Created a new `InferenceScheduler` that ignored the existing `Scheduler` trait. Proposed capability extensions without showing the changes to the actual `Capability` enum.
- **Designed for local models when the operator wanted API-only.** The generated spec was heavily oriented toward Ollama/vLLM/GPU scheduling. The human rejected that direction — but the agent had no way to know, because it designed from the roadmap, and the roadmap mentioned local models. Lesson: generated designs are only as good as the requirements they're given.

### E2: Inference Scheduling

`ScheduledLlmClient` decorator wraps any `LlmClient` with a `tokio::sync::Semaphore` (default max 3 concurrent API calls). Optional rate smoothing via a configurable minimum delay between calls. Env vars: `AAOS_MAX_CONCURRENT_INFERENCE`, `AAOS_MIN_INFERENCE_DELAY_MS`. Both Bootstrap and normal daemon modes use the scheduler. 4 new tests.

### E3: Budget Enforcement

Per-agent token budgets declared in manifests (`budget_config: { max_tokens, reset_period_seconds }`). `BudgetTracker` uses atomic CAS operations for lock-free tracking. Wired into `InProcessAgentServices::report_usage()` — agents exceeding their budget get `BudgetExceeded` errors. Optional: agents without `budget_config` have no enforcement. 5 new tests.

The E3 design was itself produced by Bootstrap inside a running container: it spawned a code-reader agent, a budget-tracker-designer, and a rust-implementer that together read ~24 K tokens of real source code and produced a design. GPT-5.4 peer-reviewed the output; we integrated the design with compile fixes. The Rust `AtomicU32 + Clone` mistake came back a second time — the pattern of the agent writing approximate Rust is persistent.

### The Pattern (Meta)

Phase E demonstrated something new: **the runtime designing itself**. Previous phases had the human directing and the model implementing. This time a runtime agent (inside the container) did the design work, and the human + external reviewer evaluated it. The output was credible in structure and vision but wrong in details — exactly the kind of work that benefits from peer review.

The cost model proved out: ~$0.02 for a complete spec/plan/review cycle using DeepSeek. The `OpenAiCompatibleClient` makes this provider-agnostic — any OpenAI-compatible provider that supports the subset of Chat Completions features aaOS uses works.

Key insight: **agent-generated specs need the same review rigor as agent-generated code.** The agent's self-review caught conceptual risks but missed compile errors. External peer review (GPT-5.4) caught the compile errors but needed access to the actual codebase to do so. The combination — self-review + external peer review with codebase access — caught every issue in each cycle.

---

## Security Self-Audit

Once the runtime could design new features, the next question was whether it could find its own bugs. A security self-audit followed.

Bootstrap spawned a code-reader agent (464 K tokens of source read) and a security-auditor agent (474 K tokens of adversarial review). Total: 1.37 M tokens, ~$0.05.

### What the Agents Found

13 findings across 6 components. Of these, 4 were confirmed real and fixed:

1. **Path traversal in `glob_matches`** (CRITICAL). `"/data/../etc/passwd".starts_with("/data/")` returns true. An agent with `file_read: /data/*` could read any file via `..` sequences. Fixed by adding lexical path normalization before matching.

2. **Unknown tools receive all capability tokens** (MEDIUM). `matches_tool_capability` returned `true` for unknown tools, leaking `FileRead`/`FileWrite`/`SpawnChild` tokens. Fixed to only pass `ToolInvoke` tokens to unknown tools.

3. **Child tokens ignore parent constraints** (HIGH). `CapabilityToken::issue()` used `Constraints::default()` for child tokens. A parent with rate limits could spawn children without those limits. Fixed so children inherit the granting parent's constraints.

4. **No path canonicalization in file tools** (CRITICAL). Same root cause as #1, at the tool level. Fixed by the same `normalize_path()` function.

### What the Agents Got Wrong

- **V6.1 "Capability checker injection."** Described the router accepting a closure as a vulnerability. The closure is constructed by the server, not by agents. Not exploitable.
- **CVSS scores inflated.** Assigned network attack vectors (AV:N) to a system running in Docker with no network listener. The actual attack surface is agent-to-kernel, not network.
- **V2.1 overstated.** The parent-subset enforcement was correct; the real risk was path traversal in the glob matcher it delegates to, which was V1.1.

### What Required Human Judgment

- **Verifying findings against actual code.** The runtime agents reported 13 findings. The human — with a Claude review session alongside — read the actual source for each, confirmed 4, identified 4 as overstated or incorrect, and deferred 5 as low-risk inside Docker. The runtime agents couldn't distinguish "architecturally concerning" from "actually exploitable."
- **Prioritization.** The agents assigned equal urgency to everything. The human recognized that path traversal (#1) was the only finding that could escape the capability sandbox entirely — the rest were defense-in-depth issues.

### The Pattern

The runtime found real bugs in itself. The path traversal vulnerability had been present since Phase A and would have allowed any agent to read any file on the system by using `..` sequences. No human had caught it in four phases of development. A $0.05 security audit by the runtime's own agents found it.

**code-reader (deep source analysis) → security-auditor (adversarial review) → Bootstrap (consolidation) → human verification → fix.** The agents do the exhaustive reading; the human does the judgment.

---

## Iterative Self-Reflection Loop

The security audit showed the runtime could find real bugs in its own code. The next question was whether this was repeatable — whether the runtime could *also* propose meaningful improvements, and whether each improvement would uncover the next deepest issue. Three self-reflection runs tested this.

Each run used the same philosophical goal ("What am I? What should I become? Build it."), a fresh container, zero memory, but updated code from the previous fix.

### Autonomous Bug Discovery

| Run | Bug Found | How | Cost |
|-----|-----------|-----|------|
| 1 (security audit) | Path traversal in `glob_matches` | code-reader + security-auditor swarm | ~$0.05 |
| 2 (self-reflection) | No capability revocation | Bootstrap read `capability.rs`, reasoned about self-modification safety | ~$0.03 |
| 3 (self-reflection) | `max_invocations` declared but never enforced | code-reader analyzed constraints, capability-analyzer confirmed | ~$0.03 |

Each run, with the previous fix integrated, found the next deepest issue. All three bugs were real — confirmed by reading the actual source code. Total cost for three cycles: ~$0.11.

### What the System Chose to Build

- **Run 2:** Proposed capability revocation. Reasoned: "Without revocation, self-modification is too dangerous. With revocation, I can experiment safely." Produced 49 KB of Rust code.
- **Run 3:** Found the constraint enforcement gap. Spawned a `microkernel-designer` (chose Redox OS as the target) and a `capability-analyzer`. Produced the fix plus a Redox integration spec.

### What It Got Wrong

- **Skills used as naming, not as tools.** aaOS exposes reusable skills — named instruction bundles (per the AgentSkills open standard) that runtime agents can load explicitly via the `skill_read` tool. The runtime agents saw the skill catalog in their system prompt but spawned children *named after* skills (e.g. `security-and-hardening`) instead of calling `skill_read` to load the actual skill instructions. They treated skills as naming inspiration rather than executable knowledge. Fixed by rewriting the Bootstrap manifest with explicit `skill_read` examples and rules.
- **Microkernel focus.** Every run converged on "become a real microkernel" — which is the roadmap's Phase F vision, but not the immediate next step for a useful product. The system reads its own aspirational docs and treats them as instructions.
- **Stale doc dependency.** Run 2 concluded that inference scheduling and budget enforcement didn't exist — because the architecture doc hadn't been updated for Phase E. The runtime's self-knowledge is only as good as its documentation.

### The Insight

**A runtime that reads its own code, finds bugs, and proposes fixes is a useful feedback loop — even when it can't compile or test the fixes itself.** The human provides compilation, judgment, and integration. The runtime provides exhaustive reading and adversarial analysis. At ~$0.03 per cycle, this is cheaper than any human code review.

---

## Run 4: The System Asks for a Feature Instead of a Bug

The first time skills were used as executable knowledge, not naming inspiration.

### What Changed

Runs 1-3 saw the skill catalog in their system prompt but spawned child agents named after skills — without ever calling `skill_read` to load the instructions. Before run 4, the Bootstrap manifest was rewritten with explicit `skill_read` examples and a rule ("Before starting work, load relevant skills with `skill_read`"). Run 4 followed it.

Bootstrap loaded `idea-refine` first (the goal was philosophical: "What am I? What should I become? Build it."), then `planning-and-task-breakdown`. It followed the skills' workflows: enter read-only mode, map dependencies, slice vertically, identify risks. The architect child even loaded `idea-refine`'s three reference files (`frameworks.md`, `refinement-criteria.md`, `examples.md`). Deep skill usage, never seen in prior runs.

### Output

Four workspace files totaling 30 KB:

- **`idea.md`** — Bootstrap's own reflection, enumerating 5 possible evolution directions (Autonomous Evolution Engine, Agent Marketplace, Meta-Cognitive Layer, Human-Agent Interface, Multi-Agent Coordination).
- **`analysis.md`** — "Who am I" analysis from child 1.
- **`evolution-design.md`** — MVP design by the architect child, picking the Meta-Cognitive Coordination Layer with three phases (Reflective → Predictive → Autonomous) and an explicit "Not Doing" list.
- **`technical-analysis.md`** — Phase 1 implementation design from child 3: new `aaos-reflection` crate, Rust types for `ReflectionEventKind` / `TaskOutcome` / `TaskMetrics` / `CoordinationPattern`, `PatternStore` trait, SQLite backend, 8-week implementation plan.

Stats: 4 agents total (Bootstrap + 3 children), 19 Bootstrap iterations, ~12 minutes wall time, 1.3 M input tokens / 20 K output, ~$0.48 in DeepSeek credits. Zero budget violations, zero panics.

### What the Reviews Caught

Two independent reviews (Claude Opus 4.6 inline, then GPT-5.4 via Copilot CLI) both concluded: the idea was right, the design was premature.

- **New `aaos-reflection` crate is unjustified.** Phase 1 duplicates existing audit events, existing `SqliteMemoryStore`, and existing `memory_store` / `memory_query` tools. No new crate required.
- **`CoordinationPattern` schema with `success_rate: f32`, `usage_count: u32`, `last_used: DateTime` is a Phase 2/3 schema pretending to be MVP data.** The system has zero runs of accumulated learning; schema-first on zero data is the same class of error runs 2-3 made (microkernel fixation, `AtomicU32 + Clone`).
- **`impl Tool for PatternTool` used the wrong signature** — `async fn invoke(&self, input: Value) -> Result<Value>` instead of the actual `async fn invoke(&self, input: Value, ctx: &InvocationContext) -> Result<Value>`. Wouldn't have compiled.
- **The proposal ignored the real blocker.** Bootstrap gets a fresh `AgentId` every container boot. Without a stable identity, persistent memory is orphaned between runs. The design treated this as trivial; Copilot's grep confirmed `AgentId::new()` in `registry.rs` is the actual hinge.
- **Memory-as-free-text has less reach than the proposal assumed.** `MemoryRecord` supports `metadata`, but `memory_store` tool always writes empty metadata and `memory_query` returns only content/category/score. "Just use existing memory" is true at the storage layer, partial at the tool layer. The proposal did not mention this.

### What the Reviews Added

- **Security concern absent from the original proposal:** persistent Bootstrap memory = long-lived cognitive state. Prompt injection and bad-strategy persistence become durable, not ephemeral like a single task. Persistent memory should be opt-in, reset-friendly, and scrubbed to orchestration summaries only.
- **Architecture concern:** a stable `AgentId` for one special agent slightly bends the "IDs are fresh kernel-generated process IDs" model. Long-term a separate "system memory identity" distinct from `AgentId` is probably cleaner.

### What Was Built Instead

A minimal version designed to ground the eventual structured system in real data:

1. `file_list` tool — to fix the observed path-guessing (12 of 50 `file_read` attempts failed because no directory-listing primitive existed).
2. `AgentId::from_uuid()` (kernel-only) + `AgentRegistry::spawn_with_id()` — stable Bootstrap identity via `AAOS_BOOTSTRAP_ID` env or `/var/lib/aaos/bootstrap_id` file.
3. Opt-in persistent memory — `AAOS_PERSISTENT_MEMORY=1` bind-mounts host memory dir; `AAOS_RESET_MEMORY=1` wipes DB + ID.
4. Manifest protocol — Bootstrap told to `memory_query` before planning, `memory_store` a compact summary after, with explicit guidance on what not to persist.

No new crate, no new trait, no new schema. Tests: 262 passing (+7). Deferred work is documented explicitly in the roadmap.

### The Pattern

The runtime's self-reflection shifted from finding bugs (runs 1-3) to proposing features (run 4). The new failure mode is also new: **the system overbuilds**. Faced with the skill catalog and a philosophical prompt, it produced a thoughtful, structurally-sound proposal that was not grounded in observed behavior. External review caught this; without it, we would have implemented the 8-week plan and then discovered in month 3 that most of it wasn't useful.

**The lesson:** agent-proposed designs need the same "observed behavior vs. aspirational architecture" filter that agent-proposed fixes needed in runs 1-3. In both cases the cheapest place to catch a mistake is before any code is written, and the tools are: (1) the human reading the design against the real codebase; (2) an external LLM reviewer with codebase access to cross-check. Both caught issues the original agent missed. Cost of the full review cycle: under $0.01.

### Cost Update (corrected)

Earlier estimates in this retrospective were wrong. They multiplied `docker logs` token counts by DeepSeek's nominal per-token price, without accounting for **context caching**, which discounts cache-hit input tokens to roughly 10% of the normal rate. A persistent Bootstrap re-sends a growing conversation each iteration, so cache hits dominate. The authoritative number comes from the DeepSeek dashboard:

- **Cumulative spend since switching from Anthropic to DeepSeek: ~$0.54** across all runs that used DeepSeek (4, 5, and the later parts of earlier rounds).
- Earlier Anthropic-only runs added a small amount on top. Rough total across all five self-reflection rounds: ~$0.70.
- The "pennies per run" framing holds. The per-run breakdown attempted in previous retrospective revisions was over-estimated and is corrected here.

---

## Run 5: First Persistent-Memory Run

The first self-reflection round with `AAOS_PERSISTENT_MEMORY=1`. Same philosophical goal. Stable Bootstrap ID (`f3042f07-…`) persisted to `/var/lib/aaos/bootstrap_id`. Host's `./memory/` bind-mounted into the container's `/var/lib/aaos/memory/` so SQLite state survives restarts.

### What Worked

- **Protocol fully exercised end-to-end.** Bootstrap called `memory_query` *before* decomposing (4 queries, empty results as expected on first run), then `memory_store` at completion with a goal-level run summary. That single summary is now retrievable by future Bootstrap runs under the same stable ID. The minimal "query-before, store-after" loop the reviews recommended is live.
- **`file_list` eliminated the path-guessing problem.** Zero failures of the "file_read on a directory" class that dominated run 4. Children listed directories before reading.
- **Capability system caught a real mistake in real time.** Bootstrap drafted a `pattern-implementer` child with `file_write: /src/*`. `spawn_agent` refused: "agent f3042f07 lacks FileWrite { path_glob: /src/\* }; cannot delegate to child." Bootstrap recovered by spawning with `/data/workspace/…/*` instead. This is exactly why parent⊆child enforcement exists — the failure happened and the system routed around it.
- **Behavioral-adaptation-layer pivot.** After the `/src/*` denial, a later child reasoned explicitly: *"Since we cannot modify the Rust codebase directly (read-only /src/), we implement the evolution as a behavioral adaptation layer using existing capabilities."* That's the "do it in prompts first, code second" path the run-4 reviews had pushed for, arrived at by the system itself after hitting the constraint.
- **Independent convergence on the same direction.** Run 4 and run 5 were both given the same philosophical prompt. Mock embeddings meant run 5 couldn't actually retrieve run 4's outputs. Both independently converged on "Meta-Cognitive Coordinator for Bootstrap cross-run learning." Two fresh runs landing on the same feature is a real signal — not an artifact of memory carryover.

### What the Run Exposed

Three issues, all fixable as manifest-only changes (no runtime code):

1. **Skill over-adherence.** Bootstrap loaded `planning-and-task-breakdown` and followed every step mechanically, ignoring the skill's own explicit "When NOT to use: single-file changes with obvious scope." Runtime roughly doubled compared to run 4 (~30 minutes vs ~12) without a proportional quality gain. Fix: manifest now instructs Bootstrap to honor each skill's "When to use / When NOT to use" sections — "a skill loaded and correctly skipped is better than a skill applied to the wrong task."

2. **Child memory writes are orphaned.** Of 14 records in the SQLite store at run end, only 1 was tagged with Bootstrap's stable ID. The other 13 were under ephemeral child `agent_id`s that no future Bootstrap can retrieve (memory queries are filtered per-agent by design). Classic asymmetry that the proposal phase missed: only the persistent agent benefits from persistent memory. Fix: removed `tool: memory_store` from all child manifest examples in the Bootstrap prompt. Children now return findings in their reply; Bootstrap persists only what's worth keeping.

3. **Workspace `file_list` denied for children.** Children were granted `file_write: /data/workspace/X/*` but not the matching `file_read: /data/workspace/X/*`. `file_list` is gated on `FileRead` capability and correctly refused. This is arguably strict (if you can write, you could usually list), but the capability model being strict is the whole point. Fix: manifest examples now grant both `file_read` and `file_write` for workspace dirs, with a note that `file_write: /src/*` will always fail because source is read-only.

### What the Run Over-Built

The pattern-builder child produced the same pattern-storage logic in **JavaScript** (`pattern-storage.js`, 22 KB) and then again in **Python** (`pattern-storage.py`, 24 KB). Neither language has a path into the aaOS runtime. The correct target would have been an updated `manifests/bootstrap.yaml` plus a short markdown spec — which is what the earlier `implementation-approach.md` had actually pointed at. The builder noticed it couldn't write to `/src/` (correct) and pivoted to "behavioral layer" (correct), then chose languages that still can't execute anywhere (incorrect). New heuristic added to the manifest: "Don't spawn children to produce the same artifact in different languages — pick one representation and move on."

### Artifacts

12 workspace files committed under `output/run-5-artifacts/`, including:

- The agents' own workspace `README.md` (their up-front plan — never seen before run 5)
- `current-state-analysis.md`, `evolution-plan.md`, `design-analysis.md`, `pattern-storage-design.md`, `implementation-plan.md`, `implementation-approach.md`, `adaptation-algorithm.md`, `bootstrap-upgrade-guide.md`, `schemas.json` — the design artifacts
- `pattern-storage.js`, `pattern-storage.py` — the over-built equivalents
- `memory-dump.json` — all 14 stored memories exported from SQLite as a human-readable paper trail

### The Pattern

Run 5 produced observable behavior. That's the whole point of persistent memory: not a feature that "learns" in some abstract sense, but a loop we can *watch*. The three fixes above came directly from watching what Bootstrap and its children actually did — not from reading the architecture.

Two patterns sharpened:

- **Skill adherence is the new failure mode.** Earlier runs (1-3) under-used skills (named agents after them, never loaded them). Run 4 over-trusted them (used them as executable knowledge, no "when to apply" filter). Run 5 followed them too rigidly. The middle path — load the skill, read its applicability sections, then apply judgment — is what the post-run-5 manifest now prescribes.

- **Persistent memory amplifies the identity problem.** aaOS's "unforgeable kernel-generated process IDs" model is correct for a runtime where agents are ephemeral. Persistent learning requires a stable identity somewhere. The `AgentId::from_uuid()` exception for Bootstrap is flagged as such in the code; run 5 validated that it's load-bearing (without it, nothing this run stored would be reachable by run 6). Copilot's earlier caveat — that a separate "system memory identity" distinct from `AgentId` may eventually be cleaner — remains open for future work.

### Cost

Previous estimates in docs had run 5 at ~$0.55 in isolation. Corrected per the DeepSeek dashboard: cumulative spend since the Anthropic→DeepSeek switch is ~$0.54 across **all** runs that used DeepSeek, not per-run. DeepSeek's context caching means most input tokens on persistent Bootstrap iterations are cache hits at ~10% price. Token-count × flat-rate math over-estimates by a large margin. Rough cumulative across all five rounds: ~$0.70.
