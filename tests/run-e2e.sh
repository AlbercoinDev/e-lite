#!/usr/bin/env bash
# Run the regtest end-to-end suites in one go.
#
#   tests/run-e2e.sh                     every suite (not e2e_networks)
#   tests/run-e2e.sh regtest snapshot    only these (with or without e2e_)
#   tests/run-e2e.sh --release           build and test the release binary
#   tests/run-e2e.sh --list              print the suite names
#
# Builds the binary once (unless E_LITE_BIN is set), runs the suites one
# after another, keeps every log under target/e2e-logs/<time>/ and fails a
# suite that leaves a process behind. E2E_TIMEOUT (seconds, default 1200)
# bounds each suite. e2e_networks syncs a real network: run it by hand.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
PROFILE=debug
SUITES=()
all_suites() {
  local f name
  for f in tests/e2e/e2e_*.sh; do
    name=$(basename "$f" .sh)
    [ "$name" = e2e_networks ] || echo "$name"
  done
}
for arg in "$@"; do
  case "$arg" in
    --release) PROFILE=release ;;
    --list) all_suites | sed 's/^e2e_//'; exit 0 ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    *) SUITES+=("e2e_${arg#e2e_}") ;;
  esac
done
[ ${#SUITES[@]} -gt 0 ] || mapfile -t SUITES < <(all_suites)
for s in "${SUITES[@]}"; do
  [ -x "tests/e2e/$s.sh" ] || { echo "unknown suite: ${s#e2e_} (see --list)" >&2; exit 2; }
done

if [ -z "${E_LITE_BIN:-}" ]; then
  if [ "$PROFILE" = release ]; then cargo build --release -p e-lite; else cargo build -p e-lite; fi
  export E_LITE_BIN="$ROOT/target/$PROFILE/e-lite"
fi
command -v bitcoind >/dev/null || { echo "bitcoind not found in PATH" >&2; exit 1; }

LOGS="${E2E_LOG_DIR:-$ROOT/target/e2e-logs/$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$LOGS"
# Every suite works under $TMPDIR/e-lite-<name>-XXXXXX: a process whose
# command line still names such a directory was left behind.
LEFT_PATTERN="${TMPDIR:-/tmp}/e-lite-[a-z_]+-[A-Za-z0-9]{6}"

FAILED=()
printf '%-16s %-8s %6s\n' suite result time
for s in "${SUITES[@]}"; do
  name=${s#e2e_}
  start=$(date +%s)
  set +e
  timeout "${E2E_TIMEOUT:-1200}" "tests/e2e/$s.sh" >"$LOGS/$name.log" 2>&1
  rc=$?
  set -e
  result=ok
  [ "$rc" -eq 0 ] || result="FAIL:$rc"
  sleep 1
  if pgrep -f "$LEFT_PATTERN" >/dev/null; then
    { echo "--- processes left behind:"; pgrep -af "$LEFT_PATTERN"; } >>"$LOGS/$name.log"
    pkill -KILL -f "$LEFT_PATTERN" 2>/dev/null || true
    [ "$result" = ok ] && result=LEFTOVER
  fi
  printf '%-16s %-8s %5ss\n' "$name" "$result" "$(($(date +%s) - start))"
  [ "$result" = ok ] || FAILED+=("$name")
done

echo "logs: $LOGS"
if [ ${#FAILED[@]} -gt 0 ]; then
  for name in "${FAILED[@]}"; do echo "--- $name (last lines)"; tail -15 "$LOGS/$name.log"; done
  echo "failed: ${FAILED[*]}" >&2
  exit 1
fi
echo "all ${#SUITES[@]} suites passed"
