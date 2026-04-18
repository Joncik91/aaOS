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

### In-process forgery by in-tree or third-party tool code — **CLOSED** (commits `14a8eae`, `18d14f0`, plus handle-field privacy)
- **Threat:** a tool running inside `agentd` (bundled in `aaos-tools` or loaded from an external crate) constructs a `CapabilityToken` struct literal, or fabricates a `CapabilityHandle` pointing at another agent's token, to escalate its grant.
- **Mitigations shipped:**
  - Handle-based tokens: tools receive `CapabilityHandle` only; the underlying `CapabilityToken` lives in a runtime-owned `CapabilityRegistry` and is never exposed to tool code.
  - `CapabilityHandle`'s inner `u64` field is private to the `aaos-core` crate; `CapabilityHandle::from_raw` is `pub(crate)`. Downstream crates (including `aaos-tools` and any external tool plugin) cannot construct a handle from an arbitrary integer — they can only receive, clone, and return handles the runtime issued.
  - Capability checks go through `registry.permits(handle, agent_id, cap)` and `registry.authorize_and_record(...)`. The registry verifies handle-to-agent ownership on every resolve; even if a hypothetical attacker constructed a handle they cannot pass a `requesting_agent` that isn't their own (the invocation layer fills it from the runtime-owned `InvocationContext`).
- **Status:** the API surface no longer lets in-process tool code either construct tokens or fabricate handles. There is no further fix until an auditor names a reachable bypass.

### Worker-side forgery on NamespacedBackend — **CLOSED BY DESIGN, WIRING PARTIAL** (commit `a73e062`)
- **Threat:** a compromised worker subprocess running a `NamespacedBackend` agent tries to construct or tamper with a capability handle locally to escalate inside the worker's address space.
- **Mitigation shipped:** workers receive **no** `CapabilityHandle` values in their launch protocol; the handle-field privacy fix above means they couldn't fabricate one locally even if they tried. The broker side owns a peer-creds-authenticated Unix socket (`SO_PEERCRED`) — pid + uid + gid exact-match against the spawned worker's recorded identity — that workers connect to during the `sandboxed-ready` handshake.
- **Current wiring caveat:** the production broker↔worker protocol today carries only launch control (handshake, `PokeOp`-style integration-test messages). Tool invocations for `NamespacedBackend` agents are not yet routed through the broker — the worker's agent loop is scaffolded for that path but not driving real workloads. The "workers hold no handles" claim is currently **vacuously true**: workers don't invoke tools in production at all. When the broker↔worker tool-invocation stream lands (see the "Persistent broker↔worker stream" entry below), the handle-privacy + peer-creds-auth + registry-owned-by-agentd stack will make worker-side forgery structurally impossible. Until then, the claim describes intent, not exercise.
- **Status:** closed in design; awaits the first real tool invocation through the namespaced-worker path to become exercised. Stacked seccomp filters + Landlock + `NoNewPrivs: 1` close adjacent memory-tampering threats regardless of whether tool-brokering is wired; verified end-to-end on Debian 13 / kernel 6.12.43.

### Registry memory tampering by attackers with Rust-level execution inside `agentd` — **OPEN**
- **Threat:** attacker gains code execution inside the `agentd` process via a memory-safety bug or a compromised dependency, then writes directly to the registry's `DashMap`.
- **Mitigation today:** none at the capability layer. Every agent trusts the `agentd` process with the capability table.
- **Why in-process HMAC doesn't fix this:** HMAC-signed tokens with the key held in `agentd`'s own memory buy nothing — an attacker who can mutate the DashMap can also read the key and produce valid signatures. HMAC with the key kept *outside* the process *would* help: hardware root (TPM2, SEV-SNP, secure enclave), a memfd_secret-backed key region (Linux 5.14+, unreadable even to the kernel), or a separate seccomp-confined signer subprocess. Those are viable but substantial — they're correctly tracked as Phase G hardening items, not Phase F-b. The first-order defenses are cheaper: Landlock ptrace denial on `agentd`, seccomp on `agentd` itself (restricting `process_vm_writev`, `ptrace`), and hardware isolation via MicroVM-per-agent keeping the registry in the host. Those are tracked under Phase F-b and Phase G.
- **Signal to reconsider:** in-scope memory-safety audit, OR an `unsafe`-heavy dependency lands that widens the attack surface, OR the signal on the "Cross-process transport" item below fires (HMAC with external key storage closes both gaps at once).

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
- **Blocks a claim:** today's "NamespacedBackend — closed by design, wiring partial" status on worker-side forgery (see `capability-token-forgery--threat-model-split` above). Once this lands, that class upgrades to "closed, exercised."

## Runtime-side confinement of tool execution for NamespacedBackend

- **Idea:** execute the agent's LLM loop + tool invocations **inside** the namespaced worker process, not in `agentd`. Tools the worker needs (file_read, file_write, grep, web_fetch, cargo_run, git_commit, the memory tools) route requests to the broker via the post-handshake stream; the broker resolves capabilities against the host-owned registry and either executes in-host (for now) or delegates to a host-side tool runtime. This is what a reader of `AAOS_DEFAULT_BACKEND=namespaced` expects the flag to buy.
- **Where seen:** the shape matches every capability-system production deployment (seL4 user-space servers, Capsicum, KataContainers' tool gVisor routing). The worker-local agent loop stub in `crates/aaos-backend-linux/src/worker.rs` is already half-written for the `Poke` path.
- **Why deferred:** layered on top of the persistent-stream item above. Also needs a decision about *which* tools execute in-host vs. which could run entirely in-worker (nothing today; pure-compute tools like `echo` are candidates later). Current single-node deployment doesn't force the split — InProcessBackend is the shipped default and already covers every threat class except in-process-`agentd` memory tampering.
- **Signal to reconsider:** (a) we ship a multi-tenant deployment where one agent process must not be able to read another agent's in-flight tool args; OR (b) Phase G MicroVM work begins and the same broker-routing design carries over; OR (c) a customer names "tool execution confined to the sandbox" as a gating security requirement.
- **Scope warning:** this is "Standard-tier Abstracted Filesystem" territory from the AOS rubber-duck exercise — it's a real capability gap today, and it's the piece that makes the word "sandbox" fully mean what readers assume.

## Dynamic model routing — cost- and latency-aware switching

- **Idea:** the PlanExecutor chooses which LLM to call per subtask based on a policy (mechanical edits → Haiku / DeepSeek-chat; design / cross-file synthesis → Sonnet / DeepSeek-reasoner), informed by live cost and latency signals. Today the `model` field on a role is static — a role YAML either says `deepseek-chat` or `claude-haiku-4-5-20251001` and every invocation pays that rate.
- **Where seen:** Cursor, Claude Code, and OpenCode all route to cheaper/faster models for mechanical edits; Aider has explicit `--weak-model` / `--editor-model` flags. The subagent-dispatch pattern we use manually ("haiku for `#[doc(hidden)]` annotation, sonnet for web_fetch capability design") is already this logic at a human layer.
- **Why deferred:** no cost/latency signal plumbed — we'd need a per-subtask budget tracker already built in `aaos-core::budget`, plus a classifier that decides "this subtask looks mechanical" from the plan. The classifier is the hard part: getting it wrong converts a design task into a rushed cheap-model output. Today the role author's explicit choice works.
- **Signal to reconsider:** (a) dashboard cost grows to the point where static routing is measurably wasteful (rough threshold: >$1/run sustained); OR (b) a role needs to escalate mid-subtask from a cheap to a strong model when the cheap model gets stuck — that's a structural change the static-model field can't express.

## Runtime tool authoring via MCP server integration

- **Idea:** accept externally-hosted MCP servers as tool sources. At startup, agentd reads an MCP-server list (env var or config file), connects over stdio/HTTP, fetches the tool list, and registers each as a `Tool` that forwards invocations to the remote server — still capability-checked at the aaOS boundary. Lets an operator drop in any MCP-compatible tool ecosystem (filesystem, browser, Playwright, database connectors, company APIs) without recompiling aaOS.
- **Where seen:** this is the Model Context Protocol's primary use case — Claude Desktop, Cursor, OpenCode, Continue.dev all consume MCP servers as a tool extension surface. `modelcontextprotocol.io` is the spec; ecosystem is now in the dozens of servers.
- **Why deferred:** (1) today's 16 built-in tools cover the single-operator use cases we've actually hit; (2) MCP-server integration needs a capability-mapping decision — what capability does an arbitrary remote `list_airtable_records` tool demand, and how does the operator grant it in a role YAML? That's a design pass, not a mechanical port; (3) MCP transport adds a network/subprocess dep on a spec that's still evolving.
- **Signal to reconsider:** (a) an operator asks for "connect my Notion / Slack / Jira" as a concrete request; OR (b) a credible MCP server appears for a gap our in-tree tools don't cover (e.g., Playwright for browser-based research); OR (c) external tool authors want to ship aaOS-compatible tools without upstreaming into `aaos-tools`.

## Distributed / multi-host agent runtime

- **Idea:** a parent agent on host A can delegate work to a child on host B. Agents hold capability handles that transport across the boundary; a cross-host router exchanges MCP messages and audit events. Today every agent runs inside a single `agentd` process on a single host.
- **Where seen:** Ray (distributed Python actors), the BEAM (Erlang/Elixir distribution), Dapr, any microservices fabric. In agent-specific terms: the "swarm" pattern OpenAI / LangChain diagrams sometimes imply but rarely deliver as a single-process simulation.
- **Why deferred:** single-operator deployments don't need it. It also forces HMAC-signed cross-process tokens (see the capability-token-forgery split), a persistent durable audit sink, and a clear story for partial failures (host B dies mid-subtask — does host A replan or drop the plan?). Each of those is its own design pass.
- **Signal to reconsider:** (a) a workload hits resource limits on a single node and the natural answer is "add another node" rather than "make the node bigger"; OR (b) a buyer / user specifically asks for multi-tenant swarms; OR (c) Phase G MicroVM work produces a transport that happens to also work across hosts, at which point the marginal effort to enable true multi-host becomes small.

## Cryptographic agent identity

- **Idea:** each agent gets a keypair at spawn; the agent signs outbound tool calls, message handoffs, and audit events with its private key. Receivers verify signatures using a runtime-owned public-key directory. Enables provenance claims ("this commit was authored by aaOS builder role in run bea8fa34") that hold up outside the trust boundary of a single `agentd`.
- **Where seen:** SPIFFE/SPIRE for workload identity in Kubernetes; Sigstore for build-artifact signing; every financial-transaction system that cares who signed what. The existing `Co-Authored-By: aaOS builder role (ephemeral droplet, run <id>)` commit trailer is the prose version of this — an unverifiable claim that would benefit from a signature.
- **Why deferred:** single-host deployments trust the `agentd` process by construction; the same threat actor who can tamper with the capability registry can also forge identities. Adding keys without moving keys out of `agentd`'s address space buys the same theater as in-process HMAC (see the registry-memory-tampering threat class). Meaningful only once either the multi-host transport lands or keys can live in a hardware-rooted location (TPM2, HSM, secure enclave).
- **Signal to reconsider:** (a) a compliance regime demands cryptographic authorship attestation on generated artifacts (SOC 2 Type 2, SLSA level 3+, code-signing requirements); OR (b) Phase G MicroVM work reaches the point where identity and HMAC signing can share the same external-key-storage mechanism; OR (c) multi-host runtime lands (see above) and cross-host verification needs something stronger than "we trust the process on that host."

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
