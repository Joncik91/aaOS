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

# Launch dashboard in a separate terminal window
# The dashboard auto-exits when the container stops (docker logs -f ends)
DASH_CMD="docker logs -f aaos-run 2>&1 | python3 $DASHBOARD; echo ''; echo 'aaOS container stopped. Press Enter to close.'; read"

if command -v xfce4-terminal >/dev/null 2>&1; then
    xfce4-terminal --title "aaOS Dashboard" --geometry 120x40 -e "bash -c '$DASH_CMD'" &
elif command -v gnome-terminal >/dev/null 2>&1; then
    gnome-terminal --title "aaOS Dashboard" --geometry 120x40 -- bash -c "$DASH_CMD" &
elif command -v xterm >/dev/null 2>&1; then
    xterm -title "aaOS Dashboard" -geometry 120x40 -e "bash -c '$DASH_CMD'" &
else
    echo "No terminal emulator found. Run manually in another terminal:"
    echo "  docker logs -f aaos-run 2>&1 | python3 $DASHBOARD"
fi

DASH_PID=$!
echo "Dashboard launched (PID $DASH_PID)"
echo "Container running. Ctrl+C here to stop."
echo ""

# Wait for container to finish, clean up on Ctrl+C
trap 'echo ""; echo "Stopping aaOS..."; docker stop aaos-run >/dev/null 2>&1; kill $DASH_PID 2>/dev/null; exit 0' INT TERM

# Wait for container to exit
docker wait aaos-run >/dev/null 2>&1

echo ""
echo "aaOS finished. Output files:"
ls -la "$SCRIPT_DIR/output/" 2>/dev/null
