# MCP integration — bidirectional, feature-gated *(2026-04-18)*

**Integration commits:** `87c9aa1` through `400f76b` (24 commits on `main`). New crate `crates/aaos-mcp`, new feature flag `agentd --features mcp`, new config file `/etc/aaos/mcp-servers.yaml`, new reference entry in `README.md` + `docs/api.md` + `docs/architecture.md` + `docs/tools.md` + `docs/distribution-architecture.md` + `docs/roadmap.md`.

## Setup

- **Memory state:** fresh planning session. No prior MCP work in-tree.
- **Goal:** close the "runtime tool authoring via MCP" gap from `ideas.md`. Both directions — client (consume external MCP servers as tool sources) and server (expose aaOS goals to external MCP clients) — in one spec.
- **Workflow:** `superpowers:brainstorming` → `superpowers:writing-plans` → `superpowers:subagent-driven-development`. 14 tasks, fresh implementer subagent per task, two-stage review (spec compliance then code quality) between each. Controller never wrote code directly — all implementation delegated, all reviews run as dedicated agents.
- **Verification:** ephemeral DigitalOcean droplet (`46.101.202.249`, destroyed after), built release with `--features mcp`, drove the endpoint with `curl` + real DeepSeek key.

## What worked

- **Two-stage review caught real bugs, not nitpicks.** The code-quality reviewer flagged genuine correctness gaps every time:
  - Task 8: `McpClient` never called `spawn_reconnect_loop`, so any transport blip would leave sessions permanently unhealthy.
  - Task 8 follow-up: `McpSession::handshake_with(new_transport)` marked the session healthy but didn't actually swap `self.transport`, so reconnect would flip healthy → unhealthy on the first subsequent `call()`. Fixed by wrapping `transport` in `RwLock`.
  - Task 10: `JsonRpcRequest.id` typed as `u64` — any MCP client using string IDs (which is spec-legal and common) would get HTTP 422 from axum's extractor. Fixed to `serde_json::Value`.
  - Task 10: malformed JSON body returned HTTP 422 plain-text instead of a JSON-RPC `-32700 Parse error`. Fixed with `Result<Json<T>, JsonRejection>` extractor.
  - Task 11: test race — `tokio::spawn` returned before axum's accept loop started; first curl could race. Fixed with a TCP-readiness poll.
  - Task 11: `assert!(resp["error"].is_null())` was vacuous — `skip_serializing_if = Option::is_none` means the field is absent on success, which deserializes to `Value::Null` regardless. The assertion couldn't fail.
  - Task 12: echo-server test used hardcoded `./target/debug/echo-mcp-server` path but `cargo build` respects `CARGO_TARGET_DIR` env override; would break under `cargo-nextest` or CI variants. Fixed by setting `CARGO_TARGET_DIR` explicitly.
- **Subagent-driven loop stayed clean across 14 tasks + ~20 review rounds.** Controller context never filled with task-internal detail; all implementation state lived in the fresh subagents. Same loop that shipped the CLI and computed orchestration.
- **Capability model needed zero new variants.** External MCP tools register as `mcp.<server>.<tool>` strings into the existing `ToolRegistry`; a manifest grants them via existing `Capability::ToolInvoke { tool_name }`. The proxy hides everything else. Remote MCP server's own input validation handles the rest.
- **Loopback-only server with no auth was the right call.** Simpler than baking in a second auth stack; operator-responsibility for remote access (SSH tunnel, Tailscale) mirrors how `agentd.sock` is already handled. No pressure to change.
- **Silent-disable on missing config.** `/etc/aaos/mcp-servers.yaml` absent → MCP does nothing, daemon behaves identically to a non-mcp build. No startup errors, no visible changes unless you opt in. Matches how `/etc/aaos/roles/` already falls back.

## What the run exposed

- **Naming collision with `aaos-ipc`.** The codebase had already branded its internal JSON-RPC bus as "MCP" (`McpMessage`, `McpResponse`) long before real Model Context Protocol entered scope. Adding a real MCP implementation meant the two share a namespace. Docs now disambiguate: `aaos-ipc` is the legacy internal bus, `aaos-mcp` is the real protocol. User flagged this during doc review — worth naming explicitly so future contributors don't conflate them.
- **CI needed two gates, not one.** `cargo check -p agentd --features mcp` in `check-lint` catches feature-off vs feature-on compile drift; `cargo test -p aaos-mcp -- --ignored` in `test-ignored` runs the stdio echo-server e2e. `cargo test --workspace` on the default build already covers the lib tests.
- **`submit_goal` returning the singleton bootstrap `AgentId`.** When called twice in a row, you get the same `run_id` back. That's correct — there's one persistent bootstrap per daemon, and goals queue to it — but the SSE stream filtering by `agent_id` can't distinguish two concurrent goals to the same bootstrap. Not blocking; noted for future if multi-bootstrap lands.
- **The `role` parameter is advertised but silently ignored.** Handler emits `tracing::warn!` when `role` is provided, but there's no routing yet. Left as a contract-disclosure line in the logs until a second role type forces the split.
- **Droplet e2e validated the control flow, not the happy-path output.** Bootstrap agent correctly received the goal, called DeepSeek, tried `file_write` on five different paths, all correctly denied by the capability system. That's the integration passing — the LLM's choice of paths outside its workspace is a separate story about prompt + default manifest, not MCP. Worth flagging so "it couldn't write a file" isn't confused with "MCP broken."

## What shipped

- **`crates/aaos-mcp`** — new library crate. `src/types.rs` (JSON-RPC wire types, `McpError`, `McpToolDefinition`), `src/config.rs` (`McpConfig::load` from `/etc/aaos/mcp-servers.yaml`), `src/client/` (`McpTransport` trait + `HttpTransport` + `StdioTransport`; `McpSession` with reconnect loop; `McpToolProxy` implementing `Tool`; `McpClient::connect_and_register`), `src/server/` (`McpServerBackend` trait, `McpServer` axum listener, `handle_jsonrpc` + `handle_sse` + `dispatch_tool_call`, `audit_sse_stream`). Unit tests in `src/`, integration tests in `tests/server_integration.rs`, ignored stdio echo-server e2e in `tests/fixtures/echo-mcp-server/`.
- **`crates/agentd`** — new optional dep `aaos-mcp`, new feature `mcp`, new module `mcp_backend.rs` implementing `McpServerBackend` for `Server` (delegates to `ensure_bootstrap_running` / `route_goal_to` / `registry.stop_sync` / `broadcast_audit.subscribe`), startup wiring in `main.rs` under `#[cfg(feature = "mcp")]`.
- **`crates/aaos-core`** — `CoreError::ToolUnavailable(String)` for downed MCP sessions; `FromStr for AgentId` parsing UUID strings for `run_id` handling.
- **Workspace** — `axum = "0.8"`, `tokio-stream`, `futures-util` added to `[workspace.dependencies]`.
- **CI** — `cargo check -p agentd --features mcp` gate + `cargo test -p aaos-mcp -- --ignored` step. Both required.
- **`packaging/debian/postinst`** — `mkdir -p /etc/aaos` so operators can drop `mcp-servers.yaml` into a place that exists.
- **Docs** — README bullet + principle-3 fix, new architecture.md section 5b, api.md MCP Server API section, roadmap.md Phase F-a follow-up + strike-through of the deferred item, tools.md external-tools note, distribution-architecture.md config + capability-table row.

## Cost

Not directly measured. The subagent-driven implementation spanned ~2 hours wall-clock across ~60 subagent dispatches (14 implementers + ~45 reviewer runs). Dominant cost was code quality reviewers using Sonnet-class models on small diffs. Droplet build was ~3m21s on a 2vCPU/4GB droplet. Total droplet hours < 1, billed < $0.05. DeepSeek key rotated post-session.

## Lessons worth lifting

- **Two-stage review (spec + quality) catches more than one-stage.** Spec compliance alone would have accepted Task 8 — the code did what the plan said. Quality review found the reconnect loop was never wired. Spec review alone would have accepted Task 10 — all three tools were implemented. Quality review found the `u64` id bug. The stages check different things and both matter.
- **"The implementer said DONE" is not enough.** Task 8 reported DONE twice (initial + fix) and Task 10 DONE once; each time a subsequent review found a real issue. The cost of running the review is tiny relative to the cost of shipping a broken reconnect loop to production.
- **Subagent reviewers find things a fresh pair of eyes finds.** The reviewer agents hadn't read the plan and hadn't watched the implementer build it. That absence of context is the point — they read the code the way an outside contributor would. See `patterns.md` for the broader form of this.
- **Naming collisions with legacy terminology need explicit disambiguation in docs.** If the codebase had called the internal bus `aaos-bus` instead of `McpMessage`/`McpResponse`, the new MCP work would have been notation-free. Worth a grep-for-confusion pass when introducing anything whose name overlaps an existing internal concept.
