#!/usr/bin/env bash
# Build + stage the binary (via build.sh), then restart the running MCP server
# so it execs the fresh build. Claude Code respawns the server on next use.
set -euo pipefail
cd "$(dirname "$0")/.."

./plugin/build.sh

PID=$(pgrep -f 'plugin/bin/scrooge -r' || true)
if [ -n "$PID" ]; then
  kill $PID
  echo "restarted MCP server (killed $PID; Claude Code will respawn it)"
else
  echo "no MCP server running; it will spawn from the new binary on next use"
fi
