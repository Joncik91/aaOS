# Docker deployment

The Docker path uses the legacy Bootstrap Agent orchestrator — a single DeepSeek Reasoner that receives a goal, spawns child agents with narrowed capabilities, and coordinates their work. No role catalog needed. Useful for quick local trials, development, and reproducing historical self-reflection runs.

For production, the `.deb` install (systemd + operator CLI + Planner/PlanExecutor) in the README's Quick Start is the recommended path.

## Basic run

```bash
git clone https://github.com/Joncik91/aaOS.git && cd aaOS
DEEPSEEK_API_KEY="sk-..." ./run-aaos.sh "fetch HN top 5 and write a summary to /output/summary.txt"
cat output/summary.txt
```

The launcher builds the container image on first run, starts the daemon as PID 1, and opens a live dashboard in a separate terminal showing agent activity in real time. `Ctrl+C` stops the container.

The source tree is mounted read-only at `/src/` inside the container, so agents can read and understand the codebase when given code-related goals.

Falls back to Anthropic if `ANTHROPIC_API_KEY` is set instead of `DEEPSEEK_API_KEY`.

## Cross-run memory (opt-in)

By default, every container start uses a fresh Bootstrap identity and empty memory. To let the Bootstrap Agent accumulate lessons across restarts:

```bash
AAOS_PERSISTENT_MEMORY=1 DEEPSEEK_API_KEY="sk-..." ./run-aaos.sh "your goal"
```

This bind-mounts `./memory/` into the container. The Bootstrap ID is persisted at `/var/lib/aaos/bootstrap_id` (overridable via `AAOS_BOOTSTRAP_ID`). The manifest instructs Bootstrap to `memory_query` before decomposing a goal and `memory_store` a compact summary after completing one. To wipe persistent state, launch once with `AAOS_RESET_MEMORY=1`.

Persistent memory carries real risk — prompt-injected content and bad strategies become durable. Feature is opt-in; reset is one env var away.

## Sending additional goals to a running container

The container keeps listening on a Unix socket after completing a goal:

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"agent.run","params":{
  "agent_id":"<bootstrap-agent-id>",
  "message":"Fetch https://lobste.rs and summarize the top 3 to /output/lobsters.txt"
}}' | python3 -c "import socket,sys; s=socket.socket(socket.AF_UNIX); \
  s.connect('/tmp/aaos-sock/agentd.sock'); \
  s.sendall((sys.stdin.read()+'\n').encode()); print(s.recv(4096).decode())"
```

See [`docs/api.md`](api.md) for the full JSON-RPC method list.
