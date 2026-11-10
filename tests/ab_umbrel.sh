#!/usr/bin/env bash
# Sample a running e-lite /status for an A/B run. It does not change config or
# restart anything: set the variant in config.toml, restart e-lite, then run
# this script for 30-60 minutes per label.
#
# Usage:
#   tests/ab_umbrel.sh baseline 1800
#   tests/ab_umbrel.sh readers6 1800
#   tests/ab_umbrel.sh cache6g 1800
#   tests/ab_umbrel.sh bulk20g 1800
#
# Environment:
#   E_LITE_STATUS_URL  default http://127.0.0.1:55084/status (mainnet metrics)
#   E_LITE_AB_OUT      default /tmp/e-lite-ab-<label>.csv
#   E_LITE_AB_INTERVAL default 15 seconds
set -euo pipefail

LABEL="${1:-baseline}"
DURATION="${2:-1800}"
URL="${E_LITE_STATUS_URL:-http://127.0.0.1:55084/status}"
OUT="${E_LITE_AB_OUT:-/tmp/e-lite-ab-${LABEL}.csv}"
INTERVAL="${E_LITE_AB_INTERVAL:-15}"

command -v curl >/dev/null || { echo "curl not found" >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 not found" >&2; exit 1; }

echo "label,ts,indexed_height,core_height,lag,phase,phase_blocks,phase_ms,phase_rate,backlog_bytes,avg_block_bytes,cache_mib,memtables_mib,readers,prefetch_p95_ms,wait_p95_ms,commit_group,commit_reason,state" >"$OUT"
echo "sampling $URL -> $OUT for ${DURATION}s every ${INTERVAL}s (label $LABEL)"

end=$((SECONDS + DURATION))
samples=0
while [ "$SECONDS" -lt "$end" ]; do
  if curl -sf --max-time 5 "$URL" | python3 -c '
import json, sys, time
label = sys.argv[1]
try:
    d = json.load(sys.stdin)
except Exception:
    raise SystemExit(1)
s = d.get("scheduler", {})
t = d.get("tuning", {})
m = d.get("memory", {})
pb = s.get("phase_blocks", 0) or 0
pm = s.get("phase_elapsed_ms", 0) or 0
rate = (pb * 1000.0 / pm) if pm else 0.0
print(",".join(str(x) for x in [
    label, int(time.time()), d.get("indexed_height", 0), d.get("core_height", 0),
    d.get("lag", 0), s.get("phase", ""), pb, pm, f"{rate:.3f}",
    s.get("backlog_bytes", 0), s.get("avg_block_bytes", 0),
    t.get("cache_mib", 0), m.get("memtables_mib", 0), t.get("readers", 0),
    t.get("prefetch_p95_ms", 0), t.get("wait_p95_ms", 0),
    t.get("commit_last_blocks", 0), t.get("commit_last_reason", 0),
    d.get("state", ""),
]))
' "$LABEL" >>"$OUT"; then
    samples=$((samples + 1))
  else
    printf '%s,%s,,,,,,,,,,,,,,,,,,DOWN\n' "$LABEL" "$(date +%s)" >>"$OUT"
  fi
  sleep "$INTERVAL"
done

python3 - "$OUT" <<'PY'
import csv, sys
from collections import defaultdict

path = sys.argv[1]
rows = list(csv.DictReader(open(path)))
by_phase = defaultdict(list)
for r in rows:
    if r["phase"] and r["phase_rate"]:
        try:
            by_phase[r["phase"]].append(float(r["phase_rate"]))
        except ValueError:
            pass
print(f"\n{len(rows)} samples in {path}")
for phase, vals in sorted(by_phase.items()):
    vals.sort()
    p50 = vals[len(vals) // 2]
    print(f"  {phase:9s}: n={len(vals):4d} mean={sum(vals)/len(vals):.2f} b/s p50={p50:.2f} b/s")
print("\nUse the phase rates, not the cumulative `blocks/s` log line.")
print("Keep a variant only if its active-phase rate improves without OOM or a")
print("worse drain rate; otherwise revert.")
PY
