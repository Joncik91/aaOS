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

## Self-Reflection Rounds *(ongoing)*

The runtime has begun reading its own code, finding bugs, and proposing features. Seven runs to date — runs 1-3 produced real bug fixes (path traversal, capability revocation, constraint enforcement), run 4 produced a feature proposal (Meta-Cognitive Coordination Layer) shipped as a minimal version after external review, run 5 exercised the persistent-memory protocol end-to-end and produced three manifest-only tuning fixes, run 6 surfaced two kernel-level gaps in the Run-5 manifest tuning (soft rules aren't enforcement; no structured child-to-child data channel) that shipped as kernel fixes `505f559` and `5feedbe`, and run 7 validated those fixes against real behavior with a four-agent chain producing a grounded error-handling unification proposal.

Full chronological detail per run lives in [`reflection/`](reflection/README.md). Cross-cutting lessons distilled from the runs (LLM calendar estimates aren't real, cost from token-math ≠ dashboard, skill adherence evolves, prompts persuade but only the kernel enforces, structured handoff beats opaque prompts) are in [`patterns.md`](patterns.md).

**What's deferred pending more data:** the structured `PatternStore`, new `aaos-reflection` crate, and `CoordinationPattern` schema are still not warranted. The minimal protocol (stable Bootstrap ID + opt-in persistent memory + query-before/store-after in the manifest) is the empirical foundation. If 10-20 runs surface recurring patterns worth indexing formally, the structured system gets designed against real data — not speculation.

## Phase F: Agent-Native Linux Distribution *(next)*

Full component and migration sketch in [`distribution-architecture.md`](distribution-architecture.md). Short version below.

Build aaOS as a Linux distribution where the primary workload is an agent runtime — like CoreOS was container-native Linux and Bottlerocket is Kubernetes-native Linux. Upstream kernel (no fork), curated userland, `agentd` as a first-class service, capability enforcement mapped onto Linux primitives that already exist.

**Why this, not a microkernel fork.** aaOS's differentiation is capability semantics, delegation, auditability, and policy compilation — not owning a kernel. A microkernel migration pushes the "it ships" date years out while losing the Linux ecosystem (GPU drivers, package management, every tool an agent might call through typed wrappers). A hardened Linux appliance puts the capability model in real users' hands within quarters, not years.

**What changes (concrete).**
- **`agentd` as a systemd service** (not PID 1 — that branding costs more edge-case burden than it's worth).
- **Capability tokens stay the policy model.** Enforcement gets a defense-in-depth backstop via Linux primitives — capabilities are still issued, narrowed, audited, and checked by `agentd`.
- **Namespaces** for per-agent isolation (mount, pid, net, user, cgroup). Replaces Docker-as-only-substrate.
- **Seccomp-BPF** as a damage-limiter, not the model. Syscall allowlists per agent derived from manifest capabilities.
- **Landlock** (Linux 5.13+) for filesystem capability enforcement at the kernel layer. Path-glob capabilities compile to Landlock rulesets.
- **cgroups v2** for CPU/memory/I/O quotas per agent — resource budgets become first-class.
- **Typed MCP wrappers for Linux tools.** `grep`, `jq`, `git`, `cargo`, `gcc`, `ffmpeg`, `pandoc` — each exposed as a tool with a declared capability. Full POSIX ecosystem for agents, every call capability-checked at the wrapper boundary.

**Deliverables.**
- Debian/Ubuntu package: `apt install aaos`.
- ISO for bare-metal installs.
- Cloud images (AMI, GCE, Azure).
- Nix expression for reproducible builds.
- Reference hardware: NUCs, Raspberry Pi 5, cloud VMs.

**What stays the same.** The `AgentServices` trait. The `Tool` trait. The manifest format. The runtime API methods. The whole agent programming model. Distribution is the *substrate*; the programming model is the product.

**Migration path from today.** Phase by phase, no big-bang rewrite:
1. `agentd.service` systemd unit on any Linux — today's binary, zero change.
2. Per-agent seccomp profiles auto-generated from capability manifests.
3. Landlock + cgroup v2 integration.
4. Minimal Debian-based image with `agentd` preinstalled and configured.
5. Typed MCP wrappers for ~30 common Linux tools.

**Progress:** Second `AgentBackend` implementation (`NamespacedBackend`)
landed as scaffolding in commits `a84cd98` + `a73e062`. Handshake protocol,
Landlock + seccomp compilation, broker session with peer-creds, fail-closed
missing-Landlock detection all working and unit-tested. Kernel launch
mechanics (clone + uid_map + pivot_root + exec) pending manual verification
on a root-privileged Linux 5.13+ host — the path is pinned by a unit test
that asserts `CloneFailed` so a future completion can't land silently.
Isolated dev VM provisioned for this work (Debian 13, kernel 6.12.43,
Landlock + unprivileged user namespaces available). Stub-finish
implementation queued: plan + peer review → sub-agent implementation →
integration-test un-ignore → `/proc/<pid>/status` verification → .deb
packaging. The `.deb` ships once the launch path is verified; until then
the distribution defaults to `InProcessBackend` with `NamespacedBackend`
as an opt-in feature flag.

## Phase G: Isolation Ladder *(research branch)*

With two backend implementations already proving `AgentServices` is substrate-agnostic, Phase G adds a third: MicroVM-per-agent via Firecracker or Kata. The same agent manifest runs on different isolation levels depending on threat model:

- **Level 1 — Process** (current): Linux process with seccomp+Landlock. Low overhead, appropriate for trusted workloads.
- **Level 2 — MicroVM**: Firecracker / Kata / gVisor per agent (or per swarm). Hardware-virtualized isolation; what AWS Lambda and Fly.io use. Strong tenant isolation without writing a kernel.
- **Level 3 — Microkernel** (research): seL4 or Redox backend, only pursued if a specific market segment (high-assurance regulated deployments) demands formally verified isolation enough to fund it. Not prioritized; documented as a backend option on a clean ABI so the door stays open.

**Why this matters.** The `AgentServices` trait was originally pitched as "future syscall interface." Reframe: it's a **substrate-agnostic ABI**. An operator picks their isolation level based on threat model and resource budget, not on what kernel we happened to build.

**Prerequisites.** Phase F ships. Real workloads on hardened Linux prove the capability model. If tenant-isolation pressure emerges, MicroVM backend is the next layer. Microkernel only if formally-verified enforcement is the buyer's gating requirement.
