#!/usr/bin/env bash
# Run e-lite for a fixed window against a real Core/index and print the
# indexing metrics it produced. A wrapper, not a synthetic benchmark (that is
# bench_index.sh). The log goes to target/bench-logs/ (BENCH_LOG_DIR), never
# into the data directory.
#
# Usage:
#   tests/bench/bench_node.sh <network> <data-dir> <core-datadir> [seconds] [-- extra e-lite flags]
#
# Example:
#   tests/bench/bench_node.sh mainnet "$DATA" "$CORE" 600 -- \
#     --core-rpc http://127.0.0.1:8332 --manage managed --prune-mib 1 \
#     --allow-mainnet --set index.profile=ssd --set index.adaptive=false
set -euo pipefail

if [ "$#" -lt 3 ]; then
  sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
fi

NETWORK="$1"; DATA="$2"; CORE="$3"; shift 3
SECONDS_TO_RUN="${1:-600}"
if [ "$#" -gt 0 ]; then shift; fi
EXTRA=()
if [ "${1:-}" = "--" ]; then shift; fi
EXTRA=("$@")

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="${E_LITE_BIN:-$ROOT/target/release/e-lite}"
[ -x "$BIN" ] || BIN="$ROOT/target/debug/e-lite"
[ -x "$BIN" ] || { echo "e-lite binary not found (set E_LITE_BIN)" >&2; exit 1; }

LOG_DIR="${BENCH_LOG_DIR:-$ROOT/target/bench-logs}"
mkdir -p "$LOG_DIR"
LOG="$LOG_DIR/bench_node-$NETWORK-$(date +%Y%m%d-%H%M%S).log"
echo "== e-lite benchmark =="
echo "network: $NETWORK  data: $DATA  core: $CORE  seconds: $SECONDS_TO_RUN"
echo "log: $LOG"
"$BIN" run --network "$NETWORK" --data-dir "$DATA" --core-datadir "$CORE" \
  --set pruning.min_free_gib=1 "${EXTRA[@]}" >"$LOG" 2>&1 &
PID=$!
sleep "$SECONDS_TO_RUN"
kill -TERM "$PID" 2>/dev/null || true
wait "$PID" 2>/dev/null || true

echo
echo "== last progress lines =="
grep -E "height .*blocks/s" "$LOG" | tail -n 5 || true
echo
echo "== milestones =="
grep "milestone" "$LOG" | tail -n 10 || true
echo
echo "== tuning =="
grep -E "tuning profile|adaptive tuning|READY|FULLY SYNCED" "$LOG" | tail -n 20 || true
echo
echo "Full log: $LOG"
