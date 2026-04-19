# S2D — Spec-to-Diff Reviewer

A git pre-commit check that flags spec requirements which appear unimplemented in the staged diff.

Scope: the class of bug that unit tests + `cargo fmt` + `cargo clippy` cannot catch — where the diff *compiles and passes tests* but the feature is structurally missing something the spec asked for (e.g., "forward capability tokens across the broker" is in the spec but no token-forwarding code is in the diff).

## What it does

On every `git commit`:

1. Look at staged `.rs` files. If none, skip silently.
2. **Detect a candidate spec**, in this priority order:
   - `S2D_SPEC` env var (explicit override).
   - `Plan: <path>` or `Spec: <path>` trailer in the commit message (read from `.git/COMMIT_EDITMSG`).
   - Any `docs/phase-*-plan.md` / `docs/phase-*-design.md` reference mentioned *inside the staged diff* (code comment, doc link, etc.).
   - **None of the above → skip silently.** S2D will not pick an arbitrary plan to second-guess a bug fix or refactor.
3. Send spec + staged diff to `claude -p --model haiku` with a fixed review prompt.
4. If Haiku replies `S2D_OK`, the commit proceeds.
5. Otherwise, print the reviewer's gap list and block the commit.

## Activation

Run once per clone:

```sh
./scripts/setup-hooks.sh
```

S2D shells out to `claude -p` (the Claude Code CLI), which inherits whatever auth your interactive Claude Code session already uses. No separate API key, no `ANTHROPIC_API_KEY` env var, no in-repo credentials.

If `claude` isn't on `$PATH` (e.g., on a bare CI runner), the hook prints a warning and skips — fresh clones don't break.

## Override paths

S2D is advisory — every override has a one-line escape hatch:

| Situation | How |
|-----------|-----|
| Bug fix, refactor, or any commit not implementing a phase plan | (auto — no spec referenced = skip silently) |
| Non-code commit where S2D would waste cycles | (auto — no `.rs` files staged = skip) |
| Plan-driven commit, and you want S2D to review against plan X | put `Plan: docs/phase-X-plan.md` in the commit message, OR set `S2D_SPEC=docs/phase-X-plan.md`, OR cite the plan path in a code comment inside the diff |
| Reviewer is flagging a false positive and you've read + disagreed | `git commit --no-verify` |
| Want a different model | `S2D_MODEL=sonnet git commit ...` |
| Kill the hook entirely for one commit | `S2D_DISABLE=1 git commit ...` |

## What it catches (empirically)

From the 2026-04-19 Phase F-b/3 sub-project work, S2D would have caught:
- **T6 gap:** spec said "worker uses forwarded capability tokens"; diff built an empty `CapabilityRegistry` — missing token forwarding.
- **Gap C:** spec said "workspace accessible inside worker"; diff added `PolicyDescription.workspace` but no bind-mount step in `clone_and_launch_worker`.
- **`extract_capability_roots` miss:** writer's `file_write: /data/compare.md` capability wasn't extracted into bind-mount targets — spec listed declared output paths as scope, diff omitted them.

All three shipped through unit tests + CI + code review and were caught only by production droplet runs.

## What it does NOT catch

- Semantic bugs where the marker is present but wrong (e.g., `ctx.permits(handle, wrong_agent_id, ...)`).
- Race conditions, timing bugs, resource leaks.
- "Feature works but not on the code path production actually uses" (that's the inline-subtask-bypass class — needs droplet verification).
- Bugs in code that was never specced.

The droplet / production exercise remains the final validator. S2D just raises the floor on "did you forget the thing the spec said to do?"

## Cost + latency

One `claude -p` invocation per commit. Uses whatever subscription / quota the Claude Code CLI is already attached to — no separate billing line, no token-math to reason about. Latency is ~30-60s per commit (CLI cold-start + one LLM round-trip; Haiku by default). Rapid-fire commits can be friction; flip `S2D_DISABLE=1` in the shell or use `--no-verify` to skip per-commit.

## Tuning

The hook caps diff input at 120k chars. Specs are sent in full. If your diffs are routinely larger, either split commits (preferred) or raise the cap in `scripts/s2d-review.sh`.

## Failure modes

- **`claude` CLI hangs or errors:** bounded by `S2D_TIMEOUT` (default 60s). On timeout or empty output the hook prints a warning and exits 0 — commits are never blocked by transport problems.
- **`claude` not on PATH:** warning + skip. Install Claude Code, or set `S2D_DISABLE=1`.
- **Spec has no requirements this diff would plausibly implement:** the reviewer should reply `S2D_OK`. If it false-positives consistently on small refactor commits, either override per-commit or disable with `S2D_DISABLE=1`.
