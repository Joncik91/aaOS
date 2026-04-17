# aaOS — Agent Runtime

**An agent-first runtime where AI agents are native processes, capabilities replace permissions, and the system is designed for autonomy — not human interaction.**

**The long-term vision:** A Debian derivative where aaOS runs as the system orchestrator — think Home Assistant OS or Raspberry Pi OS for agents. Debian 13 base, aaOS preinstalled, `NamespacedBackend` as the default agent backend under the `namespaced-agents` feature, Landlock + seccomp enforcing capability tokens at the kernel layer. Shipped as a `.deb` you can install on your own Debian, or as bootable ISO / cloud snapshots. Debian provides the kernel, apt repos, and security updates — we provide the agent runtime and the opinionated defaults. The `AgentServices` trait is a substrate-agnostic ABI: process-backed today, MicroVM-per-agent (Firecracker/Kata/gVisor) later for harder tenant isolation, microkernel (seL4/Redox) only if a customer demands formally verified boundaries. The programming model is the product; the substrate is replaceable.

**What exists today:** A working agent runtime on Linux. 8 Rust crates, ~26,300 lines, 466 tests. Ships as a Debian `.deb` with an operator CLI (`agentd submit|list|status|stop|logs|roles`). Orchestration is two-phase: a cheap-LLM **Planner** emits a structured Plan; a deterministic Rust **PlanExecutor** walks the resulting DAG, running independent subtasks in parallel. The legacy Bootstrap-Agent-in-Docker path (autonomous single-LLM orchestrator) is still supported as a fallback when no role catalog is installed. Linux-namespaced agent backend (Landlock + seccomp + peer-creds broker socket, `clone() + uid_map + pivot_root + exec` launch path) is implemented and verified on Debian 13 / kernel 6.12.43 — live workers show `Seccomp: 2`, `NoNewPrivs: 1`, both stacked seccomp filters installed; shipped off-by-default on the base `.deb` until Phase F-b's cloud image flips the default. The capability model, audit trail, and substrate-agnostic ABI date to Phase A and have survived ten self-reflection runs of the system reading its own code. See [`docs/reflection/`](docs/reflection/README.md) for what those runs surfaced and fixed.

## Why an Agent Runtime

Agent frameworks bolt orchestration onto existing runtimes. aaOS takes the opposite approach: build the runtime around agents from the ground up.

- **Agents are processes.** They have lifecycles, registries, schedulers, and supervisors — managed by the runtime, not by application code.
- **Capabilities replace permissions.** Agents (as LLM tool-call entities) start with zero capabilities. Runtime-issued tokens granted at spawn, validated on every tool invocation, narrowable-only on delegation. An agent with `file_write: /data/output/*` is refused by the bundled `file_write` tool if it tries to write outside that glob — the check is in the tool wrapper, not the kernel. The runtime itself (`agentd`) still runs with ambient OS authority; Phase F adds Landlock-backed kernel enforcement as defense-in-depth.
- **Communication is schema-validated.** MCP messages (JSON-RPC 2.0) go through a validator that enforces required fields and basic types. Full JSON Schema validation (patterns, enums, nested schemas) is not implemented — noted in `docs/ideas.md` as a deferred hardening item.
- **Inference is a managed resource.** LLM API calls go through a concurrency limiter (tokio semaphore, default 3 concurrent per provider), optional rate smoothing, per-agent token budgets, and provider abstraction. Not a true scheduler — no priority, fairness, or preemption.

## What the Runtime Provides

aaOS runs as a daemon (`agentd`) on Linux — either as a systemd unit from an installed `.deb`, or as PID 1 of a Docker container. The `AgentServices` trait is a substrate-agnostic ABI — today implemented with Linux processes, tomorrow with MicroVMs, maybe someday with a microkernel if formally-verified isolation ever becomes the gating requirement.

What's implemented and tested:

- **Computed orchestration (default) or self-bootstrapping swarms (fallback).** When `/etc/aaos/roles/*.yaml` is present, a cheap-LLM **Planner** emits a typed `Plan` — a DAG of role-instantiated subtasks with explicit dependencies — and a deterministic Rust **PlanExecutor** walks the DAG, running independent subtasks concurrently (via `futures::try_join_all`) and sequential ones in order. No LLM is in the orchestration loop. Four roles ship: `fetcher`, `writer`, `analyzer`, `generalist` — operator-extensible. When the catalog is absent (e.g. dev setups without role YAMLs), the legacy Bootstrap Agent path takes over: a DeepSeek Reasoner receives the goal, spawns child agents with narrowed capabilities, and coordinates their work as before. Multi-provider: DeepSeek, Anthropic, or any OpenAI-compatible API.
- **Operator CLI** — Same binary as the daemon. `agentd submit "<goal>"` streams live audit events over NDJSON as the Planner decides and the PlanExecutor walks the DAG. `list`/`status`/`stop`/`logs` cover the lifecycle operations; `roles list|show|validate` inspects the role catalog at `/etc/aaos/roles/`. `man agentd` for the full reference. The `.deb` creates an `aaos` system group at install time — operators join it via `adduser` to get socket access.
- **Persistent goal queue** — Goals submitted via the Unix socket API (`/run/agentd/agentd.sock` when systemd-installed, `/tmp/aaos-sock/agentd.sock` in the Docker deployment). Same socket answers whether the active orchestrator is computed (PlanExecutor) or Bootstrap.
- **Capability-based security** — Runtime-issued, handle-opaque tokens. Agents and tools hold `CapabilityHandle` values; the underlying `CapabilityToken` lives in a runtime-owned `CapabilityRegistry` and is never exposed to non-runtime code. Zero-capability default for agents. Two-level enforcement inside bundled tools (tool access + resource path). Parent agents can only delegate capabilities they hold — "you can only give what you have." Path canonicalization (filesystem-resolved, including symlinks) prevents traversal and symlink-bypass attacks. Child tokens inherit parent constraints. Tokens are revocable at runtime. **Scope of enforcement:** bundled tools check capabilities at their call boundary via the registry. Third-party tool plugins must also route through the registry — the runtime provides handles, not tokens, so direct inspection isn't possible without a registry reference. HMAC signing for cross-process/cross-host transport remains a deferred hardening item.
- **Agent orchestration** — Parent spawns children with narrowed capabilities. Spawn depth limit (5), agent count limit (100). Failed children are retried once automatically. Parallel spawning via `spawn_agents` batch tool runs up to 3 independent children concurrently (tunable via `AAOS_SPAWN_AGENTS_BATCH_CAP`) — wall-clock time is the slowest child, not the sum.
- **Pluggable agent backend** — `AgentBackend` trait abstracts the "how do I actually run an agent's execution context" contract behind `AgentServices`. Two implementations today: `InProcessBackend` (the default — a tokio task in `agentd`) and `NamespacedBackend` (opt-in under the `namespaced-agents` feature — Linux-namespaced worker subprocesses with Landlock + seccomp, verified end-to-end on Debian 13 / kernel 6.12.43). Today's shipped default on the base `.deb` remains in-process because the kernel prerequisites (unprivileged user namespaces, Landlock) aren't universal across self-installed Debian hosts; Phase F-b's cloud image bakes them in and flips the default. Future backends (MicroVM, microkernel) need a new crate, not changes to `aaos-core`.
- **Persistent agents** — Long-running agents with background message loops, request-response IPC via `send_and_wait()`, conversation persistence in JSONL
- **Managed context windows** — Runtime transparently summarizes old messages via LLM when the context fills, archives originals to disk. Agents see coherent conversations without hitting token limits.
- **Episodic memory** — Per-agent persistent memory via `memory_store`/`memory_query`/`memory_delete` tools. Semantic search via cosine similarity over embeddings. SQLite-backed for persistence across container restarts (`AAOS_MEMORY_DB`), falls back to in-memory if unset.
- **Workspace isolation** — Each goal gets its own workspace directory. Child agents write intermediate files there.
- **Inference scheduling** — Semaphore-based concurrency limiter prevents API stampedes when multiple agents run simultaneously. Configurable max concurrent calls per provider.
- **Per-agent token budgets** — Agents declare token limits in their manifest. The runtime enforces budgets via atomic tracking in `report_usage()`. Exceeded agents get stopped. Optional — no budget means no enforcement.
- **Audit trail** — 26 event kinds (including `PlanProduced`, `PlanReplanned`, `SubtaskStarted`, `SubtaskCompleted`). Streamed as JSON-lines to stdout in the Docker path, to journald when installed as a `.deb`, and to any number of NDJSON subscribers via `BroadcastAuditLog` — the CLI's streaming subcommands (`submit`, `logs`) each hold one.
- **Verbose agent logging** — Full agent thoughts, tool calls with arguments, and tool results streamed to stdout. Live dashboard shows agent activity in real-time.
- **Structured IPC** — MCP-native message routing with capability validation, request-response via pending-response map
- **Self-drafting capability** — Agents can read the mounted aaOS source code at `/src/` and produce Rust implementation drafts. Humans integrate and verify — the drafts sometimes don't compile without edits. The system drafted its own budget enforcement system; the shipped code was the draft after human integration plus peer review.
- **Self-auditing security** — The system performed a security audit of itself and found a real path traversal vulnerability in `glob_matches` that had been present since Phase A, then extended the fix (in a later run) to close a symlink bypass of the lexical-only check. Both shipped. The pattern — runtime reads its own code, proposes fixes, humans review and integrate — has caught runtime bugs, capability bypass classes, and prompt-engineering regressions across ten runs.
- **Reflection log, not marketing prose** — Every self-reflection run produces a dated per-entry file under [`docs/reflection/`](docs/reflection/README.md) with its integration commits, what worked, what surfaced, and what shipped. Cross-cutting lessons graduate to [`docs/patterns.md`](docs/patterns.md). If you want the run-by-run evidence, those files are the source of truth — this README stopped running a chronicle when Phase F-a shipped.

## Roadmap

```
 [Runtime Prototype]  Agent lifecycle, capabilities, tools       ✅ Phase A
      |
 [Persistent Agents]  Long-running agents, request-response IPC  ✅ Phase B
      |
 [Agent Memory]  Managed context windows, episodic store         ✅ Phase C
      |
 [Self-Bootstrapping]  Autonomous agent swarms in Docker          ✅ Phase D
      |
 [Multi-Provider LLM]  DeepSeek, inference scheduling, budgets   ✅ Phase E
      |
 [Security]  Self-audit, path traversal fix, revocation           ✅ Done
      |
 [AgentSkills]  Open standard, 21 skills, progressive disclosure  ✅ Done
      |
 [Self-Reflection]  System reads own code, proposes features      ✅ Done
      |
 [Debian Derivative]  .deb + CLI + computed orchestration        ✅ Phase F-a   <-- you are here
      |                 · Debian 13 image (F-b)                  Next
      |
 [Isolation Ladder]  MicroVM-per-agent via Firecracker/Kata    Research
      |
 [Microkernel]  seL4/Redox backend — only if demand exists      Optional
```

The `AgentServices` trait is the bridge between runtime and kernel. The `Tool` trait defines tool integration. The manifest format defines agent bundles. When the kernel migration happens, everything above changes implementation — not interface. Agent manifests, tools, and orchestration logic work identically.

See [Roadmap](docs/roadmap.md) for details on each phase.

## Architecture

```
+---------------------------------------------+
|         Human Supervision Layer              |  CLI (submit/list/status/stop/logs/roles), audit trail, approval queue
+---------------------------------------------+
|          Orchestration Layer                 |  Planner + PlanExecutor (primary) · Bootstrap Agent (fallback)
+---------------------------------------------+
|        Tool & Service Layer                  |  12 tools, capability-checked, schema-validated, AgentSkills registry
+---------------------------------------------+
|        Agent Memory Layer                    |  Context windows, episodic store, embeddings
+---------------------------------------------+
|          Agent Runtime Core                  |  Process model, registry, tokens, IPC router, role catalog
+---------------------------------------------+
|       Agent Backend (pluggable)              |  InProcessBackend (default) · NamespacedBackend (opt-in, Landlock + seccomp)
+---------------------------------------------+
|       Linux + Docker / Debian .deb           |  Host OS today; F-a .deb shipped; Phase F-b Debian 13 image + cloud snapshots
+---------------------------------------------+
```

8 Rust crates:
- **aaos-core** — Types, traits, capability model, audit events (26 kinds), budget tracking
- **aaos-runtime** — Agent process lifecycle, registry, scheduling, context window management, **computed orchestration (`plan/` module — Planner, PlanExecutor, RoleCatalog)**
- **aaos-ipc** — MCP message router with capability validation, request-response IPC
- **aaos-tools** — Tool registry, invocation, 13 built-in tools (echo, web_fetch, file_read/list/read_many/write, spawn_agent/spawn_agents, memory_store/query/delete, skill_read, cargo_run)
- **aaos-llm** — LLM clients (Anthropic + OpenAI-compat), execution loop, inference scheduler
- **aaos-memory** — Episodic memory store, embedding source, cosine similarity search
- **aaos-backend-linux** — Opt-in Linux-namespaced agent backend (`namespaced-agents` feature). User/mount/IPC namespaces, Landlock + seccomp confinement, peer-creds-authenticated broker socket, worker subprocess. `clone() + uid_map + pivot_root + exec` launch path verified end-to-end on Debian 13 / kernel 6.12.43 (2026-04-15 and 2026-04-17 re-verify); live workers show `Seccomp: 2`, `NoNewPrivs: 1`, both stacked seccomp filters installed.
- **agentd** — Daemon + operator CLI binary, Unix socket API, NDJSON streaming, role catalog loading, approval queue

## Agent Manifest

Agents are declared bundles:

```yaml
name: research-agent
model: deepseek-chat
system_prompt: "You are a helpful research assistant with persistent memory."
lifecycle: persistent
capabilities:
  - web_search
  - "file_read: /data/project/*"
  - "file_write: /data/output/*"
  - "tool: web_fetch"
  - "tool: file_write"
  - "tool: memory_store"
  - "tool: memory_query"
approval_required:
  - file_write
memory:
  context_window: "128k"
  max_history_messages: 200
  episodic_enabled: true
budget_config:
  max_tokens: 1000000       # 1M tokens
  reset_period_seconds: 86400  # daily reset
```

## Quick start

**On Debian 13 (preferred):**

```bash
# 1. Install the .deb (download from Releases, or build from source — see below).
sudo apt install ./aaos_0.0.0-1_amd64.deb

# 2. Join the aaos group so your shell can talk to the daemon socket.
#    (Log out and back in after this — group membership only takes effect
#    on a fresh login session.)
sudo adduser $USER aaos

# 3. Configure an LLM provider. DEEPSEEK_API_KEY is preferred; ANTHROPIC_API_KEY
#    works as a fallback.
echo 'DEEPSEEK_API_KEY=sk-...' | sudo tee /etc/default/aaos > /dev/null
sudo chmod 600 /etc/default/aaos

# 4. Start the daemon.
sudo systemctl enable --now agentd

# 5. Send a goal.
agentd submit "fetch HN top 5 stories and write a summary"
```

The CLI streams audit events live as the planner decomposes the goal and the PlanExecutor walks the resulting DAG — fetchers run in parallel, writers read their outputs. `agentd list`, `agentd status <id>`, `agentd stop <id>`, `agentd logs <id>`, and `agentd roles list` complete the operator surface; `man agentd` covers the full CLI reference.

**Building the .deb from source** (Debian 13 host with `cargo`, `cargo-deb`, and `pandoc`):

```bash
./scripts/setup-hooks.sh          # activate in-tree git hooks (once per clone)
cargo build --release -p agentd --bin agentd
cargo build --release -p aaos-backend-linux --bin aaos-agent-worker
./packaging/build-man-page.sh
cargo deb -p agentd --no-build
# target/debian/aaos_*.deb is the installable artifact.
```

`setup-hooks.sh` points git at `.githooks/` so the bundled pre-commit hook (gitleaks-based secret scan) runs on every commit. Install `gitleaks` first (`apt install gitleaks`) or the hook no-ops with a warning.

**On any system with Docker:**

Requires Docker and a DeepSeek API key (or Anthropic API key as fallback).

```bash
# Clone
git clone https://github.com/Joncik91/aaOS.git
cd aaOS

# Run with live dashboard (builds image automatically on first run)
DEEPSEEK_API_KEY="sk-..." ./run-aaos.sh "Fetch https://news.ycombinator.com and write a summary of the top 5 stories to /output/summary.txt"

# Check the output
cat output/summary.txt
```

The launcher starts the container and opens a live dashboard in a separate terminal showing agent activity in real-time. Ctrl+C stops everything.

The Bootstrap Agent (DeepSeek Reasoner) analyzes the goal, spawns specialized child agents (DeepSeek Chat), coordinates their work, and writes the output. This is the *legacy* orchestration path used by the Docker deployment; the `.deb`-installed daemon prefers the computed orchestration (Planner + PlanExecutor) when a role catalog is present at `/etc/aaos/roles/`. Falls back to Anthropic if `ANTHROPIC_API_KEY` is set instead of DeepSeek.

The source code is mounted read-only at `/src/` inside the container, so agents can read and understand the codebase when given code-related goals.

### Cross-run memory (opt-in)

By default, every container starts with a fresh Bootstrap identity and empty memory — the same behavior the first four self-reflection runs used. To let the Bootstrap Agent accumulate lessons across restarts:

```bash
AAOS_PERSISTENT_MEMORY=1 DEEPSEEK_API_KEY="sk-..." ./run-aaos.sh "your goal"
```

This bind-mounts `./memory/` into the container. The Bootstrap ID is persisted at `/var/lib/aaos/bootstrap_id` (overridable via `AAOS_BOOTSTRAP_ID`). The manifest instructs Bootstrap to `memory_query` before decomposing a goal and `memory_store` a compact run summary after completing one. To wipe persistent state, launch once with `AAOS_RESET_MEMORY=1`.

Persistent memory carries real risk: prompt-injected content and bad strategies become durable. The feature is opt-in and reset is one env var away.

To send additional goals to the running container:
```bash
# The container stays alive and accepts goals via Unix socket
echo '{"jsonrpc":"2.0","id":1,"method":"agent.run","params":{
  "agent_id":"<bootstrap-agent-id>",
  "message":"Fetch https://lobste.rs and summarize the top 3 stories to /output/lobsters.txt"
}}' | python3 -c "import socket,sys,json; s=socket.socket(socket.AF_UNIX); s.connect('/tmp/aaos-sock/agentd.sock'); s.sendall((sys.stdin.read()+'\n').encode()); print(s.recv(4096).decode())"
```

## API

JSON-RPC 2.0 over Unix socket.

| Method | Description |
|--------|-------------|
| `agent.spawn` | Spawn an agent from a YAML manifest |
| `agent.stop` | Stop a running agent |
| `agent.list` | List all running agents |
| `agent.status` | Get status of a specific agent |
| `agent.run` | Run an existing agent with a message |
| `agent.spawn_and_run` | Spawn and run in one call |
| `agent.submit_streaming` | Send a goal to Bootstrap; stream audit events as NDJSON until `end` frame |
| `agent.logs_streaming` | Attach to a specific agent's audit stream as NDJSON; no end frame unless the agent terminates |
| `tool.list` | List registered tools |
| `tool.invoke` | Invoke a tool on behalf of an agent |
| `approval.list` | List pending approval requests |
| `approval.respond` | Approve or deny a pending request |

The operator CLI (`agentd submit|list|status|stop|logs`) uses `submit_streaming` and `logs_streaming` under the hood; see `man agentd` for the operator surface.

## Tools

| Tool | Capability | Description |
|------|-----------|-------------|
| `echo` | `tool: echo` | Returns input (testing) |
| `web_fetch` | `WebSearch` | HTTP GET a URL |
| `file_read` | `FileRead { path_glob }` | Read file, path-checked |
| `file_list` | `FileRead { path_glob }` | List directory contents — use before guessing filenames |
| `file_read_many` | `FileRead { path_glob }` (per file) | Batch-read up to 16 files in parallel; partial failures OK |
| `file_write` | `FileWrite { path_glob }` | Write file, path-checked |
| `spawn_agent` | `SpawnChild { allowed_agents }` | Spawn child with narrowed capabilities |
| `spawn_agents` | `SpawnChild { allowed_agents }` (per child) | Spawn up to 3 independent children concurrently; best-effort per-child, wall-clock = slowest child |
| `memory_store` | `tool: memory_store` | Store a fact/observation/decision/preference |
| `memory_query` | `tool: memory_query` | Semantic search over stored memories |
| `memory_delete` | `tool: memory_delete` | Delete a stored memory by ID |
| `skill_read` | `tool: skill_read` | Load skill instructions or reference files |
| `cargo_run` | `CargoRun { workspace }` | Run `cargo check/test/clippy/fmt` in a Rust workspace, under capability scope |

## Skills

aaOS supports the [AgentSkills](https://agentskills.io) open standard by Anthropic. Skills are folders with a `SKILL.md` file that teach agents specialized workflows. The same skills that work in Claude Code, Copilot CLI, Gemini CLI, and OpenCode work in aaOS — but under capability-based security enforcement.

**21 bundled skills** from [addyosmani/agent-skills](https://github.com/addyosmani/agent-skills):

`spec-driven-development` · `test-driven-development` · `incremental-implementation` · `planning-and-task-breakdown` · `code-review-and-quality` · `security-and-hardening` · `debugging-and-error-recovery` · `api-and-interface-design` · `frontend-ui-engineering` · `performance-optimization` · `git-workflow-and-versioning` · `ci-cd-and-automation` · `shipping-and-launch` · `documentation-and-adrs` · `code-simplification` · `context-engineering` · `deprecation-and-migration` · `idea-refine` · `source-driven-development` · `browser-testing-with-devtools` · `using-agent-skills`

**Progressive disclosure:** Agents see the skill catalog (~100 tokens each) in their system prompt at startup. When a task matches a skill, the agent calls `skill_read` to load full instructions on demand. Reference files load only when needed.

**Add your own:** Drop a folder with a `SKILL.md` into `.agents/skills/` or set `AAOS_SKILLS_DIR`.

## Design Principles

1. **Agent-Native, Human-Optional** — The runtime boots into an agent process. Humans provide goals, not instructions.
2. **Capability-Based Security** — Agents start with zero capabilities. Runtime-issued, handle-opaque tokens replace permissions. Agents and tool implementations hold `CapabilityHandle` values; the underlying `CapabilityToken` and its mutable state (invocation counts, revocation) live inside a runtime-owned `CapabilityRegistry` and are never exposed to non-runtime code. A forged handle either resolves to nothing (unknown index) or to a token owned by a different agent (cross-agent leak protection). Still not cryptographically unforgeable — attackers with Rust-level code execution inside `agentd` can construct or mutate handles. HMAC signing for cross-process/cross-host transport is a deferred hardening item. On the Linux-namespaced backend, agents run in isolated subprocesses with Landlock and seccomp applied before the agent loop begins — closing an additional threat class of in-process memory attacks on the capability table, because the worker holds no `CapabilityHandle` values at all and all tool invocations route through a peer-creds-authenticated broker socket. Verified end-to-end on Debian 13 / kernel 6.12.43; opt-in on the base `.deb` install and on by default in Phase F-b's cloud image.
3. **Structured Communication** — Schema-validated MCP messages (required-field + basic-type checking), not raw byte pipes. Full JSON Schema validation is a deferred hardening item.
4. **Observable by Default** — Every tool invocation and agent lifecycle event produces an audit event. Durability depends on the configured backend (`StdoutAuditLog`, `InMemoryAuditLog`, or external sink).
5. **Substrate-Agnostic Abstractions** — `AgentServices` is an ABI, not a kernel API. Today: Linux processes with capability wrappers. Next: Debian derivative with `NamespacedBackend` by default. Later: MicroVM-per-agent if tenant isolation demands it. Microkernel only if a customer demands formally-verified boundaries.

## Documentation

- [Architecture](docs/architecture.md) — Layer details and design decisions
- [Roadmap](docs/roadmap.md) — Phase-by-phase path from runtime to real kernel
- [Build Retrospective](docs/retrospective.md) — Phase-by-phase build history (A through E)
- [Self-Reflection Log](docs/reflection/README.md) — Runs where aaOS reads its own code and proposes changes (per-run entries under `docs/reflection/`)
- [Patterns](docs/patterns.md) — Cross-cutting lessons distilled from the retrospective and reflection log
- [Ideas](docs/ideas.md) — Things we considered and deferred, with the signal that would prompt reconsideration
- [Distribution Architecture](docs/distribution-architecture.md) — The Debian-derivative target: components, capability enforcement via Linux primitives, `.deb` packaging, image build, migration from today's Docker-only deployment

## License

[Apache License 2.0](LICENSE)
