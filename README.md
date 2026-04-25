# aaOS — Agent Runtime

[![CI](https://github.com/Joncik91/aaOS/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/Joncik91/aaOS/actions/workflows/ci.yml)

**An agent-first runtime where AI agents are native processes, capabilities replace permissions, and the system is designed for autonomy — not human interaction.**

A working agent runtime on Linux that can read and modify source code — including its own. 9 Rust crates, ~37,600 lines, 631 tests (619 + 12 `#[ignore]`-gated on host prereqs such as ripgrep, git, Linux kernel primitives, or a child toolchain — exercised on CI, optional locally). Ships as a Debian `.deb` with an operator CLI; current release `v0.1.4` is built inside a `debian:13` container with `mcp,namespaced-agents` features baked in and attached at [Releases](https://github.com/Joncik91/aaOS/releases). Orchestration auto-detects per goal: a cheap-LLM classifier routes structured goals with independent parallelisable subtasks to a **Planner + PlanExecutor** multi-node DAG, and open-ended investigation goals to a **single multi-turn generalist** via an inline 1-node plan — both paths through the unified PlanExecutor. Operators can override with `--orchestration plan|persistent`. The capability model, audit trail, and substrate-agnostic ABI have survived 43 self-reflection runs plus two full fresh-droplet QA passes — initial v0.0.1 pass finding six bugs, verification v0.0.2 pass confirming they're all closed and surfacing Bug 7 (fixed in v0.0.3), and the v0.0.3 self-reflection run finding Bug 8 (fixed in v0.0.4); see [`docs/reflection/`](docs/reflection/README.md).

The long-term target is a **Debian derivative** where aaOS runs as the system orchestrator (Home Assistant OS for agents), with Landlock + seccomp enforcing capability tokens at the kernel layer. Runtime-side tool confinement already shipped via a `NamespacedBackend`: agent tool calls execute inside a per-agent Linux user/mount namespace with Landlock + seccomp active and capability tokens forwarded across a broker stream. The `AgentServices` trait remains a substrate-agnostic ABI — process-backed today, MicroVM-per-agent later, microkernel only if a customer demands formally-verified boundaries. The programming model is the product; the substrate is replaceable.

See [Architecture](docs/architecture.md) for the full stack, [Roadmap](docs/roadmap.md) for where it's going, and [`docs/reflection/`](docs/reflection/README.md) for what each self-reflection run surfaced and fixed.

## Quick start

**On Debian 13:**

```bash
# 1. Install the .deb (download from Releases, or build from source — see below).
sudo apt install ./aaos_0.1.4-1_amd64.deb

# 2. Join the aaos group so your shell can talk to the daemon socket.
#    Log out and back in for group membership to take effect.
sudo adduser $USER aaos

# 3. Configure an LLM provider. Interactive; writes /etc/default/aaos
#    mode 0600 root:root and restarts the daemon.
sudo agentd configure                  # or: --provider anthropic
# Non-interactive alternative (Ansible/cloud-init):
#   sudo agentd configure --key-from-env DEEPSEEK_API_KEY

# 4. Send a goal.  systemd already started agentd during step 1.
agentd submit "fetch HN top 5 stories and write a summary"
```

The CLI streams audit events live as the Planner decomposes the goal and the PlanExecutor walks the resulting DAG — fetchers run in parallel, writers read their outputs. `agentd list|status|stop|logs|roles|configure` cover the operator surface. `man agentd` for the full reference.

**Building the `.deb` from source** (Debian 13 host with `cargo`, `cargo-deb`, and `pandoc`):

```bash
./scripts/setup-hooks.sh          # activate in-tree git hooks (once per clone)
./packaging/build-man-page.sh     # agentd(1) man page via pandoc
./packaging/build-deb.sh          # builds both binaries, strips, packs with --features mcp,namespaced-agents
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

- **Computed orchestration** — Planner + PlanExecutor walks a role-based subtask DAG, running independents in parallel. Auto-classifier routes open-ended investigation goals to a 1-node direct plan (single multi-turn generalist, no Planner call). Both paths use the unified PlanExecutor; each subtask is a full multi-turn agent loop.
- **Capability-based security** — Runtime-issued handle-opaque tokens. Zero-capability default. Path canonicalization (including symlinks). Narrowable-only on delegation. [Details](docs/architecture.md#capability-security-model).
- **Pluggable agent backend** — `InProcessBackend` for trusted hosts; `NamespacedBackend` with Linux user/mount/IPC namespaces + Landlock + seccomp. The shipped `.deb` (v0.0.2+) enables namespaced-by-default when the `postinst` probe detects Landlock in `/sys/kernel/security/lsm` + unprivileged user namespaces — Debian 13 stock kernel (6.12+) has both. Under namespaced, every plan-executor subtask's filesystem + compute tools execute inside the worker with capability tokens forwarded across the broker stream. Verified under 5-way concurrent load and fresh-droplet QA. Network + subprocess tools stay daemon-side by design (see [architecture](docs/architecture.md#worker-side-tool-confinement)).
- **Coding surface** — `file_read(offset, limit)`, `file_edit`, `file_list`, `grep` (ripgrep-backed), `cargo_run` (subcommand-allowlisted), `git_commit` (subcommand-allowlisted). Matches the Claude Code / Cursor working subset.
- **Managed context + episodic memory** — Transparent summarization when context fills; per-agent semantic memory (cosine over embeddings) with SQLite persistence.
- **Observability** — 33 audit event kinds. Streamed to stdout (Docker) / journald (.deb) / any NDJSON subscriber via `BroadcastAuditLog`. CLI shows `[worker]` / `[daemon]` tag per tool line so confinement is visible in-situ.
- **Multi-provider LLM** — DeepSeek, Anthropic, any OpenAI-compatible API. Concurrency-limited and budget-tracked.
- **AgentSkills support** — The [open standard](https://agentskills.io) by Anthropic: folders with `SKILL.md`, progressive disclosure via `skill_read`. 21 bundled skills from [addyosmani/agent-skills](https://github.com/addyosmani/agent-skills) ship at `/usr/share/aaos/skills/` in the `.deb`; operator skills go in `/etc/aaos/skills/`; `AAOS_SKILLS_DIR` appends additional roots.
- **Bidirectional MCP** — Built into the shipped `.deb` by default (v0.0.2+). **Client:** external MCP servers configured in `/etc/aaos/mcp-servers.yaml` (stdio or HTTP; a starter template lives at `/etc/aaos/mcp-servers.yaml.example`) register their tools into the runtime as `mcp.<server>.<tool>`, governed by the same capability model as built-ins. **Server:** a loopback-only HTTP+SSE listener on `127.0.0.1:3781` exposes `submit_goal`, `get_agent_status`, `cancel_agent` so Claude Code, Cursor, or any MCP client can delegate goals to aaOS.
- **First-boot UX** — `sudo agentd configure` prompts for a DeepSeek or Anthropic API key, atomically writes `/etc/default/aaos` mode 0600 root:root, and restarts the daemon. Non-interactive mode via `--key-from-env VAR` for Ansible / cloud-init. Daemon's startup log points at the command when keys are missing.
- **Operator ergonomics** — Workspace GC (`AAOS_WORKSPACE_TTL_DAYS`, default 7) prunes per-run scratch dirs automatically. Broken role YAMLs surface as ERROR-level logs at startup naming the exact file + parse column. Worker-launch back-pressure (`AAOS_NAMESPACED_CONCURRENT_LAUNCHES`, default 2) keeps confinement active under bursty load. Invalid LLM keys fail fast with a named error, not a silent hang.

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

- [Changelog](CHANGELOG.md) — Per-release summary of what changed, with the pre-v0.0.1 history captured under `[0.0.0]`
- [Architecture](docs/architecture.md) — Stack, capability model, agent backends, audit trail
- [Tools](docs/tools.md) — Built-in tool catalog with capability requirements
- [API](docs/api.md) — JSON-RPC method reference
- [Docker deployment](docs/docker.md) — Container path, persistent memory, multi-goal sessions
- [Roadmap](docs/roadmap.md) — Shipped history + active milestones + research branch
- [Reflection log](docs/reflection/README.md) — Runs where aaOS reads its own code and proposes changes
- [Patterns](docs/patterns.md) — Cross-cutting lessons distilled from the log
- [Ideas](docs/ideas.md) — Deferred work, with the signals that would prompt reconsideration
- [Distribution architecture](docs/distribution-architecture.md) — The Debian-derivative target

## License

[Apache License 2.0](LICENSE)
