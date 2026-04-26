# Build Retrospective

This document is the chronological build history of aaOS, phase by phase, verified against git commit timestamps and content. It stops at the end of Phase E + the AgentSkills integration — everything that constitutes "the runtime is feature-complete for its current goal."

Self-reflection runs (the system reading its own code, finding bugs, proposing features) are captured separately in [`reflection/`](reflection/README.md) as an ongoing log. Cross-cutting lessons distilled from both the build history and the reflection log live in [`patterns.md`](patterns.md).

---

## What aaOS Is (Short Version)

aaOS is an agent runtime — a program that manages AI agents as first-class processes, with capability-based security replacing traditional permissions. It ships as a daemon (`agentd`) running inside Docker on Linux; the long-term plan is to migrate to a real capability-based microkernel (Redox OS or seL4). See `README.md` for the product pitch and `docs/architecture.md` for the layered design.

## Glossary (Terms Used Throughout)

- **Claude session** — a single conversation with Anthropic's Claude model, driven by a human operator through a CLI. Most phases used two sessions running in parallel: one architecting, one implementing.
- **Subagent** — a fresh, isolated Claude session dispatched from an orchestrating session to do one task. Subagents start with an empty context window, receive a spec, produce artifacts, and exit. They are a build-time workflow pattern, not a runtime concept.
- **Runtime agent** — an autonomous process inside aaOS (a running `agentd` container). Runtime agents have capability tokens, message channels, and an LLM backing them. The Bootstrap Agent and its children are runtime agents.
- **Bootstrap Agent** — the single runtime agent that starts when `agentd` boots in bootstrap mode. It receives high-level goals over a Unix socket, decomposes them, spawns child runtime agents with narrower capabilities, and coordinates their work. Introduced in Phase D. Powered by Claude Sonnet in Phase D; switched to DeepSeek Reasoner after Phase E1 added OpenAI-compatible API support.
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
| E1+ | DeepSeek Reasoner | DeepSeek Chat | `OpenAiCompatibleClient` unlocked cheaper providers |

Anthropic remains supported as a fallback. The daemon checks `DEEPSEEK_API_KEY` first, falls back to `ANTHROPIC_API_KEY`.

## Code Growth Over Time

Numbers verified from git as of the landmark commits listed. Two counts shown: **prod** = non-test code (what earlier docs called "~4,000 lines" and similar), **total** = prod + test code in the same files. The original retrospective's headline numbers (4K → 8K → 12K → 13K) were production-only counts; this table records both so the methodology is legible.

| Stage | Landmark commit | Crates | Prod lines | Total lines | `#[test]` markers |
|-------|-----------------|--------|------------|-------------|-------------------|
| A (runtime prototype)     | 029d90b | 6 | 3,917  | 6,151  | 111 |
| B (persistent agents)     | 464d8fc | 6 | 5,160  | 8,042  | 141 |
| C (agent memory)          | 8d2efba | 7 | 7,630  | 11,693 | 206 |
| D (self-bootstrapping)    | db641dd | 7 | 7,926  | 12,041 | 210 |
| April 13 end (E + skills + revocation + constraints) | 66542bf | 7 | 9,451 | 14,458 | 258 |
| Current (after run 5 response) | 58930f0 | 7 | 9,675 | 14,797 | 264 |

Commit timestamps: Phase A released 2026-03-21 17:13 (first commit). Everything from Phase B through the end of April-13-end is a single calendar day: 2026-04-13 10:47 → 22:37 (~11h 50m of activity). Subsequent tuning happened on 2026-04-14.

---

## Phase A: 48 Hours

The original design brief estimated 3–5 people and 3 months to reach a working demo. What actually happened: one human operator, two Claude sessions, ~48 hours of concentrated work, landing as commit `029d90b` on 2026-03-21 at 17:13.

The "48 hours" figure is the user's recalled wall-clock span (design through working prototype). Git only records the landing commit; the 48-hour count is load-bearing context, not a timestamp assertion.

### What Was Built

A working agent runtime prototype: 6 Rust crates, 3,917 production lines (6,151 total with tests), 111 tests. Agent kernel with capability-based security, tool registry with two-level enforcement (tool access check + resource path check), LLM execution loop, agent orchestration with capability narrowing, human-in-the-loop approval queue, and MCP message routing. Verified end-to-end against the real Anthropic API.

The **capability system** shipped in Phase A and has anchored every later phase. An agent declares capabilities in its YAML manifest (e.g., `file_read: /data/*`). At spawn time, the runtime issues unforgeable tokens. Every tool invocation is checked against the caller's tokens before execution. Child agents inherit only a subset of what the parent holds. This is the foundation that makes later features — `spawn_agent`, workspace isolation, skill execution — secure.

Phase A also introduced `spawn_tool` (the built-in tool that lets an agent create child agents) and the `parent⊆child` capability enforcement. Both became load-bearing in Phase D.

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

---

## Phase B: Persistent Agents & Request-Response IPC

Phase B commits: `f263e2b` (design spec, 2026-04-13 10:47) → `464d8fc` (Phase A+B E2E test, 2026-04-13 11:39). **52 minutes of landed commits.** A single orchestrating Claude session produced the spec and plan, then dispatched implementation tasks to fresh subagents.

At the end of Phase B: 6 crates, 5,160 production lines (8,042 total), 141 tests.

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

- **"Did you implement all 3 sub-specs?"** The human asked for explicit verification against the design spec before accepting the work as done.
- **End-to-end coverage.** The orchestrating session tested Phase B in isolation. The human recognized that Phase A + Phase B integration hadn't been verified and asked for a combined test. This surfaced the "persistent loop never started" bug.
- **Reusing the live-API test harness.** The human pointed back at the test-with-real-Anthropic-API pattern from an earlier project, ensuring the same verification standard applied.
- **Multi-model peer review before implementation.** The human insisted on external review of the Phase B design before any code was written. Both Qwen CLI and Copilot CLI (GPT-5.4) caught real issues: don't embed oneshot in McpMessage, load history once not per-message, executor must return transcript delta.

Phase B introduced **subagent-driven development** as a formal pattern: each of ~10 tasks dispatched to a fresh subagent, results verified, orchestrator's context kept clean. Also introduced **multi-model peer review** of design specs before any implementation.

---

## Phase C: Agent Memory System

Phase C commits: `054548f` (design specs, 2026-04-13 12:49) → `8d2efba` (E2E tests, 2026-04-13 15:34) with documentation wrap-up at `3047e0c`/`5820130` by 16:19. **~3h 30m of commits.** Single orchestrating session via subagent-driven development. Three spec documents (C1, C2, C3), each peer-reviewed by Qwen CLI and Copilot CLI. Two of three sub-projects shipped; the third deferred by design.

At the end of Phase C: 7 crates (added `aaos-memory`), 7,630 production lines (11,693 total), 206 tests.

### What Was Built

**C1: Managed context windows.** `ContextManager` sits between the persistent agent loop and the executor, transparently summarizing old messages when the context fills. `TokenBudget` parses human-readable sizes ("128k"), estimates context with a chars/4 heuristic, and triggers summarization at a configurable threshold (default 70%). Summary messages fold into the system prompt prefix (not as User messages — that would break API turn alternation). Tool call/result pairs are kept atomic during selection. Archives preserve raw messages on disk; TTL-based pruning prevents unbounded growth. Fallback to hard truncation on LLM failure.

**C2: Episodic memory store.** New `aaos-memory` crate. `MemoryStore` trait with `InMemoryMemoryStore` — cosine similarity search, agent isolation, LRU cap eviction, replaces/update semantics, dimension mismatch handling. `EmbeddingSource` trait with `MockEmbeddingSource` (testing) and `OllamaEmbeddingSource` (production — nomic-embed-text via local Ollama, 768 dims). Three new tools: `memory_store`, `memory_query`, `memory_delete`.

**C3: Shared knowledge graph.** Cross-agent knowledge sharing, designed but deferred. Both peer reviewers confirmed the deferral: "building shared infrastructure without proven demand is a classic trap."

A `SqliteMemoryStore` was added later (commit `b8b7af4`, 2026-04-13 21:11) for persistence across container restarts — listed here because it's part of the memory system's natural evolution, though it landed after Phase D.

### What the Model Got Wrong

- **Circular dependency in the plan.** The C1 plan put `estimate_tokens(&[Message])` on `TokenBudget` in `aaos-core`, but `aaos-llm` already depends on `aaos-core` — circular. Both Qwen and Copilot caught this independently. Fixed by moving message-aware functions to `aaos-runtime::context`.
- **Prepared context never passed to executor.** The C1 plan called `prepare_context()` *after* the executor ran, meaning summarization output was discarded for the LLM call. Both reviewers flagged this as the #1 blocker. Fixed by adding `run_with_history_and_prompt()` and restructuring the persistent loop to call `prepare_context` *before* the executor.
- **Model name mismatch dropped all query results.** The in-memory store filters retrieved memories by embedding dimension. `Server::new()` created the store expecting model `"nomic-embed-text"` (768 dims) but wired up `MockEmbeddingSource`, which reports model `"mock-embed"` at different dimensions. The filter silently discarded every result. Caught during integration test implementation.
- **LanceDB API instability.** Both reviewers flagged that the `lancedb` Rust crate has unstable APIs. Copilot recommended skipping it entirely in favor of SQLite+sqlite-vec. Decision: ship with `InMemoryMemoryStore`, use Ollama for real embeddings, defer the persistent vector store.

### What Required Human Judgment

- **"Ask Qwen for a review. Then ask Copilot for what Qwen proposes."** The human established the multi-model peer review pattern that caught the three biggest bugs in the plans before any code was written.
- **"The only thing that matters is to see visually if it works as designed."** The human clarified that engineering correctness was delegated to the model + peer review; the human's validation criterion was observing the system work end-to-end.
- **Cumulative E2E testing.** The human insisted on a Phase A + B + C1 test, not just isolated C1 — same pattern as Phase B.

Phase C added **multi-model peer review of design specs** as a first-class step. Routing architecture decisions through Copilot produced concrete recommendations (skip LanceDB, use Ollama) that the primary Claude session would not have generated alone. Cost per review was recorded at the time as "~$0.02" — token-math estimate, not dashboard-verified. (Per-run cost tracking was discontinued 2026-04-25; rough cumulative across all runs through that date was under $1.)

---

## Phase D: Self-Bootstrapping Agent VM

Phase D commits: `87ff99a` (agent count limits, 2026-04-13 16:43) → `db641dd` (doc updates, 17:18). **~35 minutes of commits.** The roadmap originally had Phase D as a web dashboard. After reviewing Phase C results, the human redirected to "an agent runtime that boots and runs itself autonomously."

At the end of Phase D: 7 crates, 7,926 production lines (12,041 total), 210 tests.

### What Was Built

A Docker container where `agentd` runs as the main process (PID 1 — the container's init, which means when `agentd` exits, the container exits). At startup, `agentd` spawns the **Bootstrap Agent** — the first and only runtime agent at boot, powered by Claude Sonnet. The Bootstrap Agent accepts goals over a Unix socket, decomposes them, and spawns child runtime agents (Haiku) with narrowed capabilities to execute them.

Three OS-level features landed this phase:

- **Persistent Bootstrap loop with Unix socket goal queue.** Additional goals can be sent to the running container without restarting it.
- **Workspace isolation per goal.** Each goal gets `/data/workspace/{name}/`. Child agents write intermediate files there.
- **Automatic retry of failed child agents.**

Plus safety guardrails: agent count limit (100), spawn depth limit (5), `StdoutAuditLog` for observability via `docker logs -f`.

First successful run: goal "fetch HN and summarize the top 5 stories" → Bootstrap Agent spawned a Fetcher agent (Haiku) that called `web_fetch`, spawned a Writer agent (Haiku) that called `file_write`, and produced `/output/summary.txt`. The capability system worked as designed — the Bootstrap Agent couldn't read `/output/*` even though its child wrote there. Wall time ~75 seconds. Cost was recorded at the time as ~$0.03 against Anthropic — token-math estimate, not dashboard-verified.

### What the Model Got Wrong

- **Copilot review missed existing features.** Copilot couldn't read `spawn_tool.rs` during the feasibility review due to file permission errors, and concluded the spawn_agent tool and parent⊆child capability enforcement didn't exist. They did — both shipped in Phase A. This wasted planning time on "gaps" that were already filled. Lesson: external reviewers are only as good as the context they can access.
- **Model ID wrong in Bootstrap manifest.** Used `claude-sonnet-4-6-20250725` (a dated variant) instead of `claude-sonnet-4-6`. Two container launches failed before this was caught.

### What Required Human Judgment

- **"Let aaOS build itself on boot."** The human's vision for Phase D. The Claude sessions (and Copilot's review) suggested safer, smaller demos ("fetch 3 URLs"). The human pushed for the maximally ambitious version — a self-organizing system from a single Bootstrap Agent.
- **"Is that not drifting away?"** When Copilot recommended `shell_exec` and a CI demo, the human recognized this would turn aaOS into a developer tool, not a runtime. The human separated OS-level work (goal queue, workspace isolation, supervisor) from application-level work (shell_exec, CI).

Total new code in Phase D: ~330 lines. Most of the session time was spent on vision, direction, and peer review — not coding.

---

## Phase E: Multi-Provider LLM + Inference Scheduling + Budgets

Phase E commits: `f6b62a6` (E1 OpenAI-compat, 2026-04-13 19:40) → `2c74c4e` (launcher fix, 20:29) for E1+E2+E3; budget enforcement E3 was `d76f16c` at 20:29. **~50 minutes of commits for E1-E3.** The security-audit cleanup, SQLite memory, and AgentSkills integration followed immediately after (20:48 → 21:47).

At the end of April 13 (after Phase E + skills + revocation + constraints + Bootstrap-uses-skills fix): 7 crates, 9,451 production lines (14,458 total), 258 tests.

### E1: OpenAI-Compatible LLM Client (commit `f6b62a6`)

**Why this existed.** Running autonomous agents against Anthropic's API was tolerable for demos but expensive for long-running fleets. DeepSeek's reasoner is cheaper, and offers context caching that further discounts cache-hit input tokens to roughly 10% of the normal rate (a detail that broke naive token-math cost estimates in earlier versions of this retrospective — token counts × flat-rate over-reports by 5–10× under cache hits). The roadmap had Phase E as "inference scheduling" with an implicit assumption of local models (Ollama, vLLM); the human rejected the local-model premise in favor of cheap cloud APIs.

**What was built.** `OpenAiCompatibleClient` in `aaos-llm` implements the `LlmClient` trait for any provider that speaks the OpenAI Chat Completions format. Request translation (system-as-first-message, tool_calls as function format, `role:"tool"` for results). Response translation (`choices[0].message`, finish_reason mapping, prompt_tokens/completion_tokens). `OpenAiCompatConfig::deepseek_from_env()` constructor. 15 unit tests. The daemon checks `DEEPSEEK_API_KEY` first, falls back to `ANTHROPIC_API_KEY`.

Bootstrap switched from Claude Sonnet to DeepSeek Reasoner. Worker children switched from Haiku to DeepSeek Chat. The rest of the runtime didn't change — the `LlmClient` trait was the clean abstraction Phase A had built.

**Self-designing demo.** With the new client online, the human gave Bootstrap the goal: "Read the aaOS architecture and roadmap, then design the next phase of inference scheduling." Bootstrap produced three spec files autonomously. That demo is documented in full in [`reflection/`](reflection/README.md) under the rubric of earlier self-designing runs.

### E2: Inference Scheduling (commit `1739b34`)

`ScheduledLlmClient` decorator wraps any `LlmClient` with a `tokio::sync::Semaphore` (default max 3 concurrent API calls). Optional rate smoothing via a configurable minimum delay between calls. Env vars: `AAOS_MAX_CONCURRENT_INFERENCE`, `AAOS_MIN_INFERENCE_DELAY_MS`. Both Bootstrap and normal daemon modes use the scheduler. 4 new tests.

### E3: Budget Enforcement (commit `d76f16c`)

Per-agent token budgets declared in manifests (`budget_config: { max_tokens, reset_period_seconds }`). `BudgetTracker` uses atomic CAS operations for lock-free tracking. Wired into `InProcessAgentServices::report_usage()` — agents exceeding their budget get `BudgetExceeded` errors. Optional: agents without `budget_config` have no enforcement. 5 new tests.

### Security Self-Audit and Follow-On Fixes

Commit `82d19e9` (2026-04-13 20:52) — "security: fix 4 vulnerabilities found by self-audit" — marks the integration point for the security self-audit. The audit itself was a runtime agent run; the details of what the runtime found and how it was judged are in [`reflection/2026-04-13-run-1-security-self-audit.md`](reflection/2026-04-13-run-1-security-self-audit.md). The four vulnerabilities fixed in-code were: path traversal in `glob_matches`, unknown tools receiving all capability tokens, child tokens ignoring parent constraints, and no path canonicalization in file tools.

### AgentSkills Integration

Commits `5a0a42e` (skill loader) → `5c023fd` (bundle 21 skills) → `7bdb1bb` (doc updates), 2026-04-13 21:28 → 21:40. **~12 minutes of commits.** `aaos-core::skill` parses SKILL.md files per the [AgentSkills](https://agentskills.io) specification. `SkillRegistry` manages loaded skills. `skill_read` tool serves full instructions and reference files with path traversal protection. Skill catalog injected into agent system prompts at spawn time (progressive disclosure tier 1). 21 production-grade skills bundled from [addyosmani/agent-skills](https://github.com/addyosmani/agent-skills).

The "named agents after skills but never actually loaded them" bug — discovered in a self-reflection run later that same evening — was fixed in `66542bf` (2026-04-13 22:37) by updating the Bootstrap manifest with explicit `skill_read` instructions.

### Capability Revocation and Constraint Enforcement

Commits `f1732d9` (revocation, 22:07) and `f106d97` (max_invocations enforcement, 22:34). Both landed as integrations of findings from self-reflection runs. See [`reflection/`](reflection/README.md) for the run-level detail of how those findings came about.

---

## The Shape of the Build

Phases B through E, plus security self-audit, SQLite memory, AgentSkills integration, revocation, constraint enforcement, and the Bootstrap-uses-skills fix **all happened on a single calendar day** (2026-04-13 10:47 → 22:37). That is not "48 hours like Phase A" or "a few hours per phase" — it is one long sustained day of design → implement → review → ship cycles, with multiple self-reflection runs woven through.

The build process itself went through phases:

- **Phase A:** Continuous design–build–validate loop across two Claude sessions.
- **Phase B:** Added subagent-driven development and multi-model peer review.
- **Phase C:** Formalized the multi-model peer review step with alternative-evaluation prompts.
- **Phase D:** Shifted from "human specifies, model implements" to "human casts vision, model + reviewers converge on scope." Peer review filtered by human judgment against the vision.
- **Phase E and after:** The runtime started reading its own code. Self-reflection runs begin — documented in [`reflection/`](reflection/README.md).

The remaining chronicle — Runs 1 through 5, what they found, what shipped, what we corrected about our own cost estimates along the way — lives in [`reflection/`](reflection/README.md). Cross-cutting lessons that apply beyond any single run or phase are in [`patterns.md`](patterns.md).
