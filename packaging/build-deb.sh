#!/bin/sh
#
# Build the aaOS `.deb` in one step.
#
# `cargo deb -p agentd` ships `aaos-agent-worker` as an asset but cargo-deb
# (as of v3.6.3) has no pre-build hook, so the worker binary isn't built
# automatically. This wrapper builds both binaries first, then packs.
#
# Usage:
#   ./packaging/build-deb.sh [--features mcp] [extra cargo deb args]
#
# Context: BUG #1 from the 2026-04-18 e2e QA reflection found that a
# prior attempted fix using an invented `pre-build-command` field silently
# failed because cargo-deb doesn't recognize it. This script replaces
# that fix.

set -e

cd "$(dirname "$0")/.."

echo "[build-deb] building aaos-agent-worker (release)..."
cargo build --release -p aaos-backend-linux --bin aaos-agent-worker

echo "[build-deb] running cargo deb -p agentd $*..."
cargo deb -p agentd -- "$@"

echo "[build-deb] done:"
ls -la target/debian/*.deb
