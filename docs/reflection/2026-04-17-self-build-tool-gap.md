# Second self-build attempt — tool-gap discovery *(2026-04-17)*

**Integration commits:** pending

Two more runs after the first self-build surfaced the "plan already implemented" no-op. The goal this time was a real, scoped plan (fix a test-hygiene bug the first run surfaced) that would produce a concrete diff. Neither run produced a diff — they exposed three honest tool gaps instead. The fix was applied by hand afterward, and the runs' real value is the three findings.

## Setup

Same droplet, same daemon, new plan. Two plan iterations:

- **v1 (line-number reference)** — "find the test at line 2229 and add these env-var calls." 100 lines.
- **v2 (anchor strings)** — "find this exact old_string, replace with this exact new_string." 92 lines.

Both targeted one test in `crates/agentd/src/server.rs` (file is ~100KB / ~2700 lines).

## Run 5 (v1, 136s, 5 tool calls)

Agent read the plan, then called `file_read` on server.rs **five times in a row**. No edits, no report written. The agent's reasoning trace (in the daemon log) showed it fixating on "find the test at line 2229" — it dumped the whole file, couldn't locate line 2229 in the blob of text it got back, and re-read hoping for different output.

**Finding 1 — `file_read` has no offset/limit.** The tool always returns the full file (up to 1MB). For a 100KB Rust source file, the LLM gets a wall of text with no navigation affordance. Claude Code's `Read` tool takes `offset` and `limit`; aaOS's does not. When a plan references "line N", the LLM has no way to seek there — it scrolls, loses context, and gives up.

## Run 6 (v2, 272s, 3 tool calls)

Rewrote the plan to give exact anchor strings (old_string / new_string pairs) instead of line numbers. Agent read the plan, read server.rs twice, then **stopped emitting tool calls entirely** for ~4 minutes while "thinking" in the model's internal state. Bootstrap wrapped up with no output. No edits, no report.

**Finding 2 — no `file_edit` tool.** The plan described the fix as "replace old_string with new_string" (Edit-style), but aaOS only has `file_write` which rewrites the entire file from a `content` parameter. For a surgical 3-line change in a 100KB file, the agent would have to emit all 100KB back in one `file_write` call — roughly 25,000 output tokens, well beyond the builder role's `max_output_tokens: 8000` cap. The agent almost certainly started composing that file_write, hit the output cap mid-stream, and stalled.

**Finding 3 — plan format mismatch.** Plans written in the Edit-style idiom ("find X, replace with Y") don't map onto a tool set that only has whole-file write. A plan author can't just prescribe edits — they have to prescribe total file rewrites, which for a large file forces either a huge token budget or a different tool entirely.

## What shipped (manually, after the runs)

The test-hygiene fix itself was correct; I applied it by hand and committed. Details:

1. `AAOS_ROLES_DIR` set to a **nonexistent** path (not an empty directory) inside the `submit_streaming_writes_events_then_end_frame` test. Note: `RoleCatalog::load_from_dir` returns `Ok(empty_catalog)` for an empty dir, which still wires a `PlanExecutor`. Only a load failure leaves `plan_executor` as `None`.
2. A `ROLES_DIR_LOCK: Mutex<()>` in the test module, held by both `submit_streaming_writes_events_then_end_frame` and `submit_streaming_uses_plan_executor_when_catalog_loaded`. Without this, Cargo's default parallel test runner lets the two tests race on the process-global env var and one observes the other's setting.

Verified in both environments:
- A8 (no host `/etc/aaos/roles/`): test passed before, still passes.
- DO droplet (with `/etc/aaos/roles/` populated from the `.deb`): test failed before, passes now.

## Queued for follow-up (signal fired)

Three real tool-layer improvements are now pre-requisites for meaningful self-build loops on anything bigger than a one-file change:

1. **`file_read` takes `offset` + `limit`.** Mirror Claude Code's `Read` signature. The LLM can then page through a large file under its own control instead of drowning in a whole-file dump.
2. **`file_edit` tool.** An explicit find/replace primitive — `{ path, old_string, new_string }` semantics, same as the Edit tool in Claude Code / Cursor / every modern coding agent. Refuses if `old_string` is non-unique. Makes surgical edits tractable without blowing the output budget.
3. **Builder role `max_output_tokens` is not the lever** — giving the LLM 25k of output budget so it can dump an edited server.rs is bad architecture. The right answer is tool #2 above.

None of these is urgent for the Debian-derivative milestone; all three are urgent for the self-build narrative. Added to `docs/ideas.md`.

## What this didn't change

- `cargo_run` tool still works as designed on the droplet. Three cargo invocations ran successfully across runs 5 and 6 before the agent stalled on file-edit work.
- The `builder` role YAML still holds up — capability grants expanded correctly, iteration budget (40 turns) was not exhausted. The failure was the agent running out of **useful tools**, not running out of turns.
- The "agent recognizes a no-op" discipline from run 4 still held: neither run 5 nor run 6 fabricated edits or lied about success.

## Cost

Roughly 7 minutes of DeepSeek LLM use across runs 5 and 6 combined. **[token-math estimate] well under $0.10.** Dashboard not checked yet.

## Takeaway worth lifting

**Self-build is tool-bound, not model-bound.** The LLM's reasoning was fine in both runs — it read the plan, recognized the pattern, and knew what it wanted to do. It failed at the mechanical step of emitting the edit because the tool surface forced a 100KB rewrite instead of a 3-line patch. The next self-build loop needs `file_edit` and bounded `file_read` before it's worth running again.
