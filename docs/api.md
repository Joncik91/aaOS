# aaOS JSON-RPC API

Reference for the JSON-RPC 2.0 methods `agentd` serves over its Unix socket.
Socket path: `/run/agentd/agentd.sock` when installed as a `.deb`,
`/tmp/aaos-sock/agentd.sock` in the Docker deployment. The operator CLI
(`agentd submit|list|status|stop|logs|roles`) wraps the streaming methods
below; for day-to-day operation see `man agentd`.

## Methods

| Method | Description |
|--------|-------------|
| `agent.spawn` | Spawn an agent from a YAML manifest |
| `agent.stop` | Stop a running agent |
| `agent.list` | List all running agents |
| `agent.status` | Get status of a specific agent |
| `agent.run` | Run an existing agent with a message |
| `agent.spawn_and_run` | Spawn and run in one call |
| `agent.submit_streaming` | Send a goal; stream audit events as NDJSON until `end` frame |
| `agent.logs_streaming` | Attach to a specific agent's audit stream as NDJSON; no end frame unless the agent terminates |
| `tool.list` | List registered tools |
| `tool.invoke` | Invoke a tool on behalf of an agent |
| `approval.list` | List pending approval requests |
| `approval.respond` | Approve or deny a pending request |

## Example — send a goal

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"agent.submit_streaming",
       "params":{"goal":"fetch HN top 5 and summarize"}}' | \
  nc -U /run/agentd/agentd.sock
```

The response is a stream of NDJSON frames — one `{"kind":"event",...}`
per audit event, followed by a terminal `{"kind":"end",...}` with
aggregated token usage and wall-clock elapsed.

## Example — send a goal against a running Docker container

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"agent.run","params":{
  "agent_id":"<bootstrap-agent-id>",
  "message":"Fetch https://lobste.rs and summarize top 3 to /output/lobsters.txt"
}}' | python3 -c "import socket,sys; s=socket.socket(socket.AF_UNIX); \
  s.connect('/tmp/aaos-sock/agentd.sock'); \
  s.sendall((sys.stdin.read()+'\n').encode()); print(s.recv(4096).decode())"
```

## MCP Server API (loopback only)

The shipped `.deb` (v0.0.2+) is built with `--features mcp` by default; source builds need the flag explicitly. When `/etc/aaos/mcp-servers.yaml` has `server.enabled: true`, an HTTP+SSE listener binds `127.0.0.1:3781`. The endpoint speaks [Model Context Protocol](https://modelcontextprotocol.io) (2024-11 spec) so external MCP clients (Claude Code, Cursor, other agents) can delegate goals into aaOS.

Three tools are exposed via the standard MCP `tools/call` method:

| Tool | Input | Output |
|------|-------|--------|
| `submit_goal` | `{ goal: string, role?: string }` | `{ run_id: string }` |
| `get_agent_status` | `{ run_id: string }` | `"running" \| "completed" \| "failed" \| "notfound"` |
| `cancel_agent` | `{ run_id: string }` | `{ cancelled: bool }` |

There is no auth on the endpoint. The binding is loopback-only by design — remote access is the operator's responsibility (SSH tunnel, Tailscale, or a local reverse proxy with auth).

### Example — submit a goal over MCP

```bash
curl -X POST http://127.0.0.1:3781/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call",
       "params":{"name":"submit_goal",
                 "arguments":{"goal":"fetch HN top 5 and summarize"}}}'
```

### Example — stream audit events for a run as SSE

```bash
curl -N "http://127.0.0.1:3781/mcp/events?run_id=<uuid>"
```

Each frame is a standard SSE event whose `data:` line is a JSON-serialized `AuditEvent`, filtered to the given `run_id`. The stream ends when the agent terminates or the client disconnects.
