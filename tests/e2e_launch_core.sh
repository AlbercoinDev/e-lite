#!/usr/bin/env bash
# e-lite starts Core from scratch with maximum pruning (prune=550) and indexes
# the chain. Regtest keeps the run small; the same path is used on mainnet.
set -euo pipefail

cd "$(dirname "$0")/.."
BIN="${E_LITE_BIN:-$PWD/target/debug/e-lite}"
[ -x "$BIN" ] || { echo "build first: cargo build -p e-lite" >&2; exit 1; }
command -v bitcoind >/dev/null || { echo "bitcoind not found" >&2; exit 1; }

RUN="$(mktemp -d "${TMPDIR:-/tmp}/e-lite-launch-XXXXXX")"
CORE="$RUN/core"; DATA="$RUN/index"; CONF="$RUN/e-lite.toml"
RPC=18485; ELECTRUM=15008; METRICS=19191

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
manage = "managed"
core_datadir = "$CORE"
rpc_url = "http://127.0.0.1:$RPC"
prune_mib = 550
extra_args = ["-listen=0", "-fallbackfee=0.0002"]

[index]
batch_blocks = 32
reorg_window = 8

[pruning]
min_free_gib = 1
max_core_lag_blocks = 64
poll_seconds = 1

[server]
listen = "127.0.0.1:$ELECTRUM"

[metrics]
listen = "127.0.0.1:$METRICS"

[rawtx]
enabled = false
EOF

echo "== e-lite starts Core from an empty datadir =="
"$BIN" run --config "$CONF" >"$RUN/serve.log" 2>&1 &
echo $! >"$RUN/serve.pid"

CLI="bitcoin-cli -regtest -datadir=$CORE -rpcport=$RPC"
for _ in $(seq 1 200); do $CLI getblockcount >/dev/null 2>&1 && break; sleep 0.2; done
$CLI getblockchaininfo | python3 -c 'import json,sys;d=json.load(sys.stdin);print("core pruned",d["pruned"],"blocks",d["blocks"])'

$CLI createwallet launch >/dev/null
ADDR=$($CLI -rpcwallet=launch getnewaddress)
SH=$(python3 - "$ADDR" "$CORE" "$RPC" <<'PY'
import hashlib, json, subprocess, sys
addr, core, rpc = sys.argv[1], sys.argv[2], sys.argv[3]
out = subprocess.check_output(["bitcoin-cli", "-regtest", f"-datadir={core}", f"-rpcport={rpc}", "getaddressinfo", addr])
print(hashlib.sha256(bytes.fromhex(json.loads(out)["scriptPubKey"])).digest()[::-1].hex())
PY
)
$CLI -rpcwallet=launch generatetoaddress 101 "$ADDR" >/dev/null

state() {
  curl -s "http://127.0.0.1:$METRICS/status" 2>/dev/null \
    | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["state"])
except Exception: print("DOWN")' 2>/dev/null || echo DOWN
}
for _ in $(seq 1 120); do [ "$(state)" = READY ] && break; sleep 0.5; done
echo "state: $(state)"
[ "$(state)" = READY ] || { tail -40 "$RUN/serve.log"; exit 1; }

$CLI -rpcwallet=launch generatetoaddress 50 "$ADDR" >/dev/null
for _ in $(seq 1 60); do
  H=$(curl -s "http://127.0.0.1:$METRICS/status" | python3 -c 'import json,sys;print(json.load(sys.stdin)["indexed_height"])')
  [ "$H" -ge 151 ] && break
  sleep 0.5
done
echo "indexed height: $H"

ENTRIES=$(python3 tests/query_history.py "$ELECTRUM" "$SH")
echo "history: $ENTRIES entries"
[ "$ENTRIES" -ge 151 ] || { echo "history incomplete" >&2; exit 1; }

kill -TERM "$(cat "$RUN/serve.pid")"
set +e
wait "$(cat "$RUN/serve.pid")"
RC=$?
set -e
[ "$RC" -eq 0 ] || { tail -20 "$RUN/serve.log"; exit 1; }

# e-lite owns this Core, so it must have stopped it on shutdown.
sleep 1
if $CLI getblockcount >/dev/null 2>&1; then
  echo "Core still running after e-lite shutdown" >&2
  exit 1
fi
echo "launch-core OK"
