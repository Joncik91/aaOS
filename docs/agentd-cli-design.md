# `agentd` CLI Design

**Status:** design, approved 2026-04-15. Implementation plan follows.

**Scope:** The client half of the `agentd` binary — the subcommands an operator runs after `apt install ./aaos_*.deb` to drive a live daemon. Today those subcommands are stubs that print "not yet implemented"; this design replaces them with a working operator CLI.

**Non-goals for this iteration:**
- Approval commands (`approve`/`deny`). The approval queue exists on the server but no runtime code invokes `request_approval`, so approvals never block in practice. Add when that changes.
- Direct tool invocation from the shell (`tool.invoke`). Debugging affordance, not operator flow.
- Manifest-spawn from a file path (`agentd spawn <manifest.yaml>`). Matches no real workflow today; Bootstrap is the entry point.
- Routing flag (`--to <agent>`). Bootstrap is the product story; add the flag when a custom-manifest use case actually lands.

## Principle

One binary, two halves. `agentd` is both the daemon (`agentd run ...`) and the client. The operator never sees that split — they just type `agentd submit "..."` and the binary figures out to open the socket.

## Scope confirmation (the five commands)

```
agentd submit <goal>              Send a goal to Bootstrap, stream output, exit when done.
  -v, --verbose                   Show every audit event (default: operator view only).
  --socket <path>

agentd list                       List running agents. Columns: id, name, state, uptime.
  --json                          Machine-readable output.
  --socket <path>

agentd status <agent_id>          Show detail for one agent. Id, name, state, parent,
                                  capabilities, token usage, last event time.
  --json
  --socket <path>

agentd stop <agent_id>            Terminate an agent. Exits after the daemon confirms.
  --socket <path>

agentd logs <agent_id>            Attach to an already-running agent's audit stream.
                                  Same event filter as submit; same Ctrl-C detach.
  -v, --verbose
  --socket <path>
```

Global defaults: `--socket` defaults to `/run/agentd/agentd.sock`. Agent id arguments accept any unique prefix (8-char minimum, ambiguous → error listing candidates).

## Architecture

```
┌────────────┐       Unix socket        ┌────────────┐
│ agentd     │  /run/agentd/agentd.sock │ agentd     │
│ (CLI mode) │ ───── JSON-RPC ────────> │ (daemon)   │
│            │ <── NDJSON event stream ─│            │
└────────────┘                          └────────────┘
                                              │
                                         ┌────▼─────┐
                                         │ Audit    │
                                         │ broadcast│
                                         └──────────┘
```

Three pieces:

1. **CLI module** in `crates/agentd/src/cli/` — one file per subcommand (submit.rs, list.rs, status.rs, stop.rs, logs.rs), a shared `client.rs` for the JSON-RPC transport, an `output.rs` for event filtering and formatting.

2. **Server streaming method** `agent.submit_streaming` — holds the connection open after accepting a goal, writes NDJSON audit events to the wire as they fire, closes with a final `{"kind":"end","exit_code":...}` frame. `agent.logs_streaming` does the same minus the submit, for the `logs` subcommand's attach case.

3. **`BroadcastAuditLog`** — wraps the existing `StdoutAuditLog` with a tokio broadcast channel. Per-connection subscribers filter by agent id (and its descendants via parent chain). Kept narrow: broadcast capacity is a config knob; subscribers that fall behind get a lag event, not an abort.

Bounded changes: no touch to `AgentServices`, `Tool`, or the manifest format. Only `agentd` changes. Audit event kinds (22 of them) are unchanged.

## Subcommand behavior

### `submit <goal>`

1. Open `UnixStream`.
2. Send JSON-RPC `agent.submit_streaming` with `{"goal": "..."}`.
3. Read NDJSON frames from the socket line by line. Each frame is one audit event or the terminating `end` frame.
4. For each frame: filter by verbose/default view, format, write to stdout. Colorize if stdout is a tty.
5. On `end` frame, close socket, exit with the code in the frame (0 = success, 1 = agent-reported failure).
6. On SIGINT: print "detaching — agent continues; use `agentd stop <id>` to terminate" (once), close socket, exit 4. A second SIGINT within 2 seconds kills the CLI immediately with no cleanup.

The agent keeps running after detach. Re-attach via `agentd logs <id>`.

### `list`

Single JSON-RPC `agent.list` call. Tabulate (default) or dump JSON (`--json`). Columns: id (8-char prefix), name, state (`Running` / `Paused` / `Stopped`), uptime (`HH:MM:SS`).

### `status <agent_id>`

Resolve prefix to full id (via `agent.list` + prefix match), then `agent.status` with full id. Print name, state, parent agent (or `—`), capabilities (one per line), token usage (input/output), last event time. `--json` for machine-readable.

### `stop <agent_id>`

Resolve prefix, `agent.stop`, wait for confirmation, print `stopped <id>`. No streaming.

### `logs <agent_id>`

Open socket, call `agent.logs_streaming` with full (resolved) agent id. Same read loop and SIGINT behavior as `submit`. No `end` frame unless the agent actually terminates; Ctrl-C is the normal way out.

## Output format

### Operator view (default)

One line per event. Format:
```
[HH:MM:SS] <agent-name>   <what happened>
```

Example:
```
[12:03:47] bootstrap   spawned fetcher (file_write:/data/*, web_fetch)
[12:03:49] fetcher     tool: web_fetch https://news.ycombinator.com
[12:03:51] fetcher     tool: file_write /data/hn-raw.txt
[12:03:52] bootstrap   spawned writer (file_read:/data/*, file_write:/output/*)
[12:04:03] writer      tool: file_write /output/summary.md
[12:04:05] bootstrap   complete (9.2k tokens, 38s)

Wrote /output/summary.md
```

**Visible event kinds:** `AgentSpawned`, `ToolInvoked` (name + first arg only), `AgentCompleted`, `AgentFailed`, `BudgetExceeded`, `CapabilityDenied`, plus the final output path surfaced from Bootstrap's completion payload.

**Hidden by default:** `UsageReported`, `CapabilityNarrowed`, `ToolResult` (the tool name already appeared on `ToolInvoked`), `ContextSummarized`, `MemoryWritten`, `MemoryQueried`, `HumanApprovalRequested`, `MessageRouted`, and any 6.12-era debug events.

Colors (tty only): agent names dim, `complete` green, `failed`/`denied`/`exceeded` red, tool names cyan. No color when piped.

### Verbose (`-v`)

One NDJSON line per raw audit event. Timestamp, agent_id (full), kind, payload. No filtering. Suitable for `| jq`.

### `--json` (list, status)

Structured output matching the server's JSON-RPC response shape. No decoration.

## Errors and exit codes

Exit codes:
- `0` — success
- `1` — agent-reported failure (Bootstrap said "couldn't do it")
- `2` — usage error (bad flag, missing arg, ambiguous/unknown agent id)
- `3` — daemon unreachable (socket missing, permission denied, daemon down)
- `4` — operator interrupt (SIGINT after detach grace period)

Error message format — no stack traces, no Rust type names:
```
error: daemon not reachable at /run/agentd/agentd.sock

  Is agentd running?   systemctl status agentd
  Are you in the aaos group?   groups
```

The "here's what to check" line maps common errors to the fix. Five cases written out:
- Socket missing → `systemctl status agentd`, check journal
- EACCES → `groups` + `adduser $USER aaos`
- JSON-RPC parse error (wire mismatch) → "version skew: upgrade CLI or daemon"
- Unknown agent id prefix → list candidates from `agent.list`
- Broken pipe mid-stream → "daemon restarted; try again"

## Auth and packaging changes

- `postinst` adds `addgroup --system aaos || true` (idempotent; documents intent separately from `adduser --system --group`).
- Socket mode stays `0660` (aaos:aaos). Group membership unlocks access.
- README "Quick start" section replaces the Docker-only flow with the `.deb` + `adduser $USER aaos` + `agentd submit` path.
- `packaging/agentd.1.md` → man page built via `pandoc -s -t man`. `cargo-deb` installs at `/usr/share/man/man1/agentd.1.gz`. Every subcommand, every flag, exit codes, socket path, environment variables, files, examples.
- `agentd <cmd> --help` — clap derive with `#[command(about, long_about)]` on every subcommand. `submit --help` must include one `agentd submit "..."` example.

**Docs verification:** a new operator with only the `.deb` and the README reaches a successful `agentd submit` output within 5 minutes. Fails → doc bug.

## Testing

**Unit tests** (per-subcommand, no socket):
- clap argument parsing (valid + invalid flags)
- event-kind-to-operator-visible filter
- prefix disambiguation (unique, ambiguous, none)
- exit-code mapping for each error branch
- output formatting given a fixed event sequence

**Integration tests** (`crates/agentd/tests/cli_integration.rs`):
- `submit` receives streamed events and exits 0 on completion
- `list` returns the running agent
- `status <id>` returns expected fields
- `stop <id>` stops a running agent
- SIGINT on `submit` detaches cleanly; daemon stays up; second submit still works
- `--json` on `list` round-trips through the expected struct
- Unknown agent id prefix → exit 2 with "ambiguous" or "not found"

Tests spin up `agentd` in a tokio task with a temp socket path and drive the CLI in-process (not as a subprocess) — no process isolation cost, clean `tokio::join!` on the assertion side.

**Server-side streaming tests** (in `server.rs` test module):
- `agent.submit_streaming` subscribes, receives expected event set in order, writes `end` frame on completion
- `agent.logs_streaming` filters to the requested agent id + descendants
- Subscriber disconnect mid-stream doesn't crash the server
- Broadcast lag produces a lag event, not an abort

## Droplet verification (run after implementation)

1. Rebuild `.deb` on the Debian 13 droplet; `apt install`.
2. Create a non-root user; `adduser testop aaos`.
3. Set `/etc/default/aaos` with a real DEEPSEEK_API_KEY; `systemctl restart agentd`.
4. As `testop`:
   - `agentd submit "say hello three times"` streams events and exits 0.
   - `agentd list` shows Bootstrap + the ephemeral child if any.
   - `agentd logs <bootstrap-id>` attaches; another `submit` in a second terminal shows events in both streams.
   - `agentd stop <id>` returns promptly; `list` no longer shows it.
5. `man agentd` renders.
6. `agentd submit --help` shows the example line.

## Out of scope (deferred, not "rejected")

- Shell completions (`bash-completion`, `zsh`, `fish` files)
- Batch/pipeline mode (`agentd submit - < goals.txt`)
- Output redirection (`--output <path>`, currently stdout only)
- CLI config file for personal defaults
- Structured output format other than JSON (YAML, TOML)
- Approval commands (add when approvals actually fire)
- Direct tool invocation (debugging surface, not operator surface)

Each of these is cheap to add later. Shipping without them forces us to hear what operators actually want before we guess.
