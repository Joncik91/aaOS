# Distribution Architecture

How Phase F ships: a Debian derivative where aaOS is preinstalled and configured. Not a from-scratch distribution — a customized Debian install.

This document is a sketch, not a plan. It names components and how they'd interact; the implementation plan comes next and will be peer-reviewed.

## What this is and isn't

**What it is.** A **Debian derivative**. Upstream Debian 13 as the base, the aaOS `.deb` preinstalled, opinionated systemd/config defaults baked in, the desktop meta-packages stripped. Shipped as a bootable ISO and as cloud snapshots for the targets Debian itself publishes to (AWS, DigitalOcean, Hetzner). Same scope model as Home Assistant OS, Raspberry Pi OS, DietPi, Tailscale's prebuilt images.

**What it isn't.** A from-scratch distribution. We do **not** maintain our own apt repos. We do **not** run a CVE response process. We do **not** maintain a kernel. We do **not** run release engineering for a base OS. All of that stays with upstream Debian; an installed aaOS host pulls security updates from `deb.debian.org` the same way any Debian install does.

**Why derivative works for a solo project.** We inherit Debian's security response, kernel updates, apt ecosystem, hardware support, and release cadence — none of which are free to maintain, and none of which are our differentiation. Our work is confined to the aaOS-specific layers: the `.deb` contents, the Packer pipeline, the default config files. That's solo-maintainer-sized. A true distribution (Debian, Ubuntu, Red Hat, Alpine) is a team-of-dozens project and nothing in this repo is scoped for that.

## Principle

**Linux is the substrate. Agents are the primary workload. Everything else exists to serve that.**

The derivative curates which of Debian's primitives are exposed, wraps them in typed tools with capability checks at the call boundary, and ships a cohesive image where the default shell experience is *"tell an agent what you want."* That experience is concrete today: `agentd submit "<goal>"` streams live audit events from Bootstrap as it decomposes the goal and runs child agents.

## Components

### Base layer — upstream Debian

- **Kernel:** mainline Debian 13 kernel. No fork, no custom builds. Kernel updates come from `deb.debian.org` via `apt`. This is not a research kernel project.
- **Userland:** Debian 13 minimal. The Packer pipeline starts from Debian's published base image and removes desktop meta-packages; it does not `debootstrap` a custom userland.
- **Init:** systemd (Debian's default). `agentd` is a systemd service, not PID 1. PID 1 branding costs more in edge cases than it's worth.

**What's explicitly NOT in the image:**
- X11, Wayland, desktop environments — this is a headless appliance.
- LibreOffice, browsers, end-user applications — agents call headless tools (pandoc, chromium --headless, curl) through wrappers.
- Anything that assumes a logged-in human.

### Agent runtime layer — aaOS native

- `agentd.service` — the runtime daemon. Listens on a Unix socket (`/run/agentd/agentd.sock`, mode 0660, owned `aaos:aaos`) for goal submissions. Spawns agents, enforces capabilities, routes IPC, logs to journald.
- `agentd` operator CLI — same binary as the daemon. Subcommands `submit | list | status | stop | logs` connect to the socket. Non-root operators join the `aaos` system group (`sudo adduser $USER aaos`) to get socket access. Ships with an `agentd(1)` man page.
- `/etc/aaos/manifests/` — agent manifest library. Bootstrap manifest ships with the package; operators drop additional manifests here.
- `/etc/aaos/skills/` — AgentSkills bundle. 21 skills ship by default, extensible.
- `/etc/aaos/roles/` — role catalog for computed orchestration. Four roles ship (fetcher, writer, analyzer, generalist); operator-extensible.
- `/etc/aaos/mcp-servers.yaml` — optional. When present and `agentd` is built with `--features mcp`, declares external MCP servers to consume as tool sources and/or enables a loopback MCP server on `127.0.0.1:3781`. Absent → MCP silently disabled.
- `/var/lib/aaos/memory/` — persistent memory for agents with stable identity.
- `/var/lib/aaos/sessions/` — session history for persistent agents.
- `/var/lib/aaos/workspace/` — per-goal workspace directories.

### Capability enforcement layer — Linux primitives

aaOS issues capability tokens. Enforcement uses Linux primitives as backstops so the security model has defense in depth, not just application-level checks.

| Capability | aaOS check | Linux backstop |
|---|---|---|
| `file_read: /data/*` | Tool-invocation capability match | Landlock ruleset: `LANDLOCK_ACCESS_FS_READ_FILE` on `/data` |
| `file_write: /data/output/*` | Tool-invocation capability match | Landlock: `LANDLOCK_ACCESS_FS_WRITE_FILE` on `/data/output` |
| `network_access: [api.example.com]` | Custom capability check | nftables rule, network namespace with filtered egress |
| `tool: web_fetch` | Tool registry lookup | Seccomp allow: `socket`, `connect`, `sendto`, `recvfrom`; deny the rest |
| `spawn_child: [research]` | Capability match on child manifest name | No Linux backstop — aaOS-only policy |
| `tool: mcp.<server>.<tool>` | Tool registry lookup (proxy registers as the same tool-invoke capability variant as built-ins) | Remote MCP server's own input validation; the proxy is trusted once registered |
| Budget (`max_tokens`) | `BudgetTracker` atomic CAS | cgroup v2 memory.max for process resources |

**Key point:** seccomp is a damage-limiter, not the capability model. Landlock is a filesystem backstop, not the capability model. aaOS's token logic stays the policy layer; Linux primitives are the kernel-level "even if the application-level check is bypassed, the kernel still refuses" safety net.

### Tool wrapper layer — curated Linux CLI ecosystem

Every Linux tool an agent might use is exposed via a typed MCP wrapper. The wrapper:
1. Declares its capability requirement (e.g., `grep` needs `file_read: {path}`).
2. Validates input against a JSON schema.
3. Invokes the underlying binary in a scoped environment (Landlock ruleset, seccomp filter, cgroup).
4. Parses output into typed results.

**First-tier wrappers to ship:**
- `grep` / `rg` — pattern search
- `jq` — JSON query
- `sed` / `awk` — text transformation (output-only, input from stdin)
- `git` — read operations only by default; write requires explicit capability
- `curl` — HTTP, with `network_access` capability required
- `pandoc` — document conversion
- `ffmpeg` — media transcoding
- `chromium --headless` — rendering, screenshots
- `sqlite3` — database queries
- Rust toolchain (`cargo`, `rustc`) — for code-writing workloads

The tool wrapper registry is extensible; third parties can drop a wrapper spec into `/etc/aaos/wrappers/` and it becomes available to any agent with the matching capability.

### Observability layer

- **journald** — default audit sink. Structured JSON logs; queryable via `journalctl -u agentd`.
- **Audit event schema** — the 22 event kinds already defined stay unchanged. They emit to journald instead of stdout when installed as a package.
- **Prometheus exporter** — optional service `aaos-metrics.service` that exports token usage, agent count, capability denials, budget exhaustion.
- **No dashboard in the base image** — operators can add one; the observability data is there to consume.

## Package layout

The `.deb` (Phase F-a) and the image (Phase F-b) share most of their contents — the image is "Debian + the .deb + a handful of config files." The split:

### `.deb` contents (Phase F-a)

- `aaos-base` — systemd service, agentd binary, worker binary, default config. Depends on `systemd`, `curl`, `jq`.
- `aaos-skills` — 21 bundled AgentSkills, read-only at `/usr/share/aaos/skills/`.
- `aaos-wrappers-core` — first-tier tool wrappers.
- `aaos-wrappers-media` — ffmpeg, imagemagick, pandoc. Optional.
- `aaos-wrappers-dev` — Rust toolchain wrappers. Optional.
- `aaos-metrics` — Prometheus exporter. Optional.
- `aaos-landlock-profiles` — per-capability Landlock rulesets.
- `aaos-seccomp-profiles` — per-capability seccomp filters.

Installable on any Debian 13 host: `apt install ./aaos_*.deb`.

### Image additions (Phase F-b)

The image is the `.deb` plus:

- `/etc/aaos/config.yaml` with `backend: namespaced` and `AAOS_DEFAULT_BACKEND=namespaced` baked in (assumes the host kernel supports it; Debian 13 does).
- Desktop meta-packages removed from the base.
- `/etc/motd` pointing at the socket, the journal, and the docs URL.
- Packer provisioning steps that run `apt install` against upstream Debian's repos (no custom apt repo is stood up).

### Image formats

- **Bootable ISO** — Debian's installer + `aaos-base` preselected. Same Debian installer flow users already know; aaOS is just preinstalled.
- **Cloud snapshots** — published to the targets Debian itself publishes to (AWS AMI, DigitalOcean droplet, Hetzner image, GCE image). We don't maintain cloud vendor partnerships; we publish on the targets where Debian's own cloud images already live.
- **Docker image** — for cases where a full derivative is overkill and just the agentd + wrappers + skills are wanted. The `.deb` is the source of truth; the Docker image is a secondary artifact.
- **Nix expression** — optional, for Nix users. Low priority; ship only if there's demand.

### Hardware targets

- Cloud VMs (x86_64, arm64)
- Small-form-factor x86 mini PCs
- Raspberry Pi 5 (arm64) — interesting for edge deployments (Debian publishes arm64 images)
- Laptops with sufficient RAM (~8 GB minimum for local inference)

## Migration from today

The current Docker-only deployment doesn't disappear. The derivative ships alongside it:

1. **`agentd` binary stays binary-compatible.** Same CLI flags, same socket API, same manifests. The `.deb` just includes it preinstalled and configured.
2. **Docker image keeps working.** Operators who don't want a full host install use `docker run aaos-bootstrap` as today.
3. **Systemd unit is the new preferred deployment.** For persistent single-host or cloud-VM deployments, installing the `.deb` and running `systemctl start agentd` is simpler than managing a Docker container.
4. **Landlock/seccomp profiles are opt-in initially.** Ship without them in the first `.deb` release; add them once the profile library is stable. The derivative image can flip the default earlier than the standalone `.deb`.

## What this is NOT

- **Not a kernel fork.** Kernel updates come from upstream Debian via `apt`.
- **Not a replacement for Docker.** Docker remains a deployment option; the derivative is the *alternative* for operators who want a cohesive host install.
- **Not a competitor to Kubernetes.** Kubernetes orchestrates containers across hosts. aaOS orchestrates agents within a host. Complementary — one day there'll be an `aaos-on-k8s` pattern but it's not this phase.
- **Not a from-scratch distribution.** We don't run apt repos, CVE tracking, release engineering, or kernel maintenance. That's Debian's job, not ours.
- **Not a microkernel.** seL4/Redox stay as a deferred research backend for the ABI, behind market demand.

## Open questions

1. **When to add the tool wrappers?** Ship a minimal set in the first `.deb` (grep, jq, curl, sqlite3) and grow the library as workloads demand, or ship a comprehensive set at once? Gut: minimal set first.
2. **Update model.** Stick with upstream Debian's `apt upgrade` cadence (simple, inherits security response), or layer something image-level on top for unattended deployments (more work, less failure surface for operators)? Default: trust `apt`; revisit if operators complain.
3. **Agent identity across reboots.** Persistent memory already handles this for Bootstrap. Does the derivative need a host-wide identity (machine ID) tied to agent identities for multi-host deployments?
4. **GPU access as a capability.** Local inference wants GPUs. How does a capability declaration map to CUDA / ROCm / Vulkan access? Likely a `device: /dev/nvidia0` capability, but the wrapper layer is non-trivial.

## What comes next

This sketch doesn't get implemented in one step. The order that makes sense:

1. **Proof of concept — systemd unit.** Package `agentd` as a `.deb`, install on a fresh Debian 13 VM, run a real goal. Confirms the service model works outside Docker. (This is the Phase F-a deliverable.)
2. **First tool wrapper — `grep`.** Build the wrapper scaffold (capability declaration, JSON schema, Landlock + seccomp scoping) with one tool as the reference implementation.
3. **Packer pipeline — Debian 13 + the `.deb` + 5 wrappers.** Produce an ISO. Measure size, boot time, memory baseline. (This is the Phase F-b deliverable at its minimum.)
4. **Landlock integration on by default in the image.** Per-capability rulesets, tested against the existing capability tests.
5. **Cloud snapshot.** One cloud target first; extend to the others Debian publishes on.
6. **First external users.** Someone other than the maintainer installs the `.deb` or boots the image and reports what breaks.

Each step is its own feature with its own plan + peer review. This document is the **target**; individual plans design the path.
