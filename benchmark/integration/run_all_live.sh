#!/usr/bin/env bash
# Run the Early, Middle and Late live stages in sequence, then compute ECS.
#   benchmark/integration/run_all_live.sh <output-root>
set -euo pipefail
out="$1"
root="$(cd "$(dirname "$0")/../.." && pwd)"
for stage in early middle late; do
  if [ -f "$out/$stage/summary.json" ]; then echo "== $stage already done"; continue; fi
  echo "== $stage started $(date -u +%FT%TZ)"
  "$root/benchmark/integration/run_live.sh" "$stage" "$out/$stage" 2>&1 | grep -v "Error calling tool"
  echo "== $stage finished $(date -u +%FT%TZ)"
done
"$root/benchmark/.venv/bin/python" "$root/benchmark/run.py" rust-ecs \
  --early-run "$out/early/events.jsonl" --middle-run "$out/middle/events.jsonl" \
  --late-run "$out/late/events.jsonl" --out "$out/ecs.json"
