# Distribution Architecture

How aaOS becomes an agent-native Linux distribution. Not a fork — a curated userland on an upstream kernel, the way CoreOS was container-native Linux and Bottlerocket is Kubernetes-native Linux.

This document is a sketch, not a plan. It names components and how they'd interact; the implementation plan comes next and will be peer-reviewed.

## Principle

**Linux is the substrate. Agents are the primary workload. Everything else exists to serve that.**

The distribution curates which of Linux's primitives are exposed, wraps them in typed tools with capability checks at the call boundary, and ships a cohesive image where the default shell experience is *"tell an agent what you want."*

## Components

### Base layer — upstream Linux

- Kernel: mainline Linux from kernel.org. No fork. Kernel updates come from upstream; this is not a research kernel project.
- Userland: minimal Debian base (`debootstrap --variant=minbase`). Alpine is an option if we want musl and smaller images; Debian is the default for ecosystem compatibility.
- Init: systemd. `agentd` is a systemd service, not PID 1. Copilot's argument: PID 1 branding costs more in edge cases than it's worth.

**What's explicitly NOT in the base:**
- X11, Wayland, desktop environments — this is a headless OS.
- LibreOffice, browsers, end-user applications — agents call headless tools (pandoc, chromium --headless, curl) through wrappers.
- Anything that assumes a logged-in human.

### Agent runtime layer — aaOS native

- `agentd.service` — the runtime daemon. Listens on a Unix socket (`/var/run/agentd.sock`) for goal submissions. Spawns agents, enforces capabilities, routes IPC, logs to journald.
- `/etc/aaos/manifests/` — agent manifest library. Bootstrap manifest ships with the distribution; operators drop additional manifests here.
- `/etc/aaos/skills/` — AgentSkills bundle. 21 skills ship by default, extensible.
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
- **Audit event schema** — the 22 event kinds already defined stay unchanged. They emit to journald instead of stdout in the distribution.
- **Prometheus exporter** — optional service `aaos-metrics.service` that exports token usage, agent count, capability denials, budget exhaustion.
- **No dashboard in the base image** — operators can add one; the observability data is there to consume.

## Distribution build

### Package layout (Debian)

- `aaos-base` — systemd service, agentd binary, default config. Depends on `systemd`, `curl`, `jq`.
- `aaos-skills` — 21 bundled AgentSkills, read-only at `/usr/share/aaos/skills/`.
- `aaos-wrappers-core` — first-tier tool wrappers.
- `aaos-wrappers-media` — ffmpeg, imagemagick, pandoc. Optional.
- `aaos-wrappers-dev` — Rust toolchain wrappers. Optional.
- `aaos-metrics` — Prometheus exporter. Optional.
- `aaos-landlock-profiles` — per-capability Landlock rulesets.
- `aaos-seccomp-profiles` — per-capability seccomp filters.

### Image types

- **Debian package set** — `apt install aaos` on an existing Debian/Ubuntu system.
- **Minimal ISO** — bare-metal or VM installation, ~200 MB compressed. Debian installer + `aaos-base` preselected.
- **Cloud images** — AMI (AWS), GCE image (Google), Azure image, DigitalOcean droplet.
- **Docker image** — for cases where a full distribution is overkill and just the agentd + wrappers + skills are wanted.
- **Nix expression** — reproducible build for Nix users.

### Hardware targets

- Cloud VMs (x86_64, arm64)
- NUCs (Intel x86_64)
- Raspberry Pi 5 (arm64) — interesting for edge deployments
- Laptops with sufficient RAM (~8 GB minimum for local inference)

## Migration from today

The current Docker-only deployment doesn't disappear. The distribution ships alongside it:

1. **`agentd` binary stays binary-compatible.** Same CLI flags, same socket API, same manifests. The distribution just includes it preinstalled.
2. **Docker image keeps working.** Operators who don't want a full distribution use `docker run aaos-bootstrap` as today.
3. **Systemd unit is the new preferred deployment.** For persistent single-host or cloud-VM deployments, installing the Debian package and running `systemctl start agentd` is simpler than managing a Docker container.
4. **Landlock/seccomp profiles are opt-in initially.** Ship without them in the first release; add them once the profile library is stable.

## What this is NOT

- **Not a fork of the Linux kernel.** Kernel updates come from upstream Debian.
- **Not a replacement for Docker.** Docker remains a deployment option; the distribution is the *alternative* for operators who want a cohesive system.
- **Not a competitor to Kubernetes.** Kubernetes orchestrates containers across hosts. aaOS orchestrates agents within a host. Complementary — one day there'll be an `aaos-on-k8s` pattern but it's not this phase.
- **Not a microkernel.** seL4/Redox stay as a deferred research backend for the ABI, behind market demand.

## Open questions

1. **Debian vs Alpine base?** Debian has the ecosystem, Alpine has the size. Pick one and commit; supporting both doubles packaging work.
2. **When to add the tool wrappers?** Ship a minimal set in the first release (grep, jq, curl, sqlite3) and grow the library as workloads demand, or ship a comprehensive set at once? Gut: minimal set first.
3. **Update model.** atomic A/B partition updates (like CoreOS/Fedora CoreOS) or standard `apt upgrade`? Atomic updates are safer for unattended deployments but add image-building complexity.
4. **Agent identity across reboots.** Persistent memory already handles this for Bootstrap. Does the distribution need a host-wide identity (machine ID) tied to agent identities for multi-host deployments?
5. **GPU access as a capability.** Local inference wants GPUs. How does a capability declaration map to CUDA / ROCm / Vulkan access? Likely a `device: /dev/nvidia0` capability, but the wrapper layer is non-trivial.

## What comes next

This sketch doesn't get implemented in one step. The order that makes sense:

1. **Proof of concept — systemd unit.** Package `agentd` as a `.deb`, install on a fresh Debian VM, run a real goal. Confirms the service model works outside Docker.
2. **First tool wrapper — `grep`.** Build the wrapper scaffold (capability declaration, JSON schema, Landlock + seccomp scoping) with one tool as the reference implementation.
3. **Minimal image — Debian + agentd + 5 wrappers.** Produce an ISO. Measure size, boot time, memory baseline.
4. **Landlock integration.** Per-capability rulesets, tested against the existing capability tests.
5. **Cloud image.** One cloud target (probably AMI since AWS is common).
6. **First external users.** Someone other than the maintainer runs aaOS from the distribution and reports what breaks.

Each step is its own feature with its own plan + peer review. This document is the **target**; individual plans design the path.
