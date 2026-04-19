# S2D — Spec-to-Diff Reviewer

A git pre-commit check that flags spec requirements which appear unimplemented in the staged diff.

Scope: the class of bug that unit tests + `cargo fmt` + `cargo clippy` cannot catch — where the diff *compiles and passes tests* but the feature is structurally missing something the spec asked for (e.g., "forward capability tokens across the broker" is in the spec but no token-forwarding code is in the diff).

## What it does

On every `git commit`:

1. Look at staged `.rs` files. If none, skip.
2. Pick a candidate spec — `S2D_SPEC` env override, else newest `docs/phase-*-plan.md` or `docs/phase-*-design.md`.
3. Send spec + staged diff to Claude Haiku with a fixed review prompt.
4. If Haiku replies `S2D_OK`, the commit proceeds.
5. Otherwise, print the reviewer's gap list and block the commit.

## Activation

Run once per clone:

```sh
./scripts/setup-hooks.sh
export ANTHROPIC_API_KEY=sk-ant-...
```

The key goes in your shell env, not in-repo. The hook is a no-op (with a warning) if the key is unset, so fresh clones don't break.

## Override paths

S2D is advisory — every override has a one-line escape hatch:

| Situation | How |
|-----------|-----|
| One-off refactor that deliberately diverges from any spec | `S2D_DISABLE=1 git commit ...` |
| Non-code commit where S2D would waste cycles | (auto — no `.rs` files staged = skip) |
| Reviewer is flagging a false positive and you've read + disagreed | `git commit --no-verify` |
| Want to aim at a specific spec | `S2D_SPEC=docs/phase-f-b4-plan.md git commit ...` |
| Want a different model | `S2D_MODEL=claude-sonnet-4-6 git commit ...` |

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

One Haiku API call per commit. At Haiku pricing with typical spec + diff sizes: ~$0.001 per commit, ~3-5s latency. If this becomes a friction point, flip `S2D_DISABLE=1` in the shell or unset the API key.

## Tuning

The hook caps diff input at 120k chars. Specs are sent in full. If your diffs are routinely larger, either split commits (preferred) or raise the cap in `scripts/s2d-review.sh`.

## Failure modes

- **API down or timeout:** hook prints a warning and exits 0 — commits are never blocked by transport problems.
- **Malformed response:** same — warning, exit 0.
- **`jq` or `curl` missing:** warning + skip. Install: `apt install jq curl`.
- **Spec has no requirements this diff would plausibly implement:** Haiku should reply `S2D_OK`. If it false-positives consistently on small refactor commits, either override per-commit or disable with `S2D_DISABLE=1`.
