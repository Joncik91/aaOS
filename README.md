# aaOS — Agent Runtime

[![CI](https://github.com/Joncik91/aaOS/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/Joncik91/aaOS/actions/workflows/ci.yml)

**An agent-first runtime where AI agents are native processes, capabilities replace permissions, and the system is designed for autonomy — not human interaction.**

A working agent runtime on Linux that can read and modify source code — including its own. 9 Rust crates, ~31,000 lines, 528 tests (511 + 17 `#[ignore]`-gated on host prereqs such as ripgrep, git, Linux kernel primitives, or a child toolchain — exercised on CI, optional locally). Ships as a Debian `.deb` with an operator CLI. Orchestration is two-phase: a cheap-LLM **Planner** emits a structured Plan; a deterministic Rust **PlanExecutor** walks the DAG, running independent subtasks in parallel. The capability model, audit trail, and substrate-agnostic ABI have survived twelve self-reflection runs of the system reading its own code; the most recent was the first end-to-end self-build — an agent inside aaOS read a plan, made five surgical edits to a 2700-line Rust file, ran `cargo test`, and produced a byte-identical diff to what the maintainer committed by hand.

The long-term target is a **Debian derivative** where aaOS runs as the system orchestrator (Home Assistant OS for agents), with Landlock + seccomp enforcing capability tokens at the kernel layer. The `AgentServices` trait is a substrate-agnostic ABI: process-backed today, MicroVM-per-agent later, microkernel only if a customer demands formally-verified boundaries. The programming model is the product; the substrate is replaceable.

See [Architecture](docs/architecture.md) for the full stack, [Roadmap](docs/roadmap.md) for where it's going, and [`docs/reflection/`](docs/reflection/README.md) for what each self-reflection run surfaced and fixed.

## Quick start

**On Debian 13:**

```bash
# 1. Install the .deb (download from Releases, or build from source — see below).
sudo apt install ./aaos_0.0.0-1_amd64.deb

# 2. Join the aaos group so your shell can talk to the daemon socket.
#    Log out and back in for group membership to take effect.
sudo adduser $USER aaos

# 3. Configure an LLM provider. DEEPSEEK_API_KEY preferred; ANTHROPIC_API_KEY works as fallback.
echo 'DEEPSEEK_API_KEY=sk-...' | sudo tee /etc/default/aaos > /dev/null
sudo chmod 600 /etc/default/aaos

# 4. Start the daemon and send a goal.
sudo systemctl enable --now agentd
agentd submit "fetch HN top 5 stories and write a summary"
```

The CLI streams audit events live as the Planner decomposes the goal and the PlanExecutor walks the resulting DAG — fetchers run in parallel, writers read their outputs. `agentd list|status|stop|logs|roles` cover the rest of the operator surface. `man agentd` for the full reference.

**Building the `.deb` from source** (Debian 13 host with `cargo`, `cargo-deb`, and `pandoc`):

```bash
./scripts/setup-hooks.sh          # activate in-tree git hooks (once per clone)
cargo build --release -p agentd --bin agentd
cargo build --release -p aaos-backend-linux --bin aaos-agent-worker
./packaging/build-man-page.sh
cargo deb -p agentd --no-build
# target/debian/aaos_*.deb is the installable artifact.
```

`setup-hooks.sh` wires in a gitleaks-based pre-commit secret scan. Install `gitleaks` first (`apt install gitleaks`) or the hook no-ops with a warning.

**On any host with Docker** (legacy Bootstrap-Agent path — no role catalog needed):

```bash
git clone https://github.com/Joncik91/aaOS.git && cd aaOS
DEEPSEEK_API_KEY="sk-..." ./run-aaos.sh "fetch HN top 5 and write a summary to /output/summary.txt"
cat output/summary.txt
```

The launcher starts the container with a live dashboard. Cross-run memory, alternate providers, and sending goals to a running container are documented in [`docs/docker.md`](docs/docker.md).

## What aaOS provides

- **Computed orchestration** — Planner + PlanExecutor walks a role-based subtask DAG, running independents in parallel. Bootstrap Agent path (single-LLM orchestrator) available as fallback.
- **Capability-based security** — Runtime-issued handle-opaque tokens. Zero-capability default. Path canonicalization (including symlinks). Narrowable-only on delegation. [Details](docs/architecture.md#capability-security-model).
- **Pluggable agent backend** — `InProcessBackend` default; opt-in `NamespacedBackend` with Linux user/mount/IPC namespaces + Landlock + seccomp, verified on Debian 13 / kernel 6.12.43.
- **Coding surface** — `file_read(offset, limit)`, `file_edit`, `file_list`, `grep` (ripgrep-backed), `cargo_run` (subcommand-allowlisted), `git_commit` (subcommand-allowlisted). Matches the Claude Code / Cursor working subset.
- **Managed context + episodic memory** — Transparent summarization when context fills; per-agent semantic memory (cosine over embeddings) with SQLite persistence.
- **Observability** — 26 audit event kinds. Streamed to stdout (Docker) / journald (.deb) / any NDJSON subscriber via `BroadcastAuditLog`.
- **Multi-provider LLM** — DeepSeek, Anthropic, any OpenAI-compatible API. Concurrency-limited and budget-tracked.
- **AgentSkills support** — The [open standard](https://agentskills.io) by Anthropic: folders with `SKILL.md`, progressive disclosure via `skill_read`. 21 bundled skills from [addyosmani/agent-skills](https://github.com/addyosmani/agent-skills); drop your own into `.agents/skills/` or set `AAOS_SKILLS_DIR`.
- **Bidirectional MCP** — Behind `--features mcp`. **Client:** external MCP servers configured in `/etc/aaos/mcp-servers.yaml` (stdio or HTTP) register their tools into the runtime as `mcp.<server>.<tool>`, governed by the same capability model as built-ins. **Server:** a loopback-only HTTP+SSE listener on `127.0.0.1:3781` exposes `submit_goal`, `get_agent_status`, `cancel_agent` so Claude Code, Cursor, or any MCP client can delegate goals to aaOS.

A full feature list lives in [`docs/architecture.md`](docs/architecture.md); the tool surface is cataloged at [`docs/tools.md`](docs/tools.md).

## Agent manifest

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
  - "tool: memory_store"
  - "tool: memory_query"
approval_required:
  - file_write
memory:
  context_window: "128k"
  max_history_messages: 200
  episodic_enabled: true
budget_config:
  max_tokens: 1000000
  reset_period_seconds: 86400
```

## Design principles

1. **Agent-native, human-optional.** Runtime boots into an agent process. Humans provide goals, not instructions.
2. **Capability-based security.** Agents start with zero capabilities. Runtime-issued handle-opaque tokens replace permissions.
3. **Structured communication.** Schema-validated agent messages over a typed bus, not raw byte pipes. Interop with the broader ecosystem is via the Model Context Protocol — both as a client (consuming external MCP servers as tool sources) and as a server (exposing aaOS goals to MCP clients).
4. **Observable by default.** Every tool invocation and lifecycle event produces an audit event.
5. **Substrate-agnostic abstractions.** `AgentServices` is an ABI. Process-backed today, MicroVM or microkernel later — tools and manifests unchanged.

## Documentation

- [Architecture](docs/architecture.md) — Stack, capability model, agent backends, audit trail
- [Tools](docs/tools.md) — Built-in tool catalog with capability requirements
- [API](docs/api.md) — JSON-RPC method reference
- [Docker deployment](docs/docker.md) — Container path, persistent memory, multi-goal sessions
- [Roadmap](docs/roadmap.md) — Phase-by-phase path from runtime to real kernel
- [Reflection log](docs/reflection/README.md) — Runs where aaOS reads its own code and proposes changes
- [Patterns](docs/patterns.md) — Cross-cutting lessons distilled from the log
- [Ideas](docs/ideas.md) — Deferred work, with the signals that would prompt reconsideration
- [Distribution architecture](docs/distribution-architecture.md) — The Debian-derivative target

## License

[Apache License 2.0](LICENSE)
