#!/usr/bin/env bash
# Serve one MCPEvol version (original | iter3 | iter5) through 1MCP on 127.0.0.1:3050.
# 1MCP is pinned to 0.38.3: 0.39.x rewrites public tool names (for example `@scope/name` becomes
# `_scope/name...`), which breaks the `<server>_1mcp_<tool>` routes recorded in the snapshots.
# Config files come from `live_setup.py mcp-config` under target/live/.
set -euo pipefail
version="$1"
root="$(cd "$(dirname "$0")/../.." && pwd)"
config="$root/target/live/1mcp-$version.json"
[ -f "$config" ] || { echo "missing $config" >&2; exit 1; }
exec npx -y @1mcp/agent@0.38.3 serve --port "${PORT:-3050}" --config "$config"
