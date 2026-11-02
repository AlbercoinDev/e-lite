#!/usr/bin/env bash
# e-lite starts first with no Core, waits in WAITING_FOR_CORE, then Core appears
# and e-lite auto-initializes and indexes without a restart.
set -euo pipefail

cd "$(dirname "$0")/.."
BIN="${E_LITE_BIN:-$PWD/target/debug/e-lite}"
[ -x "$BIN" ] || { echo "build first: cargo build -p e-lite" >&2; exit 1; }
command -v bitcoind >/dev/null || { echo "bitcoind not found" >&2; exit 1; }

RUN="$(mktemp -d "${TMPDIR:-/tmp}/e-lite-wait-XXXXXX")"
CORE="$RUN/core"; DATA="$RUN/index"; CONF="$RUN/e-lite.toml"
RPC=18484; ELECTRUM=15007; METRICS=19190

cleanup() {
  set +e
  [ -f "$RUN/serve.pid" ] && kill "$(cat "$RUN/serve.pid")" 2>/dev/null
  [ -f "$CORE/regtest/bitcoind.pid" ] && kill "$(cat "$CORE/regtest/bitcoind.pid")" 2>/dev/null
  pkill -f "bitcoind .*$CORE" 2>/dev/null
  sleep 1
  rm -rf "$RUN"
}
trap cleanup EXIT INT TERM

mkdir -p "$CORE" "$DATA"
cat >"$CONF" <<EOF
[network]
name = "regtest"
data_dir = "$DATA"

[core]
manage = "external"
core_datadir = "$CORE"
rpc_url = "http://127.0.0.1:$RPC"

[index]
batch_blocks = 16
reorg_window = 8

[pruning]
min_free_gib = 1
poll_seconds = 1

[server]
listen = "127.0.0.1:$ELECTRUM"

[metrics]
listen = "127.0.0.1:$METRICS"

[rawtx]
enabled = false
EOF

"$BIN" run --config "$CONF" >"$RUN/serve.log" 2>&1 &
echo $! >"$RUN/serve.pid"

state() {
  curl -s "http://127.0.0.1:$METRICS/status" 2>/dev/null \
    | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["state"])
except Exception: print("DOWN")' 2>/dev/null || echo DOWN
}

echo "== start e-lite with no Core =="
for _ in $(seq 1 60); do [ "$(state)" = WAITING_FOR_CORE ] && break; sleep 0.5; done
echo "state before Core: $(state)"
[ "$(state)" = WAITING_FOR_CORE ] || { tail -30 "$RUN/serve.log"; exit 1; }

echo "== start Core =="
bitcoind -regtest -datadir="$CORE" -daemon -server -listen=0 \
  -rpcport="$RPC" -fallbackfee=0.0002 >/dev/null
CLI="bitcoin-cli -regtest -datadir=$CORE -rpcport=$RPC"
for _ in $(seq 1 100); do $CLI getblockcount >/dev/null 2>&1 && break; sleep 0.2; done
$CLI createwallet wait >/dev/null
ADDR=$($CLI -rpcwallet=wait getnewaddress)
SH=$(python3 - "$ADDR" "$CORE" "$RPC" <<'PY'
import hashlib, json, subprocess, sys
addr, core, rpc = sys.argv[1], sys.argv[2], sys.argv[3]
out = subprocess.check_output(["bitcoin-cli", "-regtest", f"-datadir={core}", f"-rpcport={rpc}", "getaddressinfo", addr])
print(hashlib.sha256(bytes.fromhex(json.loads(out)["scriptPubKey"])).digest()[::-1].hex())
PY
)
$CLI -rpcwallet=wait generatetoaddress 101 "$ADDR" >/dev/null

height() {
  curl -s "http://127.0.0.1:$METRICS/status" 2>/dev/null \
    | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["indexed_height"])
except Exception: print(0)' 2>/dev/null || echo 0
}
for _ in $(seq 1 120); do
  [ "$(state)" = READY ] && [ "$(height)" -ge 101 ] && break
  sleep 0.5
done
echo "state after Core: $(state) height $(height)"
[ "$(state)" = READY ] || { tail -40 "$RUN/serve.log"; exit 1; }
[ "$(height)" -ge 101 ] || { tail -40 "$RUN/serve.log"; exit 1; }

ENTRIES=$(python3 tests/query_history.py "$ELECTRUM" "$SH")
echo "history: $ENTRIES entries"
[ "$ENTRIES" -ge 101 ] || { echo "history incomplete" >&2; exit 1; }

kill -TERM "$(cat "$RUN/serve.pid")"
set +e
wait "$(cat "$RUN/serve.pid")"
RC=$?
set -e
[ "$RC" -eq 0 ] || { tail -20 "$RUN/serve.log"; exit 1; }
echo "wait-for-core OK"
