#!/usr/bin/env bash
# Core outage resilience + cookie rotation + clean shutdown.
# Self-cleaning. Run with: tests/e2e_resilience.sh
set -euo pipefail

cd "$(dirname "$0")/.."
BIN="${E_LITE_BIN:-$PWD/target/debug/e-lite}"
[ -x "$BIN" ] || { echo "build first: cargo build -p e-lite" >&2; exit 1; }
command -v bitcoind >/dev/null || { echo "bitcoind not found" >&2; exit 1; }

RUN="$(mktemp -d "${TMPDIR:-/tmp}/e-lite-resil-XXXXXX")"
CORE="$RUN/core"; DATA="$RUN/index"
RPC=18463; ELECTRUM=15005; METRICS=19188

cleanup() {
  set +e
  [ -f "$RUN/serve.pid" ] && kill "$(cat "$RUN/serve.pid")" 2>/dev/null
  [ -f "$CORE/regtest/bitcoind.pid" ] && kill "$(cat "$CORE/regtest/bitcoind.pid")" 2>/dev/null
  sleep 1
  rm -rf "$RUN"
}
trap cleanup EXIT INT TERM

start_core() {
  bitcoind -regtest -datadir="$CORE" -daemon -server -listen=0 \
    -rpcport="$RPC" -fallbackfee=0.0002 >/dev/null
  for _ in $(seq 1 100); do
    bitcoin-cli -regtest -datadir="$CORE" -rpcport="$RPC" getblockcount >/dev/null 2>&1 && return 0
    sleep 0.2
  done
  return 1
}

status_state() {
  curl -s "http://127.0.0.1:$METRICS/status" 2>/dev/null \
    | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["state"])
except Exception: print("DOWN")' 2>/dev/null || echo DOWN
}

mkdir -p "$CORE" "$DATA"
start_core
CLI="bitcoin-cli -regtest -datadir=$CORE -rpcport=$RPC"
$CLI createwallet resil >/dev/null
ADDR=$($CLI -rpcwallet=resil getnewaddress)
$CLI -rpcwallet=resil generatetoaddress 101 "$ADDR" >/dev/null
SH=$(python3 - "$ADDR" "$CORE" "$RPC" <<'PY'
import hashlib, json, subprocess, sys
addr, core, rpc = sys.argv[1], sys.argv[2], sys.argv[3]
out = subprocess.check_output(["bitcoin-cli", "-regtest", f"-datadir={core}", f"-rpcport={rpc}", "getaddressinfo", addr])
print(hashlib.sha256(bytes.fromhex(json.loads(out)["scriptPubKey"])).digest()[::-1].hex())
PY
)

"$BIN" init --network regtest --core-datadir "$CORE" --core-rpc "http://127.0.0.1:$RPC" --data-dir "$DATA" >/dev/null
"$BIN" run --network regtest --core-datadir "$CORE" --core-rpc "http://127.0.0.1:$RPC" \
  --data-dir "$DATA" --listen "127.0.0.1:$ELECTRUM" --metrics "127.0.0.1:$METRICS" \
  >"$RUN/serve.log" 2>&1 &
echo $! >"$RUN/serve.pid"
for _ in $(seq 1 60); do [ "$(status_state)" = READY ] && break; sleep 0.5; done
[ "$(status_state)" = READY ] || { cat "$RUN/serve.log"; exit 1; }
echo "initial state: READY"

echo "== stop Core =="
$CLI stop >/dev/null
for _ in $(seq 1 50); do
  bitcoin-cli -regtest -datadir="$CORE" -rpcport="$RPC" getblockcount >/dev/null 2>&1 || break
  sleep 0.2
done
for _ in $(seq 1 30); do [ "$(status_state)" = CORE_UNAVAILABLE ] && break; sleep 0.5; done
echo "state while Core down: $(status_state)"

# Confirmed reads must keep working from the local index.
ENTRIES=$(python3 tests/query_history.py "$ELECTRUM" "$SH")
[ "$ENTRIES" -ge 101 ] || { echo "history missing while Core down: $ENTRIES" >&2; exit 1; }
echo "history while Core down: $ENTRIES entries"

echo "== restart Core (cookie rotates) =="
start_core
for _ in $(seq 1 90); do [ "$(status_state)" = READY ] && break; sleep 1; done
[ "$(status_state)" = READY ] || { echo "did not recover"; tail -20 "$RUN/serve.log"; exit 1; }
echo "state after Core restart: READY"

echo "== SIGTERM =="
kill -TERM "$(cat "$RUN/serve.pid")"
set +e
wait "$(cat "$RUN/serve.pid")"
RC=$?
set -e
echo "shutdown exit: $RC"
[ "$RC" -eq 0 ] || { tail -20 "$RUN/serve.log"; exit 1; }
grep -q "shutdown requested" "$RUN/serve.log"
echo "resilience OK"
