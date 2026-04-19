#!/bin/sh
# S2D — Spec-to-Diff reviewer.
#
# Pipes the current staged diff + the most recently modified spec doc
# through Claude Code (the `claude -p` CLI) and asks: "do the staged code
# changes cover every requirement the spec calls out, or are any
# requirements missing / hallucinated?"
#
# Uses the `claude` CLI rather than a raw Anthropic API call so it
# inherits whatever auth the interactive Claude Code session already has.
# No separate API key required.
#
# Exit codes:
#   0  — S2D found nothing to flag OR review was skipped (missing tool /
#        no code changes staged / no candidate spec).
#   1  — Reviewer reported gaps. The hook prints the reviewer's notes
#        and blocks the commit; operator fixes the gap or re-runs with
#        `git commit --no-verify` to override.
#
# Environment:
#   S2D_DISABLE=1       skip the check entirely (useful for docs-only
#                        or refactor commits).
#   S2D_MODEL           override the claude CLI's --model flag.
#                        Default: the CLI's own default (Haiku-class is
#                        fine; specs + diffs are small).
#   S2D_SPEC            absolute path to the spec file to review against.
#                        If unset, auto-detect the most recently modified
#                        file under docs/phase-*-plan.md or docs/phase-*-design.md.
#   S2D_TIMEOUT         seconds to wait for the CLI before giving up.
#                        Default 120. (Cold-start `claude -p` can take
#                        ~50-60s on first invocation of a session.)

set -e

cd "$(git rev-parse --show-toplevel)"

if [ "${S2D_DISABLE:-0}" = "1" ]; then
    exit 0
fi

# 1. No staged code? Nothing to review.
staged_rs=$(git diff --cached --name-only --diff-filter=ACMR | grep -E '\.rs$' || true)
if [ -z "$staged_rs" ]; then
    exit 0
fi

# 2. claude CLI present?
if ! command -v claude >/dev/null 2>&1; then
    echo "warn: claude CLI not found on PATH — S2D spec-to-diff review skipped" >&2
    echo "      install Claude Code, or S2D_DISABLE=1 to silence this warning" >&2
    exit 0
fi

# 3. Find a candidate spec, but ONLY if this commit is plausibly
#    implementing one. Previously the hook picked the newest
#    phase-*-plan.md regardless of whether the diff had anything to
#    do with it — which false-positived on bug fixes, soak-test fixes,
#    cross-cutting refactors, etc. (soak-test finding 2026-04-19).
#
# Detection order (first match wins):
#   (a) S2D_SPEC env var — explicit operator override
#   (b) Commit message — set `git config commit.template` with a
#       "Plan: <path>" or "Spec: <path>" trailer, OR paste one into
#       the commit message while editing. The hook reads
#       .git/COMMIT_EDITMSG (populated by `git commit -m ...` + the
#       editor-opened template).
#   (c) Diff content — if the staged diff *mentions* a
#       docs/phase-*-plan.md or docs/phase-*-design.md path, that's
#       the spec this commit claims to implement (typically via a
#       block comment citing the plan, or a roadmap/reflection entry
#       link).
#
# If none of (a), (b), (c) matches: skip the review. A plan-unrelated
# commit (bug fix, refactor, docs tweak) should not burn a Haiku call
# just to get flagged against an arbitrary doc.
spec_path="${S2D_SPEC:-}"

if [ -z "$spec_path" ]; then
    # (b) Commit message / trailer.
    if [ -f .git/COMMIT_EDITMSG ]; then
        spec_path=$(grep -iE '^(Plan|Spec): ' .git/COMMIT_EDITMSG 2>/dev/null \
            | head -1 | sed -E 's/^[^:]+: *//' | tr -d ' ')
    fi
fi

if [ -z "$spec_path" ]; then
    # (c) Scan the diff for a phase-*-plan.md or phase-*-design.md
    #     reference. Matches paths mentioned in added code/comment lines.
    spec_path=$(git diff --cached -- '*.rs' '*.md' '*.toml' \
        | grep -oE 'docs/phase-[a-z0-9_-]+-(plan|design|qa-plan)\.md' \
        | head -1 || true)
fi

if [ -z "$spec_path" ] || [ ! -f "$spec_path" ]; then
    # No spec claimed → not a plan-driven commit. Skip silently.
    # Operators who want S2D on an ad-hoc commit: set S2D_SPEC=...
    # or add `Plan: docs/phase-...-plan.md` to the commit message.
    exit 0
fi

timeout="${S2D_TIMEOUT:-120}"

# 4. Build the review payload. Bounded so the prompt fits comfortably:
#    - spec: full file (specs are the thing we want the reviewer to read)
#    - diff: staged diff of .rs files only, truncated at 120k chars
diff_content=$(git diff --cached -- '*.rs')
diff_bytes=$(printf '%s' "$diff_content" | wc -c)
if [ "$diff_bytes" -gt 120000 ]; then
    diff_content=$(printf '%s' "$diff_content" | head -c 120000)
    diff_content="$diff_content

[... diff truncated at 120k chars ...]"
fi
spec_content=$(cat "$spec_path")

prompt=$(cat <<EOF
You are a senior code reviewer performing a Spec-to-Diff (S2D) check.

You are given:
1. A spec document — what the code was supposed to do.
2. A git diff of the staged changes in a Rust codebase.

Your job: flag REQUIREMENTS from the spec that do NOT appear to be
implemented in the diff.

Scope rules:
- Only flag things the spec explicitly requires. Don't invent requirements.
- A requirement is "covered" if the diff contains plausible markers of
  it (a new function, a new match arm, a new error variant, a new field,
  an audit event emit, a capability check, etc.). You don't need to
  verify correctness — just presence.
- Don't complain about style, naming, or minor improvements. That's not
  your job.
- If the diff is a small, targeted subset of the spec (e.g. one task
  out of 14), that is NORMAL and fine. Only flag gaps between
  requirements that the diff CLAIMS to implement and their actual presence.
- If unsure whether something is in scope, err on the side of NOT flagging.

Output format (strict):
- If every spec requirement the diff claims to implement is covered,
  reply EXACTLY:
    S2D_OK
  and nothing else.
- Otherwise, reply with a bullet list of gaps. Each bullet is ≤2 lines:
    - <requirement summary>: <what is missing / where you expected it>
  Then a final line:
    S2D_GAPS: <count>

Do not restate the diff. Do not praise the code. Be terse and specific.

---

SPEC ($spec_path):

$spec_content

---

DIFF (staged, .rs files):

$diff_content
EOF
)

# 5. Invoke claude -p. Pipe the prompt via stdin to avoid shell arg limits.
if command -v timeout >/dev/null 2>&1; then
    runner="timeout $timeout"
else
    runner=""
fi

claude_args="-p --model ${S2D_MODEL:-haiku}"

# `claude -p` expects the prompt as either an argument or on stdin. Use
# stdin so we never hit argv size limits on large specs.
text=$(printf '%s' "$prompt" | $runner claude $claude_args 2>&1 || true)

if [ -z "$text" ]; then
    echo "warn: S2D — empty claude CLI output; review skipped" >&2
    exit 0
fi

# 6. Decide. "S2D_OK" anywhere on a line of its own = pass.
if printf '%s' "$text" | grep -q '^S2D_OK'; then
    exit 0
fi

# Otherwise block + print the gaps.
echo "" >&2
echo "S2D spec-to-diff review flagged gaps (spec: $spec_path):" >&2
echo "" >&2
printf '%s\n' "$text" >&2
echo "" >&2
echo "Fix the gaps, or re-commit with --no-verify to override (a refactor" >&2
echo "that deliberately diverges from the spec is a legitimate override)." >&2
echo "Silence S2D for one commit: S2D_DISABLE=1 git commit ..." >&2
exit 1
