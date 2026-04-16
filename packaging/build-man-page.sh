#!/bin/sh
# Build packaging/agentd.1.gz from agentd.1.md using pandoc.
# Run before `cargo deb -p agentd` (the .gz is referenced as a cargo-deb asset).

set -e

cd "$(dirname "$0")"

if ! command -v pandoc >/dev/null 2>&1; then
  echo "error: pandoc is required. install with: apt-get install -y pandoc" >&2
  exit 1
fi

pandoc -s -t man agentd.1.md -o agentd.1
gzip -f -9 agentd.1

echo "built $(pwd)/agentd.1.gz"
