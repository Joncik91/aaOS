# aaOS — Agent-First Operating System

**The vision:** A new kind of kernel where AI agents are native processes, capabilities replace permissions, and the OS is designed for autonomy — not human interaction.

**What exists today:** A working prototype that proves these abstractions in userspace on Linux. 6 Rust crates, ~6000 lines, 111 tests. Not a kernel yet — a proof of concept that validates the programming model before it gets baked into one.

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

- **Capability-based security** — Unforgeable tokens, zero-permission default, two-level enforcement (tool access + resource path)
- **Agent orchestration** — Parent spawns children with narrowed capabilities. Delegation chains are auditable.
- **Human-in-the-loop approval** — Manifests declare which tools need approval. Execution blocks until a human responds via the API.
- **Kernel-level audit trail** — Every action logged: spawn, capability grant, tool invoke, message send, approval request/response
- **Structured IPC** — MCP-native message routing with capability validation
- **Real LLM integration** — Agents call the Anthropic API, use real tools (file I/O, HTTP), subject to real capability enforcement

## The Path to a Real Kernel

```
 You are here
      |
      v
 [Prototype]  Userspace on Linux — proving the model        <-- this repo
      |
 [Persistent Agents]  Long-running agents, request-response IPC
      |
 [Agent Memory]  Managed context windows, episodic store, shared knowledge
      |
 [Supervision Dashboard]  Web UI for monitoring, approval, policy
      |
 [Inference Scheduling]  LLM as a kernel-scheduled resource
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
|        Tool & Service Layer                  |  5 tools, capability-checked, schema-validated
+---------------------------------------------+
|          Agent Kernel                        |  Process model, registry, tokens, IPC router
+---------------------------------------------+
|       Linux (userspace prototype)            |  Docker-isolated, replaced by real kernel in Phase F
+---------------------------------------------+
```

6 Rust crates:
- **aaos-core** — Types, traits, capability model, audit events
- **aaos-runtime** — Agent process lifecycle, registry, scheduling
- **aaos-ipc** — MCP message router with capability validation
- **aaos-tools** — Tool registry, invocation, built-in tools
- **aaos-llm** — LLM client, execution loop
- **agentd** — Daemon binary, Unix socket API, approval queue

## Agent Manifest

Agents are declared bundles:

```yaml
name: research-agent
model: claude-haiku-4-5-20251001
system_prompt: "You are a helpful research assistant."
capabilities:
  - web_search
  - "file_read: /data/project/*"
  - "file_write: /data/output/*"
  - "tool: web_fetch"
  - "tool: file_write"
approval_required:
  - file_write
```

## Quick Start

Requires Docker.

```bash
# Clone
git clone https://github.com/Joncik91/aaOS.git
cd aaOS

# Run tests
docker compose run --rm test

# Start the daemon (requires Anthropic API key)
ANTHROPIC_API_KEY="sk-..." docker compose up daemon

# In another terminal — spawn and run an agent
echo '{"jsonrpc":"2.0","id":1,"method":"agent.spawn_and_run","params":{
  "manifest":"name: hello\nmodel: claude-haiku-4-5-20251001\nsystem_prompt: \"Be concise.\"\n",
  "message":"What is 2+2?"
}}' | socat -t30 - UNIX-CONNECT:/run/agentd/agentd.sock
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
