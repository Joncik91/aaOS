#!/bin/sh
#
# Build the aaOS `.deb` in one step with MCP support baked in.
#
# `cargo deb -p agentd` ships `aaos-agent-worker` as an asset but cargo-deb
# (as of v3.6.3) has no pre-build hook, so the worker binary isn't built
# automatically. This wrapper builds both binaries first, then packs.
#
# Features: `agentd` is built with `--features mcp` by default so both the
# MCP client (external MCP servers register into the tool registry as
# `mcp.<server>.<tool>`) and the loopback MCP server (`127.0.0.1:3781` —
# Claude Code / Cursor / any MCP client can delegate goals to aaOS) are
# on out of the box. Set AAOS_BUILD_FEATURES to override (empty string
# disables MCP).
#
# Usage:
#   ./packaging/build-deb.sh [extra cargo deb args]
#   AAOS_BUILD_FEATURES="" ./packaging/build-deb.sh   # MCP off
#   AAOS_BUILD_FEATURES="mcp namespaced-agents" ./packaging/build-deb.sh
#
# Context: BUG #1 from the 2026-04-18 e2e QA reflection found that a
# prior attempted fix using an invented `pre-build-command` field silently
# failed because cargo-deb doesn't recognize it. This script replaces
# that fix.

set -e

cd "$(dirname "$0")/.."

: "${AAOS_BUILD_FEATURES:=mcp}"

echo "[build-deb] building aaos-agent-worker (release)..."
cargo build --release -p aaos-backend-linux --bin aaos-agent-worker

if [ -n "$AAOS_BUILD_FEATURES" ]; then
  echo "[build-deb] running cargo deb -p agentd --features '$AAOS_BUILD_FEATURES' $*..."
  cargo deb -p agentd -- --features "$AAOS_BUILD_FEATURES" "$@"
else
  echo "[build-deb] running cargo deb -p agentd (no features) $*..."
  cargo deb -p agentd -- "$@"
fi

echo "[build-deb] done:"
ls -la target/debian/*.deb
