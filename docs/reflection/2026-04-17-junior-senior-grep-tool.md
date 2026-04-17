# Junior-senior workflow — grep tool end to end *(2026-04-17)*

**Integration commits:** `7004b78` "feat: grep tool — ripgrep-backed search, capability-scoped", `533873e` "feat: grep tool production-ready" (SHAs post-rewrite; force-push on 2026-04-17 scrubbed droplet IPs from the commit trailers — content and trees are identical)

Two self-build runs (run 8, run 9) shipped a feature from plan to prod with a clear division of labor: the **junior** (aaOS builder role on a DO droplet) wrote the code; the **senior** (me, on A8) wrote the plans, reviewed the diffs, and committed. This is the first feature in the repo where aaOS is the actual author of the code — not a re-derivation of a hand-written fix (run 7) but a genuine new capability whose first draft came from the agent.

## Why this was the right next feature

Runs 5–6 surfaced that self-build is tool-bound, not model-bound. Runs 7–9 are the consequences: keep shipping tools that close gaps the agent surfaces, watch the agent do more of the work each round. The specific gap for run 8 was navigation — the agent had no search primitive, so when told "find where `submit_streaming_*` lives" it had to page through server.rs one 100-line window at a time. Adding `grep` was the obvious next step after `file_edit`, directly mirroring the Claude Code / Cursor / OpenCode tool set (Read, Edit, Grep, Bash).

## Run 8 — junior implements grep tool

**Timing:** 414 s wall clock, 25 tool calls.

**What the junior did:**
1. Read the plan once (`file_read`).
2. Oriented in `crates/aaos-tools/` via `file_list`.
3. Created `crates/aaos-tools/src/grep.rs` in one `file_write` call — 7081 bytes, matching the structure I'd sketched in the plan (async Tool trait, ripgrep spawn, capability check, timeout, output cap, 3 tests).
4. Added `pub mod grep;` and `pub use grep::GrepTool;` to `lib.rs` via two `file_edit` calls — both accepted first try (unique anchors).
5. Paged through `crates/agentd/src/server.rs` with 8 `file_read(offset, limit)` calls to find the three tool-registry setup blocks.
6. Attempted a `file_edit` with a long multi-line `old_string` covering all three registrations. The tool **refused**: `old_string matches 3 times in ...; refusing ambiguous edit. Pass replace_all=true to replace all, or extend old_string with more context to make it unique.`
7. **Recovered.** Instead of retrying with `replace_all: true` (the plan had suggested it explicitly), the agent shortened the `old_string` to just one unique line (`tool_registry.register(Arc::new(aaos_tools::CargoRunTool));`) and passed `replace_all: true` — functionally equivalent to the plan's suggestion but with a smaller, unambiguous anchor. All three sites were updated.
8. `cargo_run check` → passed.
9. `cargo_run test --package aaos-tools grep` → 3 passed.
10. `cargo_run test` (full workspace) → all green, 489 total (reported).
11. Wrote an accurate report.

**Senior review findings:**
- `--max-count` flag is per-file in ripgrep, not global. In-Rust `.take(MAX_MATCHES)` at the parse stage caps correctly anyway, so the contract holds — but the flag isn't doing what its name suggests. Noted in the commit message.
- `split_once(':')` parse doesn't handle filenames with embedded colons. Rare in Rust codebases; noted for follow-up.
- **The tests needed ripgrep on the host.** Senior's local test run failed 2/3 tests on A8 until `apt install ripgrep` landed. A8 had never installed it before; neither had the `.deb` metadata declared it as a dep.
- **The tool wasn't granted to any role.** Registered everywhere it needed to be, but no role's capability block included `"tool: grep"`, so no running agent could invoke it.
- **Doc drift.** Tool count in README + architecture still said 14.

Committed as `7004b78` (was `677a58f` pre-rewrite). Feature present but not prod-ready.

## Run 9 — junior makes grep prod-ready

**Timing:** 254 s wall clock, 24 tool calls. **Zero errors. Zero retries.**

**What the junior did:**
1. Read plan.
2. Read Cargo.toml, applied `file_edit` to add `ripgrep` to the `depends =` line.
3. Read builder.yaml, applied `file_edit` to add `"tool: grep"` to its capability list.
4. Paged through README.md with `file_read(offset, limit)` to locate the three edit sites (legend, crate description, tool table).
5. Applied 3 × `file_edit` to README.md. All first-try.
6. Paged through architecture.md. Applied 2 × `file_edit` (tool list line + new description paragraph).
7. Read grep.rs. Applied 3 × `file_edit` — one per test — to add `#[ignore]` attributes, matching the plan's pattern of "comment on the first, just the attribute on the other two."
8. `cargo_run check` → passed.
9. `cargo_run test --package aaos-tools` → all passed, 3 grep tests now ignored as intended.
10. `cargo_run test --package aaos-tools grep -- --ignored` → 3 passed (explicit run).
11. Wrote an accurate, structured report.

The junior's diff is 15 insertions, 4 deletions, exactly the shape the plan called for. No stray edits, no over-reach, no missed steps.

**Senior review:** verified full workspace test (478 passed, 0 failed, 11 ignored), no issues, committed as `533873e` (was `71ce3ec` pre-rewrite) with a `Co-Authored-By: aaOS builder role (ephemeral droplet, run 0e0d703e)` trailer.

## What this proves

1. **Junior-senior works.** The model where the senior (human) writes plans and reviews diffs while the junior (agent) does the mechanical edits is productive. Run 9 applied 8 edits across 5 files in 254 s. A careful human could do the same work in 5–10 minutes; the agent did it faster and under capability enforcement with a clean audit trail. The win isn't speed — it's that the senior never had to touch the edit keystrokes, only the review.
2. **The `file_edit` ambiguity check caught a real class of mistakes.** Run 8's first registration attempt would have, with a looser tool, silently replaced only the first of three occurrences. Instead the tool refused, the agent saw why, and picked a better anchor. This is exactly the bug-avoidance the Edit-tool idiom is supposed to deliver; first empirical confirmation in aaOS.
3. **Tests can outrun the tool surface.** Run 8's tests passed on the droplet (ripgrep installed) but failed on A8 (not installed). That's a documented bug the plan shipped with: the tests aren't `#[ignore]`-gated, so "my machine" passes and "a fresh machine" fails. Run 9 fixed it. The pattern is worth calling out: **tests that shell out to host binaries should be `#[ignore]`-gated, not assumed-available.** Same lesson the `cargo_run_check_on_probe_crate` test shipped with already.
4. **A small plan is better than a big plan.** Run 8's plan had 8 steps; run 9's plan had 8 smaller steps focused on cleanup. Run 9 was twice as fast per step as run 8 and had zero errors. The finding: the junior works best when each step is one `file_edit` with unambiguous anchors. Multi-step refactors with intertwined changes will be harder.

## What this didn't prove

- **Still one-shot-plans only.** Both runs executed a plan the senior wrote. Nothing in these runs shows the junior can *produce* a plan from a higher-level goal. The planner in aaOS today decomposes goals into roles, not into code edits.
- **No git.** The junior can't commit, push, or inspect history. Senior still owns the git path. Add `git_diff` + narrow `git_commit` tools and that becomes the next layer.
- **Still needs a pre-specified workspace.** The operator-stated-absolute-path pattern has worked every time but has to be explicit in the goal text. The Planner still defaults to per-run scratch dirs otherwise.

## Cost

Runs 8 and 9 combined: ~11 minutes of DeepSeek LLM use, ~50 tool calls, across about $0.10 of DeepSeek API [token-math estimate — dashboard not checked]. Droplet hour: $0.01.

## Takeaways worth lifting

**Well-scoped plans are the lever.** Three consecutive clean self-builds (run 7 test-hygiene fix, run 8 grep tool, run 9 grep prod-ready) shipped because the plan named exact anchors, exact edits, exact verification commands. When the plan gave a line number instead of an anchor (run 5), the agent flailed. When the plan described the edit in prose instead of `old_string`/`new_string` pairs (also run 5), the agent stalled.

**Tests that shell out need `#[ignore]`.** New pattern for the codebase: any test that invokes a host binary (cargo, rg, git) gets `#[ignore]` with a comment explaining what to install and how to run it manually. Otherwise `cargo test` on a fresh machine will flap.

The Claude Code / Cursor tool idiom (Read-with-offset, Edit, Grep, Bash) is the working coding-agent surface. aaOS now has 4 out of 4. Next expansion is a narrow `git_commit` so the junior can close its own loop into version control.
