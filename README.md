# aaOS — Agent-First Operating System

**The vision:** A new kind of kernel where AI agents are native processes, capabilities replace permissions, and the OS is designed for autonomy — not human interaction.

**What exists today:** A working prototype that proves these abstractions in userspace on Linux. 7 Rust crates, ~12,000 lines, 209 tests. Runs autonomously in Docker — a Bootstrap Agent receives a goal, spawns specialized child agents, and produces output with zero human intervention.

## Why a New Kernel

Today's operating systems were designed for humans interacting with programs through screens. AI agents don't need display servers, window managers, or interactive shells. They need capability-based security, structured communication, and orchestration primitives. aaOS asks: what does an OS look like when you design it for agents from the ground up?

The answer isn't an agent framework bolted onto Linux. It's a new kernel where:

- **Agents are processes.** They have lifecycles, registries, schedulers, and supervisors — managed by the kernel, not by application code.
- **Capabilities replace permissions.** No ambient authority. Unforgeable tokens granted at spawn, validated on every operation, narrowable but never escalatable. An agent with `file_write: /data/output/*` *cannot* write to `/etc/` — not as a policy, but as a kernel guarantee.
- **Communication is typed.** MCP messages (JSON-RPC 2.0) with schema validation replace raw byte pipes. Everything is parseable, validatable, and auditable.
- **Inference is a schedulable resource.** Like CPU time, LLM inference is allocated by the kernel — with budgets, priorities, and fair scheduling.

## What the Prototype Proves

This repo is a **userspace prototype** — the agent programming model running as a daemon on Linux, isolated in Docker. It proves the abstractions work before committing them to a real microkernel.

What's implemented and tested:

- **Self-bootstrapping agent swarms** — A Bootstrap Agent (Sonnet) receives a goal, analyzes it, spawns specialized child agents (Haiku) with narrowed capabilities, coordinates their work, and produces output. All autonomous. Runs in Docker with `agentd` as PID 1.
- **Persistent goal queue** — The Bootstrap Agent runs as a persistent process, accepting goals via Unix socket. Container stays alive between tasks.
- **Capability-based security** — Unforgeable tokens, zero-permission default, two-level enforcement (tool access + resource path). Parent agents can only delegate capabilities they hold — "you can only give what you have."
- **Agent orchestration** — Parent spawns children with narrowed capabilities. Spawn depth limit (5), agent count limit (100). Failed children are retried once automatically.
- **Persistent agents** — Long-running agents with background message loops, request-response IPC via `send_and_wait()`, conversation persistence in JSONL
- **Managed context windows** — Runtime transparently summarizes old messages via LLM when the context fills, archives originals to disk. Agents see coherent conversations without hitting token limits.
- **Episodic memory** — Per-agent persistent memory via `memory_store`/`memory_query`/`memory_delete` tools. Semantic search via cosine similarity over embeddings (Ollama nomic-embed-text).
- **Workspace isolation** — Each goal gets its own workspace directory. Child agents write intermediate files there.
- **Kernel-level audit trail** — 21 event kinds, streamed as JSON-lines to stdout for container observability
- **Structured IPC** — MCP-native message routing with capability validation, request-response via pending-response map

## The Path to a Real Kernel

```
 You are here
      |
      v
 [Prototype]  Userspace on Linux — proving the model           ✅ Phase A
      |
 [Persistent Agents]  Long-running agents, request-response IPC  ✅ Phase B
      |
 [Agent Memory]  Managed context windows, episodic store         ✅ Phase C
      |
 [Self-Bootstrapping VM]  Autonomous agent swarms in Docker      ✅ Phase D  <-- you are here
      |
 [Real Kernel]  Migrate to Redox OS or seL4 microkernel
```

The `AgentServices` trait defines the syscall interface. The `Tool` trait defines tool integration. The manifest format defines agent bundles. When the kernel migration happens, everything above the kernel — the entire agent programming model — stays the same. Applications work identically. The kernel is an implementation detail.

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
|          Agent Kernel                        |  Process model, registry, tokens, IPC router
+---------------------------------------------+
|       Linux (userspace prototype)            |  Docker-isolated, replaced by real kernel in Phase F
+---------------------------------------------+
```

7 Rust crates:
- **aaos-core** — Types, traits, capability model, audit events (21 kinds)
- **aaos-runtime** — Agent process lifecycle, registry, scheduling, context window management
- **aaos-ipc** — MCP message router with capability validation, request-response IPC
- **aaos-tools** — Tool registry, invocation, 8 built-in tools (including memory tools)
- **aaos-llm** — LLM client, execution loop with history and system prompt override
- **aaos-memory** — Episodic memory store, embedding source, cosine similarity search
- **agentd** — Daemon binary, Unix socket API, approval queue

## Agent Manifest

Agents are declared bundles:

```yaml
name: research-agent
model: claude-haiku-4-5-20251001
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
  summarization_model: "claude-haiku-4-5-20251001"
  episodic_enabled: true
```

## Quick Start

Requires Docker and an Anthropic API key.

```bash
# Clone
git clone https://github.com/Joncik91/aaOS.git
cd aaOS

# Run the autonomous bootstrap demo
ANTHROPIC_API_KEY="sk-..." AAOS_GOAL="Fetch https://news.ycombinator.com and write a summary of the top 5 stories to /output/summary.txt" \
  docker compose -f docker-compose.bootstrap.yaml up

# Check the output
cat output/summary.txt
```

The Bootstrap Agent (Sonnet) analyzes the goal, spawns Fetcher and Writer agents (Haiku), coordinates their work, and writes the output. Total cost: ~$0.03.

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

1. **Agent-Native, Human-Optional** — The OS boots into an agent runtime. No display server.
2. **Capability-Based Security** — No ambient authority. Unforgeable tokens replace Unix permissions.
3. **Structured Communication** — Typed MCP messages, not raw byte pipes.
4. **Observable by Default** — Every action logged as a kernel guarantee.
5. **Reversibility as a Primitive** — Side effects captured transactionally where possible.

## Documentation

- [Architecture](docs/architecture.md) — Layer details and design decisions
- [Roadmap](docs/roadmap.md) — Phase-by-phase path from prototype to real kernel
- [Build Retrospective](docs/retrospective.md) — How a working prototype was built in 48 hours

## License

[Apache License 2.0](LICENSE)
