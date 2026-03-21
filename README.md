# aaOS — Agent-First Operating System

An operating system where AI agents are the native processes and humans are supervisors.

Not an agent framework. Not a toolkit on Linux. A rethinking of what an operating system is for when the primary user is no longer a human.

## What Makes It Different

**Capability-based security.** Every agent starts with zero permissions. Capabilities are unforgeable tokens granted at spawn, validated on every operation, and can only be narrowed — never escalated. An agent with `file_write: /data/output/*` literally cannot write to `/etc/`. This isn't a prompt instruction. It's a kernel guarantee.

**Two-level tool enforcement.** First gate: can this agent use this tool at all? Second gate: can it use this tool on this specific path/resource? A research agent might have `tool: file_write` access but only to `/data/research/*`. Both checks happen before every invocation.

**Human-in-the-loop approval.** Manifests declare which tools require human approval. When an agent tries to write a file that requires approval, the execution loop blocks. A human sees the request (agent name, tool, file path, content) and approves or denies via the API. The agent resumes or adapts.

**Agent orchestration with capability narrowing.** A parent agent spawns children with a subset of its own capabilities. The parent's tokens are the ceiling — children get their declared scope, which must be equal to or narrower than what the parent holds. Delegation chains are auditable.

**Kernel-level audit trail.** Every action produces an audit event: agent spawned, capability granted, tool invoked, message sent, approval requested, approval granted. You can trace any action back to root cause.

**Structured IPC.** All communication uses typed MCP messages (JSON-RPC 2.0) with schema validation. No raw text pipes. Everything is parseable, validatable, and auditable.

## Project Status

Working prototype. The agent kernel, capability system, tool registry, execution loop, approval queue, and message router are implemented and tested. Agents call real LLMs (Anthropic API), use real tools (file I/O, HTTP fetch), and are subject to real capability enforcement.

**Not production-ready.** This is a proof of concept exploring how operating system abstractions should change for AI agents.

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
|       Linux (userspace abstractions)         |  Docker-isolated development
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

## License

[Apache License 2.0](LICENSE)
