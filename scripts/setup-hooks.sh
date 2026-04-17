#!/bin/sh
# Activate the in-tree git hooks for this clone.
#
# Run once after `git clone`. Idempotent — safe to re-run.
#
# What this does:
#   - Points core.hooksPath at .githooks/ so pre-commit / pre-push
#     run the scripts committed to the repo instead of .git/hooks/.
#   - Warns if gitleaks isn't installed (the pre-commit hook needs it
#     to actually scan; without it the hook is a no-op with a warning).

set -e

cd "$(git rev-parse --show-toplevel)"

git config core.hooksPath .githooks
echo "core.hooksPath -> .githooks (OK)"

if ! command -v gitleaks >/dev/null 2>&1; then
    cat >&2 <<EOF

warn: gitleaks not installed — pre-commit secret scan will be skipped.
  Debian/Ubuntu:  apt install gitleaks
  macOS:          brew install gitleaks
  Other:          https://github.com/gitleaks/gitleaks

EOF
fi
