#!/usr/bin/env bash
# Start each version's 1MCP in turn, capture its live tool listing, then stop it.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
py="$root/benchmark/.venv/bin/python"
for version in "$@"; do
  log="$root/target/live/1mcp-$version.capture.log"
  "$root/benchmark/integration/serve_1mcp.sh" "$version" >"$log" 2>&1 &
  pid=$!
  for _ in $(seq 1 200); do grep -q "Server is running" "$log" && break; sleep 3; done
  grep -q "Server is running" "$log" || { echo "$version: 1MCP did not start" >&2; kill "$pid"; exit 1; }
  "$py" "$root/benchmark/integration/live_setup.py" capture --out "$root/target/live/snapshot-$version.json"
  grep -h "backend.connection.failed" "$log" | sed -E 's/.*serverName\\":\\"([^\\]*).*/  failed server: \1/' | sort -u || true
  kill "$pid"; wait "$pid" 2>/dev/null || true
  pkill -f "agent@0.38.3 serve" 2>/dev/null || true
  sleep 2
done
