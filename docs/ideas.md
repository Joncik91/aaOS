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

## Cryptographically unforgeable capability tokens — **PARTIALLY ADDRESSED** (commits `14a8eae`, `18d14f0`)

- **What shipped:** handle-based tokens (`CapabilityHandle` + `CapabilityRegistry` in `aaos-core`). Agents and tool implementations hold opaque handles; the underlying `CapabilityToken` lives in a runtime-owned table and is never exposed to non-runtime code. Cross-agent leak protection: a handle issued for agent A cannot resolve into agent B's invocation context. This closes the in-process forgery path — a compromised or third-party tool running in `agentd` can no longer fabricate valid `CapabilityToken` struct literals because it never sees a token in the first place.
- **What remains:** HMAC-signed tokens for **cross-process / cross-host transport**. Handles are process-local indices; they're meaningless outside `agentd`'s address space. If tokens ever need to cross into a MicroVM (Phase G), a separate host (multi-node agent swarms), or be persisted and later re-validated, we need `(agent_id, capability, constraints)` signed with a runtime-only secret. Also remains: defense against attackers with direct read access to `agentd`'s memory (`/proc/<pid>/mem`) — that requires OS-level primitives (Landlock ptrace denial, seccomp) or hardware isolation (MicroVM), tracked in Phase F and Phase G.
- **Further progress:** Commit `a73e062` closes the in-process forgery path a second time via `NamespacedBackend` — isolated workers hold no `CapabilityHandle` values at all; all tool invocations route through the broker over a peer-creds-authenticated Unix socket. HMAC signing remains for genuine cross-process/cross-host transport (Phase G MicroVM, multi-host). Worker-local handle forgery stops being a concern because there are no handles in the worker to forge.
- **Signal to reconsider (remaining piece):** (a) Phase G MicroVM backend is being implemented and tokens need to cross the VM boundary, OR (b) multi-host agent swarms become a real requirement, OR (c) a security-focused customer names HMAC signing as a gating requirement.

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

---

## How to add an entry

Keep each entry to four short sections:
- **Idea** (one-line statement)
- **Where seen** (prior art; cite the source by name)
- **Why deferred** (the actual reason, not hand-waving)
- **Signal to reconsider** (concrete condition that would flip the decision)

If the reason to defer is "we don't need it yet," say *exactly* what would make us need it. "Maybe later" is not a signal; "when scans exceed 3 minutes" is.
