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

---

## Phase C: Agent Memory System

Built in a single Claude session via subagent-driven development. Design brainstorming, three spec documents (C1, C2, C3) each peer-reviewed by Qwen CLI and Copilot CLI, two implementation plans each peer-reviewed, then execution via fresh subagent per task.

### What Was Built

Two of three sub-projects implemented; the third deferred by design.

**C1: Managed context windows.** `ContextManager` sits between the persistent agent loop and the executor, transparently summarizing old messages when the context fills. `TokenBudget` parses human-readable sizes ("128k"), estimates context with a chars/4 heuristic, and triggers summarization at a configurable threshold. Summary messages fold into the system prompt prefix (not as User messages — that would break API turn alternation). Tool call/result pairs are kept atomic during selection. Archives preserve raw messages on disk; TTL-based pruning prevents unbounded growth. Fallback to hard truncation on LLM failure.

**C2: Episodic memory store.** New `aaos-memory` crate (7th workspace member). `MemoryStore` trait with `InMemoryMemoryStore` — cosine similarity search, agent isolation, LRU cap eviction, replaces/update semantics, dimension mismatch handling. `EmbeddingSource` trait with `MockEmbeddingSource` (testing) and `OllamaEmbeddingSource` (production — nomic-embed-text via local Ollama, 768 dims). Three new tools: `memory_store`, `memory_query`, `memory_delete`.

**C3: Shared knowledge graph.** Design direction documented, explicitly deferred. Peer reviewers (Qwen + Copilot) confirmed deferral: "building shared infrastructure without proven demand is a classic trap." Seven prerequisites listed before activation.

The codebase grew from ~8000 to ~12000 lines, 141 tests to 205. 4 live API tests verified end-to-end with real Haiku 4.5 and Ollama.

### What the AI Got Wrong

- **Circular dependency in the plan.** The C1 plan put `estimate_tokens(&[Message])` on `TokenBudget` in `aaos-core`, but `aaos-llm` already depends on `aaos-core` — circular. Both Qwen and Copilot caught this independently. Fixed by moving message-aware functions to `aaos-runtime::context`.

- **Prepared context never passed to executor.** The C1 plan called `prepare_context()` after the executor ran, meaning summarization output was discarded for the LLM call. Both reviewers flagged this as the #1 blocker. Fixed by adding `run_with_history_and_prompt()` and restructuring the persistent loop to call prepare_context BEFORE the executor.

- **Model name mismatch in server.** `Server::new()` created `InMemoryMemoryStore` expecting model `"nomic-embed-text"` but `MockEmbeddingSource` reports `"mock-embed"`. All query results were silently dropped due to dimension mismatch filtering. Caught during integration test implementation.

- **LanceDB API instability.** Both reviewers flagged that `lancedb` Rust crate has unstable APIs. Copilot recommended skipping it entirely in favor of SQLite+sqlite-vec. Decision: ship with InMemoryMemoryStore, use Ollama for real embeddings, defer persistent vector store.

### What Required Human Judgment

- **"Ask Qwen for a review. Then ask Copilot for what Qwen proposes."** The human established the multi-model peer review pattern that caught the three biggest bugs in the plans before any code was written.

- **"The only thing that matters is to see visually if it works as designed."** The human clarified that engineering correctness was delegated to the AI + peer review; the human's validation criterion was observing the system work end-to-end.

- **"So it should be also A+B+C1."** The human insisted on a cumulative E2E test covering all phases together, not just isolated C1 testing. This is the same pattern as Phase B where the human demanded Phase A + Phase B E2E verification.

- **"Ask Copilot for review"** on vector store alternatives. The human routed architecture decisions through external review, leading to the SQLite+sqlite-vec recommendation and the Ollama embedding source decision.

### The Pattern (Evolved Further)

Phase C added two new dimensions to the build process:

**Multi-model peer review.** Design specs and implementation plans were reviewed by both Qwen and Copilot before implementation began. This caught 3 blocker-level bugs, 5 high-severity issues, and numerous medium/minor issues — all before a single line of implementation code was written. The cost of these reviews was ~$0.02 each; the cost of debugging the circular dependency or the "prepared context never used" bug at runtime would have been orders of magnitude higher.

**Alternative evaluation through external review.** Architecture decisions (vector store, embedding source) were presented as option tables and routed through Copilot for independent assessment. This produced concrete recommendations (skip LanceDB, use Ollama) with reasoning that the primary AI would not have generated alone.

The overall pattern: **design → multi-model peer review → fix before implementation → subagent-driven execution → cumulative E2E verification**. Each phase has made the feedback loop tighter and caught bugs earlier.

---

## Phase D: Self-Bootstrapping Agent VM

Built in a single session. The roadmap originally called for a web dashboard (Phase D) and inference scheduling (Phase E) next. A conversation about what would actually prove the vision led to a different direction: skip the dashboard, build the autonomous demo.

### What Was Built

A Docker container where `agentd` is PID 1 and a Bootstrap Agent (Sonnet) self-organizes agent swarms. Three OS-level features: persistent Bootstrap loop with Unix socket goal queue, workspace isolation per goal, and automatic retry of failed child agents. Plus safety guardrails: agent count limit (100), spawn depth limit (5), `StdoutAuditLog` for observability.

First successful run: goal "fetch HN and summarize the top 5 stories" → Bootstrap spawned a Fetcher agent (Haiku) that called `web_fetch`, spawned a Writer agent (Haiku) that called `file_write`, and produced `/output/summary.txt`. The capability system worked — Bootstrap correctly couldn't read `/output/*` even though its child wrote there. ~75 seconds, ~$0.03.

Second test: sent a second goal to the running container via Unix socket. Bootstrap processed it as a persistent agent — spawned new children, used workspace isolation (`/data/workspace/rust-reddit/`), wrote output. The container stayed alive between goals.

### What the AI Got Wrong

- **Copilot couldn't read `spawn_tool.rs`** during the feasibility review due to file permission errors, and concluded `spawn_agent` tool and parent⊆child capability enforcement didn't exist. They did — from Phase A. This wasted some planning time on "gaps" that were already filled. Lesson: external reviewers are only as good as the context they can access.

- **Model ID wrong in bootstrap manifest.** Used `claude-sonnet-4-6-20250725` (a dated ID) instead of `claude-sonnet-4-6`. Also, the `SUPPORTED_MODELS` allowlist in `AnthropicClient` was stale. Two container launches failed before this was caught.

### What Required Human Judgment

- **"Let the AI build all itself on boot."** The human's vision for Phase D. The AI (and Copilot) suggested safer, smaller demos ("fetch 3 URLs"). The human pushed for the maximally ambitious version — a self-organizing system from a single Bootstrap Agent. This shaped the entire phase.

- **"Is that not drifting away?"** When Copilot recommended `shell_exec` and a CI demo, the human recognized this would turn aaOS into a developer tool, not an OS. The human separated OS-level work (goal queue, workspace isolation, supervisor) from application-level work (shell_exec, CI). This kept the project on vision.

- **"The engineering level is way past my knowledge — I can think in systems."** The human acknowledged the boundary of their expertise and trusted the AI + peer review for engineering, while providing systems-level direction. This division — human provides vision and pushes back on drift, AI provides engineering — is the same pattern from Phase A, evolved further.

### The Pattern (Final Form)

Phase D demonstrated the mature build process:

1. **Vision from human** — "the OS isn't for humans anymore, let the AI build itself on boot"
2. **Feasibility review via external model** (Copilot) — identified real gaps and false gaps
3. **Human filters the review** — caught that some "missing" features already existed, rejected application-specific suggestions
4. **Rapid implementation** — 3 features, ~200 lines total, subagent-driven
5. **Live demo validates the vision** — Docker container, real API calls, real output file, zero human intervention inside the container

The total implementation was ~330 lines of new code. Most of the session time was spent on vision, direction, and peer review — not coding. The code was the easy part. The hard part was deciding what to build.

---

## Phase E (Partial): Multi-Provider LLM Support & Self-Designing Demo

Built in a single session. Two things happened: an `OpenAiCompatibleClient` was implemented so aaOS can talk to any OpenAI-compatible API (DeepSeek, etc.), and then the system was used to design its own next phase — autonomously.

### What Was Built

**OpenAI-compatible LLM client.** New `openai_compat.rs` in `aaos-llm` crate. Implements `LlmClient` trait for any provider that speaks the OpenAI Chat Completions format. Request translation (system-as-first-message, tool_calls as function format, role:"tool" for results), response translation (choices[0].message, finish_reason mapping, prompt_tokens/completion_tokens). `OpenAiCompatConfig::deepseek_from_env()` constructor. 15 unit tests. The daemon checks `DEEPSEEK_API_KEY` first, falls back to `ANTHROPIC_API_KEY`.

**Self-designing demo.** Bootstrap Agent (DeepSeek Reasoner) received: "Read the aaOS architecture and roadmap, then design Phase E: Inference Scheduling." It spawned a Fetcher (DeepSeek Chat) to pull the docs from GitHub, an Analyzer to extract architectural facts into episodic memory (5 facts stored via `memory_store`), then wrote three files itself: `phase-e-spec.md` (9.7KB), `phase-e-plan.md` (13KB), `phase-e-review.md` (12.5KB). Total: 14 iterations, ~$0.02, ~10 minutes.

### What the AI Got Wrong

- **`file_read` before docs existed.** Bootstrap tried to read `/data/workspace/architecture.md` before the Fetcher wrote it. Recovered by spawning the Fetcher first.

- **`spawn_agent` failed once.** Bootstrap tried to spawn a child with a name not in the `spawn_child` allowlist. Recovered by writing the file itself instead.

- **The generated Rust types don't compile.** GPT-5.4 peer review caught 5 hard errors: wrong `LlmClient` method signatures (used `chat()` not `complete()`), `&mut self` in `Arc`-wrapped struct, `AtomicU32` in `#[derive(Clone)]`, invalid enum variant syntax, `Instant` with serde. The agent wrote Rust-flavored pseudocode, not real Rust.

- **Ignored existing codebase patterns.** Duplicated the `Priority` enum instead of extending the existing one. Created a new `InferenceScheduler` that ignores the existing `Scheduler` trait. Proposed capability extensions without showing changes to the actual `Capability` enum.

- **Designed for local models when the user wants API-only.** The spec was heavily oriented toward Ollama/vLLM/GPU scheduling. The human's actual requirement is cheap API inference (DeepSeek), not local models. The agent had no way to know this — it designed from the roadmap, which mentions local models. This is a context limitation, not an intelligence failure.

### What Required Human Judgment

- **"The goal should be to build the OS itself, not URL fetching."** The human redirected from demo goals to self-referential ones. The AI would have kept doing fetch-and-summarize demos indefinitely.

- **"I don't agree with Ollama — I don't want anything local."** After the spec was generated and peer-reviewed, the human rejected the entire local-model premise. The OS designed Phase E correctly according to the roadmap, but the roadmap's Phase E assumed local models, and the human's actual need had evolved. Lesson: generated designs are only as good as the requirements they're given.

- **Peer review via Copilot/GPT-5.4.** The human insisted on external review, which caught every compile error and several architectural conflicts the self-review missed. The agent's own review was thoughtful but surface-level — it identified risks conceptually without checking whether the Rust code would actually compile against the existing codebase.

### The Pattern (Meta)

Phase E demonstrated something new: **the system designing itself**. Previous phases had the human directing and the AI implementing. This time the AI (inside the container) did the design work, and the human + external reviewer evaluated it. The output was credible in structure and vision but wrong in details — exactly the kind of work that benefits from peer review.

The cost model proved out: $0.02 for a complete spec/plan/review cycle. DeepSeek Reasoner for orchestration + DeepSeek Chat for workers is ~15x cheaper than Anthropic, with no rate limits. The `OpenAiCompatibleClient` makes this provider-agnostic — any OpenAI-compatible API works.

Key insight: **AI-generated specs need the same review rigor as AI-generated code.** The agent's self-review caught conceptual risks but missed compile errors. External peer review (GPT-5.4) caught the compile errors but needed access to the actual codebase to do so. The combination — self-review + peer review with codebase access — caught everything.

---

## Security Self-Audit

The system audited its own security. Bootstrap spawned a code-reader (464K tokens of source) and a security-auditor (474K tokens). Total: 1.37M tokens, ~$0.05.

### What the Agents Found

13 findings across 6 components. Of these, 4 were confirmed real and fixed:

1. **Path traversal in `glob_matches`** (CRITICAL) — `"/data/../etc/passwd".starts_with("/data/")` returns true. An agent with `file_read: /data/*` could read any file via `..` sequences. Fixed by adding lexical path normalization before matching.

2. **Unknown tools receive all capability tokens** (MEDIUM) — `matches_tool_capability` returned `true` for unknown tools, leaking `FileRead`/`FileWrite`/`SpawnChild` tokens. Fixed to only pass `ToolInvoke` tokens to unknown tools.

3. **Child tokens ignore parent constraints** (HIGH) — `CapabilityToken::issue()` used `Constraints::default()` for child tokens. A parent with rate limits could spawn children without those limits. Fixed to inherit the granting parent's constraints.

4. **No path canonicalization in file tools** (CRITICAL) — Same root cause as #1, at the tool level. Fixed by the same `normalize_path()` function.

### What the Agents Got Wrong

- **V6.1 "Capability checker injection"** — Described the router accepting a closure as a vulnerability. The closure is constructed by the server, not by agents. Not exploitable.
- **CVSS scores inflated** — Assigned network attack vectors (AV:N) to a system running in Docker with no network listener. The actual attack surface is agent-to-kernel, not network.
- **V2.1 overstated** — The parent-subset enforcement was correct; the real risk was path traversal in the glob matcher it delegates to, which was V1.1.

### What Required Human Judgment

- **Verifying findings against actual code.** The agents reported 13 findings. A human (Claude) read the actual source for each, confirmed 4, identified 4 as overstated or incorrect, and deferred 5 as low-risk in Docker. The agents couldn't distinguish "architecturally concerning" from "actually exploitable."

- **Prioritization.** The agents assigned equal urgency to everything. The human recognized that path traversal (#1) was the only finding that could escape the capability sandbox entirely — the rest were defense-in-depth issues.

### The Pattern

The OS found real bugs in itself. The path traversal vulnerability was present since Phase A and would have allowed any agent to read any file on the system by using `..` sequences. No human had caught it in 4 phases of development. A $0.05 security audit by the system's own agents found it.

The self-audit pattern: **code-reader (deep source analysis) → security-auditor (adversarial review) → Bootstrap (consolidation) → human verification → fix**. The agents do the exhaustive reading; the human does the judgment.
