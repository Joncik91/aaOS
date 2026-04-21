# AGENTS.md

Also read `CLAUDE.md` — it contains machine-specific context, deployment topology, and operational runbooks complementary to this file.

## Workspace structure

9 Rust crates under `crates/`, resolver v2. `agentd` is the binary entrypoint; the rest are libraries.

| Crate | Role |
|---|---|
| `aaos-core` | Shared types: agent IDs, capability tokens, audit events, manifests, `AgentBackend` trait |
| `aaos-runtime` | Agent lifecycle, in-process backend, context management, plan executor, role catalog |
| `aaos-ipc` | JSON-RPC message types, schema validation |
| `aaos-tools` | Tool implementations (file, grep, git, cargo, web, memory, skills), `WorkerHandle` for out-of-process calls |
| `aaos-llm` | `LlmClient` trait, Anthropic + OpenAI-compatible providers, `AgentExecutor` tool-use loop |
| `aaos-memory` | `MemoryStore` trait, in-memory + SQLite backends, cosine similarity, Ollama embeddings |
| `aaos-backend-linux` | `NamespacedBackend`: user/mount/IPC namespaces, Landlock, seccomp-BPF sandboxing |
| `aaos-mcp` | MCP client (external servers) + server (loopback JSON-RPC/SSE). Own version cadence (0.1.0) |
| `agentd` | Daemon binary + CLI (`run`, `submit`, `list`, `status`, `stop`, `logs`, `roles`, `configure`) |

## Commands

```bash
# Lint + typecheck (CI runs exactly these — keep main green)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings

# Tests (default suite — no host prerequisites needed)
cargo test --workspace

# Ignored tests (need ripgrep, git, Linux kernel primitives)
cargo test --workspace -- --ignored

# Namespaced-backend tests (feature-gated, also --ignored)
cargo test -p agentd --features namespaced-agents -- --ignored

# MCP integration tests (builds fixture binary)
cargo test -p aaos-mcp -- --ignored

# Feature compile checks (CI runs these to catch regressions)
cargo check -p agentd --features mcp
cargo check -p agentd --features namespaced-agents

# Build worker binary (needed before namespaced tests)
cargo build -p aaos-backend-linux --bin aaos-agent-worker

# Build .deb (one step — builds both binaries, strips, packs)
./packaging/build-deb.sh

# Man page (needs pandoc)
./packaging/build-man-page.sh

# Activate git hooks (once per clone)
./scripts/setup-hooks.sh
```

Run order when making changes: `cargo fmt --all` → `cargo clippy` → `cargo test --workspace`.

## Feature flags

Only `agentd` has features; both are **off by default**:

- `mcp` — MCP client + server (`dep:aaos-mcp`)
- `namespaced-agents` — Linux namespace sandboxing (`dep:aaos-backend-linux`)

The shipped `.deb` enables both via `--features mcp,namespaced-agents`. Runtime selection: `AAOS_DEFAULT_BACKEND=namespaced`.

## Testing conventions

- Tests are co-located inline (`#[cfg(test)] mod tests` blocks), not in separate directories.
- `#[tokio::test]` for async, `#[test]` for sync. Both used extensively.
- `#[ignore]` gates tests needing host tools (ripgrep, git, cargo) or Linux kernel features (namespaces, Landlock). CI runs these in a separate job.
- Live API tests (in `agentd/tests/`) use early-return skip: check for `ANTHROPIC_API_KEY`, print skip, `return`. Not `#[ignore]`.
- Namespace tests call `should_skip_namespaced_test()` → `probe_mount_capable()` with descriptive skip messages.
- Temp dirs: `tempfile::TempDir`. Mock LLM: `Mutex<Vec<LlmResult<CompletionResponse>>>` with canned responses.
- Only test fixture: `crates/aaos-mcp/tests/fixtures/echo-mcp-server/` (minimal MCP stdio server for transport tests).

## Pre-commit hooks

`scripts/setup-hooks.sh` sets `core.hooksPath = .githooks/`. The pre-commit hook runs:

1. **gitleaks protect** — blocks secrets in staged changes. No-op if gitleaks not installed.
2. **S2D review** (`scripts/s2d-review.sh`) — spec-to-diff checker. Only activates for plan-driven commits (`Plan:` / `Spec:` trailer, or `docs/phase-*-plan.md` in diff). Needs `ANTHROPIC_API_KEY`. Override: `S2D_DISABLE=1`.

## Build and packaging

- `packaging/build-deb.sh` builds `aaos-agent-worker` and `agentd` separately, strips both, then packs with `cargo deb --no-build`. Do not use `cargo deb` alone — it doesn't build the worker binary.
- `aaos-mcp` is on its own 0.1.0 cadence — don't bump it during workspace version updates.
- No `rust-toolchain.toml`. CI uses `dtolnay/rust-toolchain@stable` (latest stable at job time). Run `rustup update stable` locally to stay in sync, especially before releases.

## Architecture notes

- **Two-phase orchestration**: Planner (LLM) emits structured Plan → PlanExecutor (Rust) walks the DAG, running independent subtasks in parallel.
- **Two agent backends**: `InProcessBackend` (default) and `NamespacedBackend` (feature-gated, Linux namespace sandboxing). Selected via `AAOS_DEFAULT_BACKEND`.
- **`AgentServices` trait** is the substrate-agnostic ABI — process-backed today, MicroVM later.
- **API key scrubbing**: daemon zeros keys from both the libc env table and `/proc/self/environ` after loading them into owned structs.
- **Audit trail**: 31 event kinds, streamed to stdout (Docker) / journald (.deb) / NDJSON subscribers.

## Style

- `clippy -D warnings` is enforced in CI. New warnings fail the build.
- Auto-fix: `cargo clippy --workspace --all-targets --fix --allow-dirty --allow-staged`, then `cargo fmt --all`.
- Run `cargo fmt --all` after any subagent-generated Rust before committing.
