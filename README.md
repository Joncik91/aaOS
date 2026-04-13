# aaOS — Agent Runtime

**An agent-first runtime where AI agents are native processes, capabilities replace permissions, and the system is designed for autonomy — not human interaction.**

**The long-term vision:** Migrate these abstractions to a real capability-based microkernel (Redox OS or seL4) where agent isolation is hardware-enforced and inference is a schedulable resource like CPU time.

**What exists today:** A working agent runtime on Linux. 7 Rust crates, ~13,000 lines, 220+ tests. Runs autonomously in Docker — a Bootstrap Agent receives a goal, spawns specialized child agents, and produces output with zero human intervention. The system has designed its own features and audited its own security — both for pennies.

## Why an Agent Runtime

Agent frameworks bolt orchestration onto existing runtimes. aaOS takes the opposite approach: build the runtime around agents from the ground up.

- **Agents are processes.** They have lifecycles, registries, schedulers, and supervisors — managed by the runtime, not by application code.
- **Capabilities replace permissions.** No ambient authority. Unforgeable tokens granted at spawn, validated on every operation, narrowable but never escalatable. An agent with `file_write: /data/output/*` *cannot* write to `/etc/` — not as a policy, but as a runtime guarantee.
- **Communication is typed.** MCP messages (JSON-RPC 2.0) with schema validation replace raw byte pipes. Everything is parseable, validatable, and auditable.
- **Inference is a schedulable resource.** LLM API calls are managed by the runtime — with concurrency limits, budgets, and provider abstraction.

## What the Runtime Provides

aaOS runs as a daemon (`agentd`) on Linux, isolated in Docker. The `AgentServices` trait defines the syscall interface — designed to survive a future migration to a real microkernel.

What's implemented and tested:

- **Self-bootstrapping agent swarms** — A Bootstrap Agent (DeepSeek Reasoner) receives a goal, analyzes it, spawns specialized child agents (DeepSeek Chat) with narrowed capabilities, coordinates their work, and produces output. All autonomous. Runs in Docker with `agentd` as PID 1. Multi-provider: works with DeepSeek, Anthropic, or any OpenAI-compatible API.
- **Persistent goal queue** — The Bootstrap Agent runs as a persistent process, accepting goals via Unix socket. Container stays alive between tasks.
- **Capability-based security** — Unforgeable tokens, zero-permission default, two-level enforcement (tool access + resource path). Parent agents can only delegate capabilities they hold — "you can only give what you have." Path normalization prevents traversal attacks; child tokens inherit parent constraints.
- **Agent orchestration** — Parent spawns children with narrowed capabilities. Spawn depth limit (5), agent count limit (100). Failed children are retried once automatically.
- **Persistent agents** — Long-running agents with background message loops, request-response IPC via `send_and_wait()`, conversation persistence in JSONL
- **Managed context windows** — Runtime transparently summarizes old messages via LLM when the context fills, archives originals to disk. Agents see coherent conversations without hitting token limits.
- **Episodic memory** — Per-agent persistent memory via `memory_store`/`memory_query`/`memory_delete` tools. Semantic search via cosine similarity over embeddings (Ollama nomic-embed-text).
- **Workspace isolation** — Each goal gets its own workspace directory. Child agents write intermediate files there.
- **Inference scheduling** — Semaphore-based concurrency limiter prevents API stampedes when multiple agents run simultaneously. Configurable max concurrent calls per provider.
- **Per-agent token budgets** — Agents declare token limits in their manifest. The runtime enforces budgets via atomic tracking in `report_usage()`. Exceeded agents get stopped. Optional — no budget means no enforcement.
- **Audit trail** — 21 event kinds, streamed as JSON-lines to stdout for container observability
- **Verbose agent logging** — Full agent thoughts, tool calls with arguments, and tool results streamed to stdout. Live dashboard shows agent activity in real-time.
- **Structured IPC** — MCP-native message routing with capability validation, request-response via pending-response map
- **Self-designing capability** — Agents can read the mounted aaOS source code at `/src/` and produce working Rust implementations. The OS has designed its own budget enforcement system.
- **Self-auditing security** — The system performed a security audit of itself (1.37M tokens, $0.05), found a real path traversal vulnerability in `glob_matches` that had been present since Phase A, and produced a hardening plan. The vulnerability was fixed based on the audit findings.

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
 [Multi-Provider LLM]  DeepSeek, inference scheduling, budgets   ✅ Phase E  <-- you are here
      |
 [Security Hardening]  Self-audit, path traversal fix, hardening  ✅ Done
      |
 [Real Kernel]  Migrate to Redox OS or seL4 microkernel          Future
```

The `AgentServices` trait is the bridge between runtime and kernel. The `Tool` trait defines tool integration. The manifest format defines agent bundles. When the kernel migration happens, everything above changes implementation — not interface. Agent manifests, tools, and orchestration logic work identically.

See [Roadmap](docs/roadmap.md) for details on each phase.

## Architecture

```
+---------------------------------------------+
|         Human Supervision Layer              |  Approval queue, audit trail
+---------------------------------------------+
|          Orchestration Layer                 |  spawn_agent, capability narrowing
+---------------------------------------------+
|        Tool & Service Layer                  |  8 tools, capability-checked, schema-validated
+---------------------------------------------+
|        Agent Memory Layer                    |  Context windows, episodic store, embeddings
+---------------------------------------------+
|          Agent Runtime Core                  |  Process model, registry, tokens, IPC router
+---------------------------------------------+
|       Linux + Docker                         |  Host OS, replaced by microkernel in future
+---------------------------------------------+
```

7 Rust crates:
- **aaos-core** — Types, traits, capability model, audit events (21 kinds), budget tracking
- **aaos-runtime** — Agent process lifecycle, registry, scheduling, context window management
- **aaos-ipc** — MCP message router with capability validation, request-response IPC
- **aaos-tools** — Tool registry, invocation, 8 built-in tools (including memory tools)
- **aaos-llm** — LLM clients (Anthropic + OpenAI-compat), execution loop, inference scheduler
- **aaos-memory** — Episodic memory store, embedding source, cosine similarity search
- **agentd** — Daemon binary, Unix socket API, approval queue

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

## Quick Start

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

The Bootstrap Agent (DeepSeek Reasoner) analyzes the goal, spawns specialized child agents (DeepSeek Chat), coordinates their work, and writes the output. Total cost: ~$0.02. Falls back to Anthropic if `ANTHROPIC_API_KEY` is set instead.

The source code is mounted read-only at `/src/` inside the container, so agents can read and understand the codebase when given code-related goals.

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
| `tool.list` | List registered tools |
| `tool.invoke` | Invoke a tool on behalf of an agent |
| `approval.list` | List pending approval requests |
| `approval.respond` | Approve or deny a pending request |

## Tools

| Tool | Capability | Description |
|------|-----------|-------------|
| `echo` | `tool: echo` | Returns input (testing) |
| `web_fetch` | `WebSearch` | HTTP GET a URL |
| `file_read` | `FileRead { path_glob }` | Read file, path-checked |
| `file_write` | `FileWrite { path_glob }` | Write file, path-checked |
| `spawn_agent` | `SpawnChild { allowed_agents }` | Spawn child with narrowed capabilities |
| `memory_store` | `tool: memory_store` | Store a fact/observation/decision/preference |
| `memory_query` | `tool: memory_query` | Semantic search over stored memories |
| `memory_delete` | `tool: memory_delete` | Delete a stored memory by ID |

## Design Principles

1. **Agent-Native, Human-Optional** — The runtime boots into an agent process. Humans provide goals, not instructions.
2. **Capability-Based Security** — No ambient authority. Unforgeable tokens replace permissions.
3. **Structured Communication** — Typed MCP messages, not raw byte pipes.
4. **Observable by Default** — Every action logged as a runtime guarantee.
5. **Kernel-Ready Abstractions** — `AgentServices` is the future syscall interface. Code written today works on a real microkernel tomorrow.

## Documentation

- [Architecture](docs/architecture.md) — Layer details and design decisions
- [Roadmap](docs/roadmap.md) — Phase-by-phase path from runtime to real kernel
- [Build Retrospective](docs/retrospective.md) — How the runtime was built and evolved

## License

[Apache License 2.0](LICENSE)
