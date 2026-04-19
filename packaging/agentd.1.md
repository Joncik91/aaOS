% AGENTD(1) aaOS Manual
% Jounes
% April 2026

# NAME

agentd — aaOS agent daemon and operator CLI

# SYNOPSIS

**agentd run** [**--config** *PATH*] [**--socket** *PATH*]

**agentd submit** [**-v**|**--verbose**] [**--socket** *PATH*] *GOAL*

**agentd list** [**--json**] [**--socket** *PATH*]

**agentd status** [**--json**] [**--socket** *PATH*] *AGENT_ID*

**agentd stop** [**--socket** *PATH*] *AGENT_ID*

**agentd logs** [**-v**|**--verbose**] [**--socket** *PATH*] *AGENT_ID*

**agentd roles list** [**--dir** *PATH*]

**agentd roles show** [**--dir** *PATH*] *NAME*

**agentd roles validate** *PATH*

# DESCRIPTION

**agentd** is both the aaOS daemon and the operator CLI that drives it. The **run** subcommand starts the daemon, which binds a Unix socket for JSON-RPC API calls. The other subcommands connect to that socket as clients.

Agents in aaOS are managed by a Bootstrap Agent that receives goals and decomposes them into child agents with narrowed capabilities. The operator never spawns agents directly; they submit goals, and Bootstrap assembles whatever agent team the goal requires.

# SUBCOMMANDS

## run

Start the daemon. Runs as the **aaos** system user via systemd: `sudo systemctl start agentd`. Reads `/etc/default/aaos` for environment variables (most importantly, the LLM provider API key).

## submit *GOAL*

Send *GOAL* to the Bootstrap Agent and stream audit events live. Exits with Bootstrap's final status. Without **--verbose**, only operator-visible events are shown (agent spawns, tool calls, completion); with **--verbose**, every audit event is emitted as raw NDJSON.

First Ctrl-C detaches the CLI from the stream (the agent keeps running; re-attach with `agentd logs <id>`). A second Ctrl-C within two seconds aborts the CLI immediately.

## list

Show running agents. Default output is a four-column table (id prefix, name, state, uptime). **--json** emits the raw server response.

## status *AGENT_ID*

Show detail for one agent: id, name, model, state, parent agent, capability count. *AGENT_ID* may be any unique prefix of the full UUID. Ambiguous prefixes error with the list of candidates.

## stop *AGENT_ID*

Terminate a running agent. *AGENT_ID* may be any unique prefix.

## logs *AGENT_ID*

Attach to a running agent's live audit stream. Same event filter as **submit** (verbose flag toggles between operator view and raw NDJSON). Ctrl-C detaches cleanly; the agent keeps running.

## roles

Inspect the role catalog at */etc/aaos/roles/*. Three subcommands:

- **list** — tabulate loaded roles and their parameter schemas.
- **show** *NAME* — print a role's full YAML definition.
- **validate** *PATH* — parse a role YAML without installing it; reports schema issues.

## configure

First-boot setup. Prompts for a DeepSeek or Anthropic API key, writes */etc/default/aaos* with mode **0600 root:root**, and restarts **agentd.service**. Intended as the single-command replacement for hand-editing the env file.

Flags:

- **--provider** *deepseek|anthropic* (default: deepseek) — which API key to seed.
- **--key-from-env** *VAR* — read the key from the given env var instead of prompting. Intended for non-interactive provisioning (Ansible, cloud-init).
- **--env-file** *PATH* (default: */etc/default/aaos*) — write target. Non-default paths don't require root.
- **--no-restart** — skip the `systemctl restart agentd` at the end.

Must run as root when writing */etc/default/aaos* (e.g. `sudo agentd configure`).

# EXIT CODES

- **0** — success (goal completed, list returned, agent stopped)
- **1** — agent-reported failure (Bootstrap said "couldn't do it")
- **2** — usage error (bad flag, missing argument, ambiguous or unknown agent id)
- **3** — daemon unreachable (socket missing, permission denied, daemon down)
- **4** — operator interrupt (second Ctrl-C during stream detach)

# ENVIRONMENT

*DEEPSEEK_API_KEY*, *ANTHROPIC_API_KEY*
: At least one must be set in `/etc/default/aaos` for the daemon to serve **submit** calls. DeepSeek is preferred; Anthropic is the fallback.

*AAOS_DEFAULT_BACKEND*
: Set to **namespaced** on kernels with Landlock (5.13+) to use the namespaced agent backend (process isolation via user/mount namespaces + Landlock + seccomp). Default: in-process.

*AAOS_BOOTSTRAP_MANIFEST_PATH*
: Override the Bootstrap manifest location (default `/etc/aaos/manifests/bootstrap.yaml`). Intended for tests; operators don't normally need it.

# FILES

*/run/agentd/agentd.sock*
: Unix socket the daemon listens on. Owned **aaos:aaos**, mode **0660**.

*/etc/aaos/manifests/bootstrap.yaml*
: Bootstrap Agent manifest. Marked as a conffile; operator edits survive upgrades.

*/var/lib/aaos/*
: Daemon state directory. Subdirectories: `memory/`, `sessions/`, `workspace/`.

*/etc/default/aaos*
: Environment file read by the systemd unit (API keys, backend selector). **Must be 0600 root:root.** systemd reads this as root before dropping to `User=aaos`, so the daemon still starts; the tight mode denies read access to every agent process, every operator in the `aaos` group, and every child agent the daemon spawns. `agentd` itself also scrubs the API-key env vars from its own process environment at startup, so `/proc/<pid>/environ` doesn't leak the key either. The `.deb` `postinst` tightens perms automatically on install or upgrade.

*/lib/systemd/system/agentd.service*
: The systemd unit.

# SOCKET ACCESS

The socket is owned `aaos:aaos` with mode `0660`. To let a non-root operator submit goals, add them to the **aaos** group:

    sudo adduser $USER aaos

Log out and back in for the new group membership to take effect.

# EXAMPLES

Submit a goal:

    agentd submit "fetch HN top 5 stories and write a summary"

List running agents:

    agentd list

Inspect one agent:

    agentd status a3b7c9d2

Stop an agent:

    agentd stop a3b7c9d2

Attach to an agent's live log:

    agentd logs a3b7c9d2

Machine-readable output for scripting:

    agentd list --json | jq '.agents[] | select(.state == "Running")'

# SEE ALSO

**systemctl**(1), **journalctl**(1)

The aaOS source and documentation live at *https://github.com/Joncik91/aaOS*.
