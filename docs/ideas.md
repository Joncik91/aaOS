# Ideas Log

Things we considered and deliberately did **not** build, with a note on why and what would prompt reconsideration. Keeping this public because knowing what we looked at and chose not to do is useful evidence — not secret deliberation.

Each entry is short by design. If something here grows enough to deserve an implementation plan, it graduates to [`roadmap.md`](roadmap.md) and this entry becomes a pointer to the commit(s) that shipped it.

---

## Hibernate and resume for long-running sandboxes

- **Idea:** a child agent's workspace (and possibly the LLM session) can freeze mid-task and resume later, independent of the orchestrating agent's lifecycle. If the container crashes or gets restarted, in-flight work is not lost.
- **Where seen:** Vercel's `open-agents` template uses this pattern — their Workflow SDK treats chat requests as durable workflow runs that survive request boundaries, and their sandbox VMs can hibernate and resume independently.
- **Why deferred:** aaOS runs are short (typical Run is 15-30 min, cost <$0.20). Container restarts mid-task have not been observed. Bootstrap memory already persists across restarts via the stable-identity + SQLite path; child workspace state is the only thing we'd lose, and that's recoverable by re-running.
- **Signal to reconsider:** runs routinely exceed ~1 hour, or we see real failures where a child is >50% done and the container dies.

## `spawn_agents` batch tool — **SHIPPED** (Run 11 prep, commit `04dc0c7`)

- **What was shipped:** `spawn_agents` tool in `agentd/src/spawn_agents_tool.rs`. Best-effort batch semantics: preflight fast-fail snapshot (not atomic), per-child spawn delegates to `SpawnAgentTool::invoke` to reuse its scopeguard cleanup. Cap: `AAOS_SPAWN_AGENTS_BATCH_CAP` env var (default 3). Three Copilot review rounds resolved the atomic-reservation concerns — ended with honest best-effort + centralized cleanup via `remove_agent` in the registry.
- **What was NOT built:** true transactional multi-spawn (sibling abort on failure) — deferred until there's a concrete workload that needs all-or-nothing guarantees. Today, Bootstrap sees per-child `{agent_id, response, error}` entries and decides what to do with partial success.

## Repository Intelligence Graph (RIG)

- **Idea:** pre-compute a deterministic JSON summary of `/src/` at container startup — crate tree, public symbols, imports, line counts — so agents can query structure via one tool call instead of 20+ `file_list`/`file_read` walks.
- **Where seen:** repository intelligence graph research in CLI agent tooling reports ~54% completion-time reduction and ~12% accuracy improvement on scan-heavy tasks.
- **Why deferred:** keeping the map current is a real maintenance cost (rebuild pipeline integration, cache invalidation). `file_read_many` + the shorter chain may recover enough of the scan cost without the overhead. Phase 2 candidate at earliest, after Run 8 data.
- **Signal to reconsider:** post-Phase-1 runs still spend >3 minutes in scan phases, and the scans look structurally the same run-to-run.

## Deterministic decomposition for known goal shapes

- **Idea:** when the user goal matches a known template (e.g., "scan source and propose X"), skip Bootstrap's LLM-mediated decomposition and hardcode the agent chain. Bootstrap only reasons when the goal is genuinely novel.
- **Where seen:** TB-CSPN research on deterministic rule-based coordination reports ~67% fewer API calls and ~167% throughput vs sequential LLM-led prompt chaining. Anthropic's workflow framework documentation describes similar "if the shape is known, skip the planner" patterns.
- **Why deferred:** small number of goal shapes seen so far. We need 10-20 runs of varied goals before we know which templates are worth hardcoding. Encoding a template too early risks cementing a pattern that turns out to be wrong.
- **Signal to reconsider:** the same goal shape appears ≥5 times and Bootstrap's decomposition is roughly identical across them.

## Small-model orchestrator (Nemotron-style control plane)

- **Idea:** replace `deepseek-reasoner` for Bootstrap's routine routing decisions with a specialized small model (e.g., an 8B orchestration-tuned model). Reserve the expensive reasoner for novel decomposition only.
- **Where seen:** NVIDIA's Nemotron-Orchestrator-8B targets exactly this role — specialized SLM for agentic routing.
- **Why deferred:** adds a second model dependency, another API key, more moving parts. Bootstrap's thinking cost is ~7 minutes across a run, real but not dominant. Copilot's review: "at aaOS scale (20-40 req/run), a rule router for known workflows is simpler and more reliable than adding another model tier."
- **Signal to reconsider:** Bootstrap orchestration exceeds 40% of total run time *and* deterministic decomposition (see above) has already been tried or doesn't fit.

## Generic executor-level tool-call parallelism

- **Idea:** `AgentExecutor::run` automatically dispatches multiple tool_use blocks from a single LLM response in parallel via `tokio::join_all`.
- **Why deferred:** Copilot review flagged this as "too broad — same-turn tool calls can be semantically dependent even when they look independent." Safer shape is tool-level opt-in via explicit batch tools (we did this: `file_read_many`). Generic parallelism would need a per-tool `parallel_safe` classification first.
- **Signal to reconsider:** multiple concrete tools are all worth running in parallel AND the per-tool classification work has been done.

## Capability token forgery — threat-model split

The monolithic "cryptographically unforgeable tokens" item from earlier rounds conflated four distinct threat classes with different current statuses. Splitting them out replaces a vague **PARTIAL** with specific **CLOSED** / **OPEN** / **N/A** per case — which is both more accurate and a stronger security claim than implementing HMAC for a path that doesn't exist yet.

### In-process forgery by in-tree or third-party tool code — **CLOSED** (commits `14a8eae`, `18d14f0`)
- **Threat:** a tool running inside `agentd` (bundled in `aaos-tools` or loaded from an external crate) constructs a `CapabilityToken` struct literal, or mutates an existing one, to escalate its grant.
- **Mitigation shipped:** handle-based tokens. Tools receive `CapabilityHandle` (a `u64` wrapper) only. The underlying `CapabilityToken` lives in a runtime-owned `CapabilityRegistry` and is never exposed to tool code. Capability checks go through `registry.permits(handle, agent_id, cap)`; the registry verifies handle-to-agent ownership on every resolve. A forged handle either points at nothing (unknown index) or points at a token owned by a different agent, which the ownership check rejects.
- **Status:** the API surface no longer lets in-process tool code construct or mutate tokens. There is no further fix until an auditor names a reachable bypass.

### Worker-side forgery on NamespacedBackend — **CLOSED** (commit `a73e062`)
- **Threat:** a compromised worker subprocess running a `NamespacedBackend` agent constructs its own handle or tampers with one to escalate inside the worker's address space.
- **Mitigation shipped:** workers hold no `CapabilityHandle` values at all. All tool invocations leave the worker via a peer-creds-authenticated Unix socket (`SO_PEERCRED`) to the broker in `agentd`. The broker looks up the caller's agent ID from socket peer creds, resolves the requested capability against the registry in-process, and answers. Handles never cross the process boundary.
- **Status:** closed by design — the worker has nothing to forge. Stacked seccomp filters + Landlock + `NoNewPrivs: 1` close adjacent memory-tampering threats; verified end-to-end on Debian 13 / kernel 6.12.43.

### Registry memory tampering by attackers with Rust-level execution inside `agentd` — **OPEN** (target fix is not HMAC)
- **Threat:** attacker gains code execution inside the `agentd` process via a memory-safety bug or a compromised dependency, then writes directly to the registry's `DashMap`.
- **Mitigation today:** none at the capability layer. Every agent trusts the `agentd` process with the capability table.
- **Why HMAC doesn't fix this:** HMAC-signed tokens would live next to the HMAC key in the same address space. An attacker who can mutate the DashMap can also read the key and produce valid signatures. The real defenses are OS-level (Landlock ptrace denial on `agentd`, seccomp on `agentd` itself) or hardware isolation (MicroVM-per-agent with the registry kept in the host). Both are tracked under Phase F-b and Phase G.
- **Signal to reconsider:** an in-scope memory-safety audit, OR an `unsafe`-heavy dependency lands that widens the attack surface.

### Cross-process / cross-host transport of tokens — **N/A today, OPEN when needed** (HMAC is the right fix here)
- **Threat:** tokens need to leave `agentd`'s address space — e.g., a Phase G MicroVM backend where a VM-resident worker holds tokens locally, or a multi-host swarm where a parent on host A delegates to a child on host B.
- **Mitigation today:** no such transport exists. `NamespacedBackend` workers are local and hold no handles; all inter-process communication routes through the peer-creds-authenticated broker.
- **Target fix (when signal fires):** HMAC-signed `(agent_id, capability, constraints, issued_at)` with a runtime-only secret, plus a nonce table to defeat replay. Key management (rotation, daemon-restart semantics) needs designing against the real transport shape — building it speculatively now would commit to a format the real use case might want different.
- **Signal to reconsider:** (a) Phase G MicroVM backend is being implemented and tokens need to cross the VM boundary, OR (b) multi-host agent swarms become a real requirement, OR (c) a security-focused customer names HMAC signing as a gating requirement.

## Full JSON Schema validation for MCP messages

- **Idea:** `aaos-ipc::validator` currently checks required-field presence and basic type (string/object/array/etc). Full JSON Schema — `pattern`, `enum`, `minimum`/`maximum`, `properties` recursion, `$ref`, `oneOf`/`anyOf` — is not implemented.
- **Where seen:** standard `jsonschema` crates (`jsonschema`, `valico`, `schemars`). The MCP spec itself uses draft-2020-12 JSON Schema.
- **Why deferred:** bundled tools define their own input schemas and parse within their `invoke()` bodies; invalid input surfaces as a typed `CoreError::InvalidManifest` or similar. The validator layer is belt-and-braces. Cost of adding full validation: one dependency, modest compile-time hit, some refactoring of the validator interface.
- **Signal to reconsider:** third-party tools start shipping with rich schemas we'd like to enforce centrally, OR a bug lands where malformed input crashes a tool and a validator would have caught it upstream.

## TOCTOU hardening for path capability checks (openat + O_NOFOLLOW)

- **Idea:** capability checks resolve symlinks at check time (Fix 4 from Run 9, commit `45418cc`), but the actual file open happens later. An attacker who can swap a symlink between check and open can still redirect the read/write. Stronger guarantee: open the file with `openat(AT_FDCWD, path, O_NOFOLLOW)` in the tool itself and compare the resulting fd's `fstat` device/inode against a canonicalized grant, so no second filesystem lookup happens.
- **Where seen:** standard Unix filesystem-security pattern (seL4 capability reference monitor, Linux `openat2(RESOLVE_BENEATH)`, Go's `os.Root`).
- **Why deferred:** TOCTOU requires an attacker who can write to a symlink-mutable path *inside a granted prefix* between the check and the open. In aaOS today the filesystem is a Docker container the agent doesn't write symlinks to (we verified: no manifest grants target symlink-aliased paths), so the exploit surface is near-zero. The platform-specific `openat` plumbing would be invasive across every file tool. Fix 4's filesystem canonicalization closes the bulk of the risk.
- **Signal to reconsider:** agents gain the ability to create symlinks inside their writable prefixes (no `file_symlink` tool today), OR the kernel migration brings us to a platform where `openat` is the canonical filesystem primitive anyway.

## Enriched audit events for tool arguments / results

- **Idea:** `ToolInvoked` / `ToolResult` audit events today carry only `tool: String` and a hash. If they carried truncated args (path, first 200 bytes of result), the dashboard and detail-log could show rich summaries without parsing the parallel tracing stream.
- **Where seen:** Copilot's Round-2 review of the observability redesign flagged this as a cleaner alternative to the current "two-stream parse" approach (`detail-log.py` consumes both audit JSON and tracing output).
- **Why deferred:** the tracing stream works today; rewiring audit events is a wider change that would invalidate historical fixture files and require schema-versioning in the consumer. The observability redesign explicitly scoped v1 to "dashboard from existing audit JSON only."
- **Signal to reconsider:** the two-stream parse breaks when we add a new tool, or when we want to archive full tool payloads durably (also see: payload archiver, which was opt-in-but-deferred in the observability plan).

## Persistent broker↔worker stream for post-handshake messaging

- **Idea:** after the `sandboxed-ready` handshake in `NamespacedBackend`, retain the connected `UnixStream` on `BrokerSession` and expose a `send_poke(agent_id, PokeOp)` / eventual `invoke_tool(...)` method on the backend. Today the handshake completes and the stream is dropped, so the broker has no way to reach the running worker.
- **Where seen:** standard client-session-on-handler pattern — any long-lived RPC framework. The worker-side half already handles `Request::Poke(...)` through its agent loop.
- **Why deferred:** (1) the worker's agent loop today only exists for poke-style integration tests — production tool-brokering isn't wired yet; (2) keeping the stream alive requires lifting it into a `Mutex<Option<UnixStream>>` on the session and defining who owns writes (currently nothing does). Needs a small design pass on framing + concurrency before implementation.
- **Signal to reconsider:** (a) the `worker_cannot_execve` integration test needs to be real (currently a launch+stop scaffold), OR (b) the broker starts forwarding real tool invocations — the first workload that isn't "launch + stop" forces this wiring.

## Custom aaOS-specific installer / live image

- **Idea:** instead of shipping a Debian-derivative image (Phase F-b: upstream Debian installer + aaOS preinstalled + opinionated defaults), ship a fully custom installer — branded "aaOS," non-Debian installation flow, potentially immutable A/B-partition rootfs à la Bottlerocket / Fedora CoreOS / Talos.
- **Where seen:** Bottlerocket, Fedora CoreOS, Talos, RancherOS — each runs its own installer, its own update mechanism, its own image-signing pipeline. Home Assistant OS ships a custom live image with a Home Assistant-branded first-boot flow.
- **Why deferred:** a custom installer is a separate project on the scale of the rest of aaOS combined. The Debian installer works, is well-understood by operators, handles the hardware-compatibility long tail we'd otherwise inherit, and lets us ship a derivative today. The "Debian branding appears during install" is a real but cosmetic cost. Immutable A/B partitions are a real but operational-convenience cost, not a security cost — security comes from Landlock + seccomp + namespaces at runtime, which the derivative already provides.
- **Signal to reconsider:** (a) users complain that the Debian installer shows Debian branding instead of aaOS branding and that's blocking adoption, OR (b) a buyer specifically demands immutable A/B partitions (atomic updates, rollback-on-failure) as a gating requirement for an unattended-deployment use case.

## Self-hosted build loop (aaOS applies plans to aaOS) — **core tool set shipped 2026-04-17**

- **Idea:** aaOS should be able to read a markdown implementation plan and apply it to its own Rust source tree on a throwaway host, running `cargo check`/`cargo test` between edits to bound LLM drift. The minimum surface is a capability-scoped `cargo_run` tool (allowlisted subcommands, fixed workspace) plus a `builder` role that reads the plan and drives the loop.
- **Where seen:** OpenHands, Devin, SWE-agent, OpenCode, Claude Code — all build self-editing loops on top of an Edit/Read/Bash trio. aaOS's twist is capability enforcement: the agent gets `CargoRun { workspace }` scoped to one tree and a subcommand allowlist, not a general shell. `cargo install` and `cargo publish` are refused at the tool boundary.
- **Status:** coding-capable tool set now shipped across three commits: `cargo_run` + `builder` role (`45ce06b`), `file_edit` + `file_read(offset, limit)` (pending). Self-build runs 1–6 on a DO droplet surfaced the tool gaps (documented in `docs/reflection/2026-04-17-self-build-tool-gap.md`) and those gaps are now closed.
- **Next signal:** a self-build run on a non-trivial plan that produces a clean, reviewable diff. If that works, expand the tool set with `grep` (ripgrep wrapper, capability-scoped) and a narrow `git_commit` so the agent can produce branch + diff + commit end-to-end. If the loop still diverges after those land, the failure mode points at the next primitive.

## Deterministic scaffold roles (runtime-side execution for mechanical work) — **SIGNAL FIRED** (2026-04-17)

- **Idea:** roles whose work is purely mechanical (fetcher: `web_fetch → file_write → return path`) should not run through the LLM loop at all. Runtime detects a `scaffold: true` marker on the role YAML (or a `scaffold_kind: "fetcher"` discriminator) and dispatches directly via Rust code that calls `ToolInvocation::invoke` for each step. LLM-shaped roles (analyzer, writer) stay untouched.
- **Where seen:** the computed-skills project (github.com/Joncik91/computed-skills) names this explicitly — *"Don't make the LLM do work that code can do faster and more reliably."* Ties into Phase F-b's tool-wrapper layer where `cargo-builder` and `git-committer` would naturally be scaffolds too.
- **Why deferred originally:** the simpler path was an LLM-powered fetcher + a tight prompt + a tight output budget. Signal fired 2026-04-17 across four benchmark runs (see `docs/reflection/2026-04-17-role-budget-wiring.md`): prompt tightening achieved 12× wall-clock improvement (5m30s → 28s) but the fetcher LLM still satisfied its `"respond with the workspace path"` contract by **emitting the path without ever calling `file_write`**. Prompt contracts cannot enforce tool-call side effects when the LLM can satisfy the surface reading without performing the effect.
- **Signal to reconsider:** already fired. Next iteration ships it as the first bundled scaffold.

---

## How to add an entry

Keep each entry to four short sections:
- **Idea** (one-line statement)
- **Where seen** (prior art; cite the source by name)
- **Why deferred** (the actual reason, not hand-waving)
- **Signal to reconsider** (concrete condition that would flip the decision)

If the reason to defer is "we don't need it yet," say *exactly* what would make us need it. "Maybe later" is not a signal; "when scans exceed 3 minutes" is.
