#!/bin/bash
# aaOS Bootstrap Runner with Live Dashboard
# Usage: ./run-aaos.sh "Your goal here"
#
# Requires: DEEPSEEK_API_KEY or ANTHROPIC_API_KEY in environment

set -e

GOAL="${1:-Fetch https://news.ycombinator.com and write a summary of the top 5 stories to /output/summary.txt}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
DASHBOARD="$SCRIPT_DIR/tools/observability/dashboard.py"
DETAIL_LOG="$SCRIPT_DIR/tools/observability/detail_log.py"

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
mkdir -p "$SCRIPT_DIR/output" "$SCRIPT_DIR/memory"

# Persistent memory: if AAOS_PERSISTENT_MEMORY=1, bind-mount the memory dir so
# SQLite episodic memory + the stable Bootstrap ID survive container restarts.
# Opt-in because persistent memory carries prompt-injection / bad-strategy risk.
MEMORY_MOUNT=""
if [ "${AAOS_PERSISTENT_MEMORY:-0}" = "1" ]; then
    MEMORY_MOUNT="-v $SCRIPT_DIR/memory:/var/lib/aaos/memory"
    echo "Persistent memory enabled at $SCRIPT_DIR/memory (Bootstrap memory will survive restarts)"
fi

echo "Starting aaOS..."
echo "Goal: $GOAL"
echo ""

# Start container in background
# Mask docs/ and README.md from the container's view of /src so
# self-reflection runs cannot synthesize answers from our own
# roadmap/architecture/ideas documents — only the source code is visible.
# Keeps the whole-repo mount (so new top-level files appear automatically)
# and overlays empty mounts on the excluded paths. Reversal: remove the
# two mask lines.
DOCS_MASK_DIR="$(mktemp -d)"
README_MASK_FILE="$(mktemp)"

docker run -d --name aaos-run \
    -e DEEPSEEK_API_KEY="${DEEPSEEK_API_KEY:-}" \
    -e ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY:-}" \
    -e AAOS_BOOTSTRAP_MANIFEST=/etc/aaos/manifests/bootstrap.yaml \
    -e AAOS_BOOTSTRAP_GOAL="$GOAL" \
    -e AAOS_MAX_CONCURRENT_INFERENCE="${AAOS_MAX_CONCURRENT_INFERENCE:-5}" \
    -e AAOS_SPAWN_AGENTS_BATCH_CAP="${AAOS_SPAWN_AGENTS_BATCH_CAP:-3}" \
    -e RUST_LOG="${RUST_LOG:-info}" \
    -v "$SCRIPT_DIR:/src:ro" \
    -v "$DOCS_MASK_DIR:/src/docs:ro" \
    -v "$README_MASK_FILE:/src/README.md:ro" \
    -v "$SCRIPT_DIR/output:/output" \
    $MEMORY_MOUNT \
    aaos-bootstrap >/dev/null

# Launch two terminals: a status dashboard + a detail log (think/tool narrative)
DASH_CMD="docker logs -f aaos-run 2>&1 | python3 $DASHBOARD; echo ''; echo 'aaOS container stopped. Press Enter to close.'; read"
DETAIL_CMD="docker logs -f aaos-run 2>&1 | python3 $DETAIL_LOG; echo ''; echo 'aaOS container stopped. Press Enter to close.'; read"

spawn_term() {
    local title="$1"; local geometry="$2"; local cmd="$3"
    if command -v xfce4-terminal >/dev/null 2>&1; then
        xfce4-terminal --title "$title" --geometry "$geometry" -e "bash -c '$cmd'" &
    elif command -v gnome-terminal >/dev/null 2>&1; then
        gnome-terminal --title "$title" --geometry "$geometry" -- bash -c "$cmd" &
    elif command -v xterm >/dev/null 2>&1; then
        xterm -title "$title" -geometry "$geometry" -e "bash -c '$cmd'" &
    else
        echo "No terminal emulator found for '$title'. Run manually: docker logs -f aaos-run | ..."
        return 1
    fi
}

spawn_term "aaOS Dashboard" "120x40" "$DASH_CMD"
DASH_PID=$!
spawn_term "aaOS Detail Log" "140x50" "$DETAIL_CMD"
DETAIL_PID=$!

echo "Dashboard launched (PID $DASH_PID); detail log launched (PID $DETAIL_PID)"
echo "Container running. Ctrl+C here to stop."
echo ""

# Wait for container to finish, clean up on Ctrl+C
trap 'echo ""; echo "Stopping aaOS..."; docker stop aaos-run >/dev/null 2>&1; kill $DASH_PID 2>/dev/null; kill $DETAIL_PID 2>/dev/null; exit 0' INT TERM

# Wait for container to exit
docker wait aaos-run >/dev/null 2>&1

echo ""
echo "aaOS finished. Output files:"
ls -la "$SCRIPT_DIR/output/" 2>/dev/null
