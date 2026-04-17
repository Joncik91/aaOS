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
