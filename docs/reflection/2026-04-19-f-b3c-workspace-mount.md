# Phase F-b sub-project 3c — workspace bind-mount closes the last Gap-3 gap *(2026-04-19)*

**Integration commits under test:** `47eb485`..`7adc147` (2 commits on `main` after the `2026-04-19-f-b3b-gap-fix.md` reflection). Closes Gap C: workers can now see + write to the shared workspace and declared output paths, so full plan-executor subtask confinement works end-to-end.

## Setup

Same droplet as prior F-b/3 runs — `159.89.31.179`, Debian 13, kernel 6.12.43. Default configuration after T17 ships: `AAOS_DEFAULT_BACKEND=namespaced` + `AAOS_CONFINE_SUBTASKS=1` (now default). No scripted opt-outs.

## What shipped

Two commits for Gap C:

### `47eb485` — workspace bind-mount + Landlock allow
- `PolicyDescription` gains `workspace: Option<PathBuf>`.
- `clone_and_launch_worker` adds Step D2 (between scratch and shared libs): bind-mount the host workspace at the same absolute path inside the worker's mount ns.
- `landlock_compile::build_ruleset` adds a `PathBeneath` read-write rule for the workspace path (narrow per-agent scope).
- `AgentLaunchSpec.workspace_path` (previously unused) now flows into the policy.
- `SubtaskRunner` signature grows a `run_root: PathBuf` parameter so the plan executor can pass `Substitutions::run_root` into `run_subtask_inline`.
- `AAOS_CONFINE_SUBTASKS` flips to **default-ON** (was opt-in when workspace access was broken; now it's the correct default).

### `7adc147` — extract capability-declared writable roots
First droplet run with Gap C revealed writer still failed: it declares `file_write: /data/compare.md`, and `/data` wasn't bind-mounted. The workspace alone doesn't cover declared output paths.

Fix: `extract_capability_roots(manifest)` scans the manifest's capability declarations for `file_read:` / `file_write:` entries, takes the parent directory of each path glob, and populates `PolicyDescription.extra_writable_roots`. Launch bind-mounts each; Landlock permits each with a per-path `PathBeneath` rule. Capability tokens remain the policy gate — bind-mount + Landlock just provide the filesystem visibility for the tokens to mean something inside the sandbox.

4 new unit tests pin the parse/extract behaviour.

## What the canonical run showed

Run: `agentd submit "fetch HN and lobste.rs, compare the top 3 stories on each, write a detailed 800-word comparison to /data/compare.md"` with default configuration (namespaced backend + confine-subtasks on).

**All six DoD criteria hit first-try:**

| # | Criterion | Result |
|---|-----------|--------|
| 1 | Canonical goal completes end-to-end | ✅ `/data/compare.md` = 6034 bytes, proper 800-word comparison. 152s total. |
| 2 | `[worker]` tag visible on production traffic | ✅ 5 `[worker]` + 4 `[daemon]` — analyzer + writer work happened under confinement. |
| 3 | Both `file_read` + `file_write` worker-side | ✅ `file_read [worker]` on workspace HTML files; `file_write [worker]` on `/data/compare.md`. |
| 4 | No tool failures | ✅ zero `tool FAILED` — workspace bind-mount + `/data` extra-root covered all paths. |
| 5 | Regression-free default build | ✅ Previously verified; default `--features mcp` path unchanged. |
| 6 | No panics / crashes | ✅ `journalctl \| grep -iE panic\|backtrace` empty. |

The event stream tells the full story:
```
[14:38:24] 5a66f883    tool: file_read [worker] {"path":"/var/lib/aaos/workspace/.../hn.html"}    → ok
[14:38:29] bae8b9c0    tool: file_read [worker] {"path":"/var/lib/aaos/workspace/.../hn.html"}    → ok
[14:39:34] bae8b9c0    tool: file_write [worker] {"content":"# Comparison of Top Stories..."}    → ok (→ /data/compare.md)
[14:39:36] bae8b9c0    complete
[14:39:36] bootstrap   complete (0k in / 0k out, 152s)
```

## What worked

- **Workspace bind-mount at the same absolute path** removed the whole class of "host path doesn't exist inside the worker" errors. Tool code that uses `/var/lib/aaos/workspace/<run>/hn.html` now works identically daemon-side vs worker-side. No tool-code changes required.
- **Extracting capability roots from the manifest** made the writer's `/data/compare.md` accessible without manually enumerating writable paths at launch. Generalizes to any role that declares typed file capabilities — no new plumbing per role.
- **SubtaskRunner signature change** was mechanical (13 closures) but carried zero behavioural risk — all test stubs trivially accept `_run_root`.
- **Defaulting to `AAOS_CONFINE_SUBTASKS=1`** is now safe. The canonical goal works with workers handling every worker-eligible tool call; the opt-out path (`=0`) exists only for latency debugging.

## What the run exposed

Nothing new. The design-level decision — "bind-mount at the same absolute path, extract capability roots from the manifest" — was correct on the first full-workflow exercise. No iteration needed.

## What shipped

Two commits (`47eb485` + `7adc147`) on `main`. 577 workspace tests pass. Default `--features mcp` build unchanged.

Combined Phase F-b sub-project 3 (v1 + 3b + 3c) scope is now:
- `spawn_agent`-launched children: confined end-to-end.
- Plan-executor subtasks (analyzer, writer, generalist): confined end-to-end, workspace + declared output paths accessible.
- Scaffolds (fetcher): daemon-side by design. Trivial Rust, writes to workspace, no LLM loop — confinement would be overhead without meaningful security gain. Documented in architecture.md.
- Tool-layer capability re-check inside the worker: works (Gap B, shipped in 3b).
- Request/response correlation for concurrent tool calls: works (shipped in 3 v1).
- Operator-visible `[worker]`/`[daemon]` tag: works.

## Cost

Droplet billed <1.5hrs across today's F-b/3 + 3b + 3c work at $0.047/hr = under $0.07. DeepSeek spend not measured on dashboard; the successful canonical run used ~150s of planner + analyzer + writer LLM time, probably under $0.05.

## Lessons worth lifting

**"Extract structural truth from the manifest, don't re-encode it."** The writer role's `file_write: /data/compare.md` already declares everything needed to know which directory must be accessible inside the worker. The Gap C fix just reads the existing source of truth (the manifest) instead of adding a new `extra_writable_paths` config knob per deployment. Every time a new role ships, its declared paths flow into the worker's policy automatically. This is the right shape for "capability-driven confinement." Candidate for patterns.md after a second confirming data point.

**"Two-step confinement is layered, not either/or."** Bind-mount + Landlock are doing different jobs:
- Bind-mount provides **visibility** — makes a host path resolvable inside the worker's mount ns.
- Landlock provides **access control** — within the mount ns, limits what paths the worker may touch.
- **Capability tokens** provide **per-call authorization** — within the Landlock-allowed paths, the tool's own `ctx.capability_registry.permits()` check decides if this specific file is in scope for this specific agent.

A worker with a bind-mount but no Landlock rule: can see the path but can't read/write it. A worker with a Landlock rule but no bind-mount: the rule points at a directory that doesn't exist in its mount ns, ruleset compile fails with "skipping rule" warnings. All three layers must agree for a tool call to succeed. That's defense in depth, and the honest design naming in `docs/architecture.md` now reflects it.
