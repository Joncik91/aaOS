# Roadmap

The prototype demonstrates that agent-first OS abstractions work: capability-based security, structured IPC, tool execution with two-level enforcement, agent orchestration with capability narrowing, and human-in-the-loop approval. Everything below builds on this foundation.

## Phase B: Persistent Agents & Request-Response IPC *(complete)*

Persistent agents run continuously in a tokio background task, processing messages sequentially from a channel. Request-response IPC uses a `DashMap<Uuid, oneshot::Sender>` pending-response map on the router. Conversation history persists in JSONL files via a `SessionStore` trait, loaded once at startup and appended after each turn.

**What was built:** `persistent_agent_loop()`, `start_persistent_loop()` on registry, `send_and_wait()` on `AgentServices`, `SessionStore` trait + `JsonlSessionStore`, `run_with_history()` on `AgentExecutor` with transcript delta, `max_history_messages` config, Pause/Resume/Stop commands, 3 new audit events, `MailboxFull`/`Timeout` error variants. 141 tests (30 new), verified end-to-end with real Haiku 4.5 API.

**What this enables:** Agents that remember context across interactions. Multi-agent workflows where peers communicate directly via `send_and_wait`. The foundation for the NarrativeEngine orchestration layer.

## Phase C: Agent Memory System *(C1+C2 complete, C3 deferred)*

**C1: Managed context windows.** *(complete)* The runtime manages what's in the agent's context window. When the conversation grows too long, `ContextManager` summarizes older messages via an LLM call and archives the originals to disk. The agent sees a coherent conversation; the runtime handles the compression transparently. `TokenBudget` estimates context size using a chars/4 heuristic, triggering summarization at a configurable threshold (default 70%). Summary messages are folded into the system prompt prefix, preserving User/Assistant turn alternation. Tool call/result pairs are kept atomic during summarization. Fallback to hard truncation on LLM failure.

**What was built (C1):** `TokenBudget` type with `from_config()`, `ContextManager` with `prepare_context()`, `Message::Summary` variant, `ArchiveSegment` + archive methods on `SessionStore` trait, `LlmClient::max_context_tokens()`, `run_with_history_and_prompt()` on `AgentExecutor`, 2 new audit events. 25 new tests (166 total). Verified end-to-end with real Haiku 4.5 — summarization preserves facts across compression boundaries.

**C2: Episodic store.** *(complete)* Per-agent persistent memory via explicit `memory_store`, `memory_query`, and `memory_delete` tools. Agents store facts, observations, decisions, and preferences. Later, they query by meaning via cosine similarity over embeddings. In-memory store with brute-force search (SQLite+sqlite-vec planned for persistence). Embeddings via Ollama's nomic-embed-text model (768 dims, OpenAI-compatible `/v1/embeddings` endpoint).

**What was built (C2):** New `aaos-memory` crate (7th workspace member) with `MemoryStore` trait, `InMemoryMemoryStore` (cosine similarity, agent isolation, LRU cap eviction, replaces/update semantics, dimension mismatch handling), `EmbeddingSource` trait with `MockEmbeddingSource` and `OllamaEmbeddingSource`. Three new tools in `aaos-tools`. `MemoryConfig` with episodic fields. 2 new audit events. 39 new tests (205 total). Verified end-to-end with real Haiku + Ollama nomic-embed-text.

**C3: Shared knowledge graph.** *(deferred)* Cross-agent knowledge sharing. Design direction documented but not buildable — requires C1+C2 production usage, cross-agent capability model, proven multi-agent need. See `docs/phase-c3-design.md` (local only).

**What this enables:** Agents that learn from experience. A persistent agent that remembers facts across summarization boundaries. Agents that explicitly store and retrieve knowledge by meaning. The foundation for shared intelligence (C3) when multi-agent patterns prove the need.

## Phase D: Self-Bootstrapping Agent VM *(complete)*

A Docker container where `agentd` is PID 1 and a Bootstrap Agent autonomously builds agent swarms to accomplish goals.

**What was built:** Bootstrap Agent manifest (Sonnet) with few-shot child manifest examples, persistent goal queue via Unix socket, workspace isolation per goal (`/data/workspace/{name}/`), spawn depth limit (5), global agent count limit (100), parent⊆child capability enforcement (already existed from Phase A), automatic retry of failed child agents, `StdoutAuditLog` for container observability.

**What this proves:** The OS vision works. A container boots, receives a goal ("fetch HN and summarize the top 5 stories"), and the Bootstrap Agent self-organizes: spawns a Fetcher agent with `web_fetch` capability, spawns a Writer agent with `file_write:/output/*`, coordinates their work, and produces a real output file. The capability system enforces isolation — the Bootstrap Agent correctly cannot read `/output/*` even though its child wrote there. Total time ~75 seconds, ~$0.03. The container stays alive accepting additional goals via the socket.

**What this enables:** Autonomous agent systems that self-organize for arbitrary goals. The OS manages agent lifecycle, capability enforcement, and observability. Humans provide goals, not instructions.

## Phase E: Multi-Provider LLM Support & Inference Scheduling *(complete)*

**E1: Multi-provider API support.** *(complete)* `OpenAiCompatibleClient` in `aaos-llm` speaks the OpenAI Chat Completions format — works with DeepSeek, OpenRouter, and any OpenAI-compatible provider. The daemon checks `DEEPSEEK_API_KEY` first, falls back to `ANTHROPIC_API_KEY`. Bootstrap uses `deepseek-reasoner` (thinking mode), children use `deepseek-chat`. 15 unit tests. Verified end-to-end: Bootstrap + 3 child agents designed Phase E autonomously for ~$0.02.

**What was built (E1):** `OpenAiCompatConfig::deepseek_from_env()`, request translation (system-as-first-message, tool_calls as function format, role:"tool" for results), response translation (choices[0].message, finish_reason mapping, prompt_tokens/completion_tokens), auth via `Authorization: Bearer`. Manifest model field routes to the correct provider.

**E2: Inference scheduling.** *(complete)* `ScheduledLlmClient` decorator wraps any `LlmClient` with a `tokio::sync::Semaphore` to limit concurrent API calls (default 3). Optional rate smoothing via configurable minimum delay between calls. Both bootstrap and normal daemon modes use the scheduler. 4 new tests.

**What was built (E2):** `ScheduledLlmClient`, `InferenceSchedulingConfig::from_env()`. Env vars: `AAOS_MAX_CONCURRENT_INFERENCE` (default 3), `AAOS_MIN_INFERENCE_DELAY_MS` (default 0).

**E3: Budget enforcement.** *(complete)* Per-agent token budgets declared in the manifest. `BudgetTracker` uses atomic CAS operations for lock-free tracking. Wired into `InProcessAgentServices::report_usage()` — agents exceeding their budget get `BudgetExceeded` errors. Optional — agents without `budget_config` have no enforcement. 5 new tests.

**What was built (E3):** `BudgetConfig` + `BudgetTracker` + `BudgetExceeded` in `aaos-core`, `budget_config: Option<BudgetConfig>` on `AgentManifest`, `budget_tracker: Option<Arc<BudgetTracker>>` on `AgentProcess`, `track_token_usage()` on `AgentRegistry`. The E3 design was produced by aaOS itself — Bootstrap spawned code-reader, budget-tracker-designer, and rust-implementer agents that read 24K tokens of real source code and produced the implementation. GPT-5.4 peer-reviewed the first design, we integrated with compile fixes.

**Also built:** `run-aaos.sh` launcher with auto-launching live dashboard. Verbose executor logging (full agent thoughts, tool calls, tool results). Source code mounted read-only at `/src/` so agents can read and understand the codebase.

**What this enables:** Cost-effective agent fleets using cheap API providers. A team of 20 agents where most use DeepSeek Chat ($0.27/M input) and a few use Claude for complex reasoning. Provider selection, scheduling, and budget enforcement as kernel concerns.

## AgentSkills Integration *(complete)*

aaOS now supports the [AgentSkills](https://agentskills.io) open standard by Anthropic. Skills are the universal way to give agents capabilities — used by Claude Code, Copilot CLI, Gemini CLI, Qwen CLI, OpenCode, Goose, and VS Code.

**What was built:** Skill loader (`aaos-core::skill`) parses SKILL.md files per the specification. `SkillRegistry` manages loaded skills. `skill_read` tool serves full instructions and reference files with path traversal protection. Skill catalog injected into agent system prompts at spawn time (progressive disclosure tier 1). 21 production-grade skills bundled from [addyosmani/agent-skills](https://github.com/addyosmani/agent-skills).

**What this enables:** Any AgentSkills-compatible skill works in aaOS — but under capability enforcement that no other runtime provides. The same skill that has open shell access in Claude Code runs under unforgeable capability tokens in aaOS. Skills become the "driver model" for agent capabilities; the runtime provides the security boundary.

## Self-Reflection, Round 4 *(complete)*

Three earlier self-reflection runs each found the next deepest bug in the runtime (path traversal → missing revocation → unenforced constraints). Run 4 was different: with skill_read instructions finally wired into the Bootstrap manifest, the agent loaded `idea-refine` and `planning-and-task-breakdown` as executable knowledge and produced a feature proposal instead of a bug report — a "Meta-Cognitive Coordination Layer" so Bootstrap learns from past tasks.

**Observed during the run:** 12 of 50 `file_read` attempts failed because children were guessing filenames or calling `file_read` on directories. Not a capability bug — a missing primitive.

**Shipped (incrementally):**

1. **`file_list` tool** — Directory listing (or file metadata), capability-gated by `FileRead` (same glob, same path normalization). Fixes the path-guessing problem. 5 new unit tests.
2. **Stable Bootstrap ID** — `AgentId::from_uuid()` (kernel-only constructor) + `AgentRegistry::spawn_with_id()` + `AAOS_BOOTSTRAP_ID` env / `/var/lib/aaos/bootstrap_id` file. Makes cross-run memory meaningful for Bootstrap specifically; other agents' IDs remain fresh per-spawn. 1 new test.
3. **Persistent memory, opt-in** — `AAOS_PERSISTENT_MEMORY=1` bind-mounts host memory dir into the container. `AAOS_RESET_MEMORY=1` wipes DB + ID file on boot. Off by default because persistent memory carries prompt-injection / bad-strategy risk.
4. **Memory protocol in manifest** — Bootstrap is now told to `memory_query` before decomposing a goal, `memory_store` a compact run summary after completion, with explicit guidance on what NOT to persist (credentials, user content). Free-form summaries, no structured schema yet.

**Not built (deliberately deferred):** The proposal's new `aaos-reflection` crate, `CoordinationPattern` schema with `success_rate`/`usage_count`, `PatternStore` trait, and dedicated reflection service — all were judged premature. Two external reviews (Claude Opus 4.6 and GPT-5.4 via Copilot CLI) independently concluded the design was schema-first on zero data, with multiple trait signatures that wouldn't have compiled. The minimal version above is designed as the empirical ground for a future structured system: run 10-20 goals, observe what Bootstrap actually remembers and retrieves, then design `PatternStore` against real patterns if they emerge.

**Cost so far across all four runs:** ~$0.59. Each run produced either a real bug fix or a design artifact.

## Self-Reflection, Round 5 *(complete)*

First run with persistent memory enabled. Same philosophical goal, same model lineup, but this time `AAOS_PERSISTENT_MEMORY=1` so the stable Bootstrap ID and its episodic store would survive for future runs.

**What worked:**

- **Protocol fully exercised.** Bootstrap called `memory_query` *before* decomposing the goal (4 queries), read skills via `skill_read`, then `memory_store` at the end with a goal-level summary under its stable ID. Future runs will actually be able to retrieve that summary.
- **`file_list` in active use.** The tool added after run 4 eliminated the path-guessing failures. Children listed directories before reading.
- **Capability model caught a real mistake in real time.** Bootstrap tried to spawn `pattern-implementer` with `file_write: /src/*` — the parent⊆child enforcement denied it because Bootstrap itself doesn't hold that (and `/src/` is read-only by design). Bootstrap recovered and spawned with correct capabilities.
- **Behavioral-adaptation-layer pivot.** After hitting the `/src/*` denial, one child correctly reasoned: *"Since we cannot modify the Rust codebase directly (read-only /src/), we implement the evolution as a behavioral adaptation layer using existing capabilities."* That's the empirically-driven "do it in prompting first" path the reviews had recommended for the Meta-Cognitive Coordinator.
- **Independent convergence.** Run 5 arrived at the same "Meta-Cognitive Coordinator" direction as run 4, without reading run 4's output (mock embeddings meant cross-run retrieval was effectively off). Two fresh runs landing on the same direction is a real signal.

**What the run exposed (all fixed as manifest-only changes):**

1. **Skill over-adherence.** Bootstrap followed `planning-and-task-breakdown`'s workflow mechanically, ignoring the skill's own "When NOT to use: single-file changes with obvious scope." Runtime roughly doubled vs run 4. Fix: manifest now tells Bootstrap to honor each skill's "When to use / When NOT to use" sections and skip the planning dance for simple goals.
2. **Child memory writes are orphaned.** 7 of 14 stored memories ended up under ephemeral child `agent_id`s, unreadable by any future Bootstrap. Fix: children no longer get `tool: memory_store` in the manifest examples; they return findings in their reply and Bootstrap persists only what's worth keeping. Children may still use `memory_query` (read-only).
3. **Workspace `file_list` denied for children.** Children got `file_write: /data/workspace/X/*` but no matching `file_read`; `file_list` (gated on `FileRead`) refused. Fix: manifest rule to grant both, with a note that `file_write: /src/*` will fail because source is read-only.

**Cost bookkeeping correction.** Earlier per-run cost estimates in docs were computed naively from `docker logs` token counts at a flat rate. DeepSeek's context caching discounts cache-hit input tokens to ~10% of the normal price, which a persistent Bootstrap benefits from heavily (same system prompt + skill catalog + growing history on every iteration). The authoritative cumulative spend since switching to DeepSeek is **~$0.54** per dashboard — not the higher per-run sums estimated in prior roadmap entries. With earlier Anthropic runs, rough cumulative is ~$0.70 across all five self-reflection rounds. The "pennies per run" framing holds; the exact per-run breakdown in earlier docs was over-estimated.

**Artifacts:** 12 workspace files from run 5 are committed under `output/run-5-artifacts/` (design docs, schemas, pseudocode in JS+Python, plus `memory-dump.json` exporting all 14 stored memories for the record).

**Not implemented (still deferred):** The structured `PatternStore`, new `aaos-reflection` crate, and `CoordinationPattern` schema are still not warranted. Run 5 confirmed the minimal protocol works; we now need 5-10 more runs before there's enough real data to justify designing a schema around it.

## Phase F: Real Kernel Migration

Move from userspace abstractions on Linux to a real capability-based microkernel.

**Target kernels.** Redox OS (Rust-native, capability-based, active development) or seL4 (formally verified). The agent syscall API is already defined by the `AgentServices` trait — the migration replaces the implementation, not the interface.

**What changes.** Capability tokens become kernel objects, not userspace UUIDs. Agent isolation uses hardware-enforced process boundaries, not Docker containers. The audit trail is a kernel subsystem, not an application-level log. IPC uses kernel message passing, not Unix sockets.

**What stays the same.** The `AgentServices` trait. The `Tool` trait. The manifest format. The API methods. Everything above the kernel — the entire agent programming model — is unchanged. Applications (agent manifests, tools, orchestration logic) work identically. This is the Android pattern: the app model is the product, the kernel is an implementation detail.

**Prerequisites.** Phases B through D have proven the abstractions under real workloads. The kernel migration replaces the implementation, not the programming model.
