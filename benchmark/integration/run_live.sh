#!/usr/bin/env bash
# Run one live stage end to end: serve the matching 1MCP version, run the Rust harness (which
# launches benchmark/integration/rust_runner.py per strategy), then stop 1MCP.
#   benchmark/integration/run_live.sh <early|middle|late> <output-dir> [config]
# The provider key is read from OPENAI_ETHICS_KEY, loading it from ~/.zshrc when unset.
set -euo pipefail
stage="$1"; out="$2"
root="$(cd "$(dirname "$0")/../.." && pwd)"
config="${3:-$root/configs/live/$stage.toml}"
case "$stage" in early) version=original ;; middle) version=iter3 ;; late) version=iter5 ;;
  *) echo "unknown stage $stage" >&2; exit 2 ;; esac
if [ -z "${OPENAI_ETHICS_KEY:-}" ]; then
  OPENAI_ETHICS_KEY="$(zsh -ic 'print -rn -- $OPENAI_ETHICS_KEY' 2>/dev/null)"
fi
[ -n "$OPENAI_ETHICS_KEY" ] || { echo "OPENAI_ETHICS_KEY is not set" >&2; exit 1; }
export OPENAI_ETHICS_KEY
export MCP_DATA_DIR="$root/benchmark/src/custom/data" EVAL_DATA_DIR="$root/benchmark/src/custom/data"
export PATH="$HOME/.nvm/versions/node/v24.11.1/bin:$HOME/.local/bin:$PATH"

pkill -f "agent@0.38.3 serve" 2>/dev/null || true
mkdir -p "$root/target/live/logs"
log="$root/target/live/logs/1mcp-$stage-$(date +%Y%m%d%H%M%S).log"
"$root/benchmark/integration/serve_1mcp.sh" "$version" >"$log" 2>&1 &
mcp_pid=$!
trap 'kill $mcp_pid 2>/dev/null || true; pkill -f "agent@0.38.3 serve" 2>/dev/null || true' EXIT
for _ in $(seq 1 200); do grep -q "Server is running" "$log" && break; sleep 3; done
grep -q "Server is running" "$log" || { echo "1MCP $version did not start; see $log" >&2; exit 1; }
if grep -q "backend.initialization.failed" "$log"; then
  echo "1MCP $version: some servers failed to initialize; see $log" >&2; exit 1
fi
echo "1MCP $version ready ($log)"
cd "$root"
"$root/target/debug/harness" run --config "$config" --output "$out"
