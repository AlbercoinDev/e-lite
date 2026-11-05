#!/usr/bin/env bash
# Run e-lite for a fixed window and print the indexing metrics it produced.
# The real workload is whatever Core/index you point it at; this is a wrapper,
# not a synthetic benchmark.
#
# Usage:
#   scripts/benchmark-index.sh <network> <data-dir> <core-datadir> [seconds] [-- extra e-lite flags]
#
# Example:
#   scripts/benchmark-index.sh mainnet "$DATA" "$CORE" 600 -- \
#     --core-rpc http://127.0.0.1:8332 --manage managed --prune-mib 1 \
#     --allow-mainnet --profile ssd --no-adaptive
set -euo pipefail

if [ "$#" -lt 3 ]; then
  sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
fi

NETWORK="$1"; DATA="$2"; CORE="$3"; shift 3
SECONDS_TO_RUN="${1:-600}"
if [ "$#" -gt 0 ]; then shift; fi
EXTRA=()
if [ "${1:-}" = "--" ]; then shift; fi
EXTRA=("$@")

BIN="${E_LITE_BIN:-$(dirname "$0")/../target/release/e-lite}"
[ -x "$BIN" ] || BIN="$(dirname "$0")/../target/debug/e-lite"
[ -x "$BIN" ] || { echo "e-lite binary not found (set E_LITE_BIN)" >&2; exit 1; }

LOG="$DATA/benchmark-$(date +%Y%m%d-%H%M%S).log"
echo "== e-lite benchmark =="
echo "network: $NETWORK  data: $DATA  core: $CORE  seconds: $SECONDS_TO_RUN"
echo "log: $LOG"
"$BIN" run --network "$NETWORK" --data-dir "$DATA" --core-datadir "$CORE" \
  --min-free-gib 1 "${EXTRA[@]}" >"$LOG" 2>&1 &
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
