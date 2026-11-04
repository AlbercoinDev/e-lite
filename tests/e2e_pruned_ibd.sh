#!/usr/bin/env bash
# Mode B: e-lite starts Core with manual pruning (`-prune=1 -fastprune`),
# indexes from genesis while both run, then the supervisor prunes safely and
# the server keeps answering from its local index.
set -euo pipefail

cd "$(dirname "$0")/.."
BIN="${E_LITE_BIN:-$PWD/target/debug/e-lite}"
[ -x "$BIN" ] || { echo "build first: cargo build -p e-lite" >&2; exit 1; }
command -v bitcoind >/dev/null || { echo "bitcoind not found" >&2; exit 1; }

RUN="$(mktemp -d "${TMPDIR:-/tmp}/e-lite-pruned-XXXXXX")"
CORE="$RUN/core"; DATA="$RUN/index"; CONF="$RUN/e-lite.toml"
RPC=18483; ELECTRUM=15006; METRICS=19189

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
# Seed the datadir (wallet + 101 blocks), then let e-lite own and restart Core.
bitcoind -regtest -datadir="$CORE" -daemon -server -listen=0 -prune=1 -fastprune \
  -rpcport="$RPC" -fallbackfee=0.0002 >/dev/null
CLI="bitcoin-cli -regtest -datadir=$CORE -rpcport=$RPC"
for _ in $(seq 1 100); do $CLI getblockcount >/dev/null 2>&1 && break; sleep 0.2; done
$CLI createwallet pruned >/dev/null
ADDR=$($CLI -rpcwallet=pruned getnewaddress)
SH=$(python3 - "$ADDR" "$CORE" "$RPC" <<'PY'
import hashlib, json, subprocess, sys
addr, core, rpc = sys.argv[1], sys.argv[2], sys.argv[3]
out = subprocess.check_output(["bitcoin-cli", "-regtest", f"-datadir={core}", f"-rpcport={rpc}", "getaddressinfo", addr])
print(hashlib.sha256(bytes.fromhex(json.loads(out)["scriptPubKey"])).digest()[::-1].hex())
PY
)
$CLI -rpcwallet=pruned generatetoaddress 101 "$ADDR" >/dev/null
$CLI stop >/dev/null
for _ in $(seq 1 100); do $CLI getblockcount >/dev/null 2>&1 || break; sleep 0.2; done

cat >"$CONF" <<EOF
[network]
name = "regtest"
data_dir = "$DATA"

[core]
manage = "managed"
core_datadir = "$CORE"
rpc_url = "http://127.0.0.1:$RPC"
prune_mib = 1
extra_args = ["-fastprune", "-listen=0"]
timeout_seconds = 30

[index]
block_source = "auto"
batch_blocks = 16
reorg_window = 8

[pruning]
min_free_gib = 1
max_core_lag_blocks = 64
min_gain_blocks = 288
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
height() {
  curl -s "http://127.0.0.1:$METRICS/status" 2>/dev/null \
    | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["indexed_height"])
except Exception: print(0)' 2>/dev/null || echo 0
}

# e-lite starts Core; wait for RPC and the wallet.
for _ in $(seq 1 150); do $CLI getblockcount >/dev/null 2>&1 && break; sleep 0.2; done
$CLI loadwallet pruned >/dev/null 2>&1 || true
for _ in $(seq 1 120); do [ "$(state)" = READY ] && break; sleep 0.5; done
echo "initial state: $(state) height $(height)"
[ "$(state)" = READY ] || { tail -40 "$RUN/serve.log"; exit 1; }

echo "== mine 2000 blocks (Core is managed with manual prune) =="
$CLI -rpcwallet=pruned generatetoaddress 2000 "$ADDR" >/dev/null
for _ in $(seq 1 180); do
  [ "$(height)" -ge 2101 ] && break
  sleep 1
done
echo "indexed height: $(height) state $(state)"
[ "$(height)" -ge 2101 ] || { tail -40 "$RUN/serve.log"; exit 1; }

echo "== wait for the prune gate =="
PRUNED=0
for _ in $(seq 1 120); do
  PRUNED=$($CLI getblockchaininfo | python3 -c 'import json,sys;print(json.load(sys.stdin)["pruneheight"])')
  [ "$PRUNED" -gt 0 ] && break
  sleep 1
done
echo "core pruneheight: $PRUNED"
[ "$PRUNED" -gt 0 ] || { echo "prune gate did not prune"; tail -40 "$RUN/serve.log"; exit 1; }

OLD_HASH=$($CLI getblockhash 10)
if $CLI getblock "$OLD_HASH" >/dev/null 2>&1; then
  echo "expected old block to be pruned" >&2
  exit 1
fi
echo "old block is pruned from Core (expected)"

ENTRIES=$(python3 tests/query_history.py "$ELECTRUM" "$SH")
echo "history from e-lite index: $ENTRIES entries"
[ "$ENTRIES" -ge 2000 ] || { echo "history incomplete" >&2; exit 1; }
[ "$(state)" = READY ] || { echo "server not READY: $(state)"; exit 1; }

echo "== SIGTERM =="
kill -TERM "$(cat "$RUN/serve.pid")"
set +e
wait "$(cat "$RUN/serve.pid")"
RC=$?
set -e
echo "shutdown exit: $RC"
[ "$RC" -eq 0 ] || { tail -20 "$RUN/serve.log"; exit 1; }
echo "pruned-ibd OK"
