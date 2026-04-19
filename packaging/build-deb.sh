#!/bin/sh
#
# Build the aaOS `.deb` in one step with MCP support baked in.
#
# `cargo deb -p agentd` ships `aaos-agent-worker` as an asset but cargo-deb
# (as of v3.6.3) has no pre-build hook, so the worker binary isn't built
# automatically. This wrapper builds both binaries first, then packs.
#
# Features: `agentd` is built with `--features mcp,namespaced-agents` by
# default so the release .deb ships:
#
#   * mcp — MCP client (external MCP servers register as
#     `mcp.<server>.<tool>`) + loopback MCP server (127.0.0.1:3781)
#     exposing submit_goal / get_agent_status / cancel_agent.
#   * namespaced-agents — NamespacedBackend with Landlock + seccomp +
#     user/mount/pid namespaces.  Without this feature the runtime
#     silently falls back to InProcessBackend regardless of the
#     AAOS_DEFAULT_BACKEND env var — defeating the confinement the
#     postinst-generated /etc/default/aaos.example promises.
#
# Set AAOS_BUILD_FEATURES to override (empty string disables both).
#
# Usage:
#   ./packaging/build-deb.sh [extra cargo deb args]
#   AAOS_BUILD_FEATURES="" ./packaging/build-deb.sh         # no features
#   AAOS_BUILD_FEATURES="mcp" ./packaging/build-deb.sh      # mcp only
#
# Context: BUG #1 from the 2026-04-18 e2e QA reflection found that a
# prior attempted fix using an invented `pre-build-command` field silently
# failed because cargo-deb doesn't recognize it. This script replaces
# that fix.  The 2026-04-19 v0.0.1 droplet QA surfaced the namespaced
# feature gap: .deb was shipping without runtime-side confinement even
# though the postinst probe and env-file template promised it.

set -e

cd "$(dirname "$0")/.."

: "${AAOS_BUILD_FEATURES:=mcp,namespaced-agents}"

echo "[build-deb] gzipping debian/changelog (cargo-deb has no auto-gzip)..."
# Produce packaging/debian/changelog.Debian.gz via `gzip -9 -n` (no
# timestamp) so the .deb build is reproducible.  The plain `changelog`
# remains in the repo as the source; only the `.gz` ships in the .deb.
rm -f packaging/debian/changelog.Debian.gz
gzip -9 -n -c packaging/debian/changelog > packaging/debian/changelog.Debian.gz

echo "[build-deb] building aaos-agent-worker (release)..."
cargo build --release -p aaos-backend-linux --bin aaos-agent-worker

# Build agentd ahead of cargo-deb so we can strip both binaries before
# cargo-deb packs them.  `cargo deb --strip` exists but only runs
# reliably when it invokes the build itself; when cargo-deb finds a
# pre-built binary (as it must here, since the worker is a sibling
# crate) the strip step is skipped.  Strip explicitly instead.
if [ -n "$AAOS_BUILD_FEATURES" ]; then
  cargo build --release -p agentd --bin agentd --features "$AAOS_BUILD_FEATURES"
else
  cargo build --release -p agentd --bin agentd
fi
strip --strip-unneeded target/release/agentd target/release/aaos-agent-worker

if [ -n "$AAOS_BUILD_FEATURES" ]; then
  echo "[build-deb] running cargo deb --no-build -p agentd --features '$AAOS_BUILD_FEATURES' $*..."
  cargo deb --no-build -p agentd -- --features "$AAOS_BUILD_FEATURES" "$@"
else
  echo "[build-deb] running cargo deb --no-build -p agentd (no features) $*..."
  cargo deb --no-build -p agentd -- "$@"
fi

echo "[build-deb] done:"
ls -la target/debian/*.deb
