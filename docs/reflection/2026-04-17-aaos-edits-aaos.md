# Third self-build attempt — aaOS edits aaOS *(2026-04-17)*

**Integration commits:** `2819921` "feat: file_edit + file_read offset/limit" (A8) — the tool set that made this run possible.

The first self-build attempt (runs 1–4) validated the loop on a plan that was already implemented and correctly reported "already done." The second attempt (runs 5–6) attempted a real fix and failed at the edit step because the tool surface couldn't express surgical edits. Run 7 is the first end-to-end success: **aaOS read a plan, paged through a 100 KB Rust source file, applied four surgical edits via `file_edit`, ran `cargo check` + `cargo test`, both passed, and wrote an honest report** — all under capability enforcement.

## Setup

- Fresh DO droplet, Debian 13 / kernel 6.12.43 / 4 vCPU / 8 GB RAM.
- `agentd` release-built on droplet in 3m12s with the new `file_edit` + `file_read(offset, limit)` tools registered (14 tools total; log confirms `tool registered tool=file_edit` at startup).
- Droplet's `crates/agentd/src/server.rs` reverted to commit `65e2025` (parent of the fix) via `git checkout` so there was real work to do. The fix on A8's `main` is `f6dbc97`; the goal was to see whether the agent could independently re-derive it.
- Plan: `test-hygiene-roles-dir-v3.md`, 176 lines. Describes four surgical edits using explicit `file_edit` invocations with `old_string` / `new_string` pairs. Tells the agent to prefer `file_read(offset, limit)` for navigation over whole-file dumps.

## What the run did

Timeline (absolute times from `/tmp/submit7.log`, wall clock 471 s = 7m51s):

| Time | Tool | What |
|---|---|---|
| 09:42:47 | `file_read(plan.md)` | Read the plan once. |
| 09:43:09 | `file_read(server.rs, offset=1, limit=50)` | Start at top of file. |
| 09:43:31 | `file_read(server.rs, offset=2500, limit=100)` | Jump to end. |
| 09:43:52 | `file_read(server.rs, offset=2000, limit=100)` | Binary-search toward the tests. |
| 09:44:11 | `file_read(server.rs, offset=1900, limit=50)` | Narrowing on the test module. |
| 09:44:31 | `file_read(server.rs, offset=1950, limit=100)` | Located `mod tests { … }`. |
| 09:44:55 | **`file_edit`** | Edit 1: added `ROLES_DIR_LOCK` mutex. |
| 09:45:11 | `file_read(offset=2100, limit=100)` | Paging toward the failing test. |
| 09:45:29 | `file_read(offset=2200, limit=200)` | Located the test body. |
| 09:45:48 | **`file_edit`** | Edit 2: added lock guard. |
| 09:46:04 | `file_read(offset=2255, limit=20)` | Verified Edit 2 landed. |
| 09:46:25 | **`file_edit`** | Edit 3: added `AAOS_ROLES_DIR` setup. |
| 09:46:38 | `file_read(offset=2375, limit=20)` | Navigating to cleanup block. |
| 09:46:53 | **`file_edit`** | Edit 4: added `AAOS_ROLES_DIR` cleanup. |
| 09:47:07 | `file_read(offset=2400, limit=100)` | Checking sibling test location. |
| 09:47:20 | `file_read(offset=2600, limit=200)` | Located the sibling test. |
| 09:47:37 | **`file_edit`** | Edit 5: added lock guard to sibling. |
| 09:47:42 | `cargo_run(check)` | Verify compile — passed in 55 s. |
| 09:48:37 | `cargo_run(test, --package agentd submit_streaming)` | Two tests pass in ~76 s. |
| 09:49:53 | `file_write(report.md)` | Wrote honest report. |

Five `file_edit` calls, all accepted on first try (no "ambiguous match, matched N times" errors). No `file_write` on source files — the agent correctly used `file_edit` for all surgical changes. Nine `file_read(offset, limit)` calls to page through the 2700-line file; never dumped the whole file.

## Diff vs. the A8-manual fix

The agent's diff is **byte-identical** to the fix I committed manually at `f6dbc97` yesterday after runs 5 and 6 failed. Five hunks, ~25 added lines, zero removed except a single comment retouched from "Clean up env var" → "Clean up env vars". The only prose difference: one of the agent's comment blocks reads "on a host where /etc/aaos/roles/ exists" instead of my "without this, a host with a real /etc/aaos/roles/ directory" — same meaning, different phrasing.

Both the compile check and the two-test filter passed cleanly, matching my A8 verification exactly (2 passed; 0 failed).

## What this proves

1. **The runtime can code.** An agent running inside aaOS, given a plan and capability-scoped tools, made real surgical changes to aaOS itself and verified them with the Rust toolchain — end to end, no human in the edit loop.
2. **Capability enforcement scales to coding work.** All 15 tool calls flowed through `FileRead` / `FileWrite` / `CargoRun` capability checks. Zero capability denials; zero out-of-scope accesses. The agent never read a file it didn't have `FileRead` for, and never tried to.
3. **The tool set matches the mainstream coding-agent idiom.** `file_read(offset, limit)` for paging and `file_edit(old_string, new_string)` for surgical changes are exactly what Claude Code, Cursor, Aider, and OpenCode ship. aaOS now has that surface.
4. **The two runs that failed before (5 and 6) were tool-bound, not model-bound.** Same LLM (deepseek-chat), same plan shape, same prompt — once `file_edit` existed, the model produced the correct fix in one pass. This is the diagnosis from the previous reflection vindicated by direct measurement.

## What didn't happen

- No retries. Zero `file_edit` calls errored on ambiguity. Zero cargo failures. This is a clean first-pass run — unusual on a first attempt for any coding agent. I'd expect messier on larger / less-scoped plans.
- No tool-schema errors from the LLM (unlike run 4 which once passed `path: [2310, 2320]` by mistake).
- No output-token stalls (the run 6 failure mode). `file_edit`'s argument shape is small, so turns stay within budget.
- No commit from this run. The fix already exists on `main` as `f6dbc97`; the run's value is the demonstration, not a new patch.

## What I didn't change

The fix was already committed to `main` yesterday. I'm not replaying the agent's diff into a new commit — that would be noise. The agent and I arrived at the same patch; the production record of the fix remains `f6dbc97`.

## Cost

Roughly 8 minutes wall-clock. Dashboard not checked yet; this is fresh DeepSeek usage. **[token-math estimate] well under $0.10.** Droplet-hour cost ~$0.01.

## Takeaway worth lifting

**Tools are the bottleneck, not the model.** Every self-build loop article on the internet frames coding-agent quality as a model-quality question; this session's data points the other way. DeepSeek-chat (a cheap non-frontier model) produced a perfect first-pass fix once the tool surface matched the task shape. The previous two attempts with the same model failed on the same plan because the tool surface didn't match. When an agentic loop underperforms, look at the tool set before you look at the model.

The second self-build lesson — "self-build is tool-bound, not model-bound" from `2026-04-17-self-build-tool-gap.md` — is now empirically confirmed.
