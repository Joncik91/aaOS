#!/bin/bash
# aaOS Bootstrap Runner with Live Dashboard
# Usage: ./run-aaos.sh "Your goal here"
#
# Requires: DEEPSEEK_API_KEY or ANTHROPIC_API_KEY in environment

set -e

GOAL="${1:-Fetch https://news.ycombinator.com and write a summary of the top 5 stories to /output/summary.txt}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
DASHBOARD="$SCRIPT_DIR/tools/dashboard.py"

# Check for API key
if [ -z "$DEEPSEEK_API_KEY" ] && [ -z "$ANTHROPIC_API_KEY" ]; then
    echo "Error: Set DEEPSEEK_API_KEY or ANTHROPIC_API_KEY"
    exit 1
fi

# Ensure image exists
if ! docker image inspect aaos-bootstrap >/dev/null 2>&1; then
    echo "Building aaos-bootstrap image..."
    docker build -t aaos-bootstrap -f "$SCRIPT_DIR/Dockerfile.bootstrap" "$SCRIPT_DIR"
fi

# Clean up any previous run
docker rm -f aaos-run 2>/dev/null || true
mkdir -p "$SCRIPT_DIR/output"

echo "Starting aaOS..."
echo "Goal: $GOAL"
echo ""

# Start container in background
docker run -d --name aaos-run \
    -e DEEPSEEK_API_KEY="${DEEPSEEK_API_KEY:-}" \
    -e ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY:-}" \
    -e AAOS_BOOTSTRAP_MANIFEST=/etc/aaos/manifests/bootstrap.yaml \
    -e AAOS_BOOTSTRAP_GOAL="$GOAL" \
    -e AAOS_MAX_CONCURRENT_INFERENCE="${AAOS_MAX_CONCURRENT_INFERENCE:-5}" \
    -e RUST_LOG="${RUST_LOG:-info}" \
    -v "$SCRIPT_DIR:/src:ro" \
    -v "$SCRIPT_DIR/output:/output" \
    aaos-bootstrap >/dev/null

# Give it a moment to start logging
sleep 0.5

# Stream logs through the dashboard until container stops
# trap ensures cleanup on Ctrl+C
trap 'echo ""; echo "Stopping aaOS..."; docker stop aaos-run >/dev/null 2>&1; exit 0' INT TERM

docker logs -f aaos-run 2>&1 | python3 "$DASHBOARD"

# Container exited on its own — show output
echo ""
echo "Container exited. Output files:"
ls -la "$SCRIPT_DIR/output/" 2>/dev/null
