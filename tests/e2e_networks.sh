#!/usr/bin/env bash
# Signet / testnet4 smoke test. This performs a real network sync, so it is
# intentionally opt-in:
#
#   E_LITE_NETWORK=signet tests/e2e_networks.sh
#   E_LITE_NETWORK=testnet4 E_LITE_CORE_DATADIR=/srv/core-t4 tests/e2e_networks.sh
#
# If E_LITE_CORE_DATADIR is not set, a temporary datadir is used and e-lite
# launches Core with pruned IBD (prune=550). The sync can take a long time.
# If you provide a datadir, stop any Core using it first: the script runs with
# manage = "managed", so e-lite must own the process.
#
# Optional:
#   E_LITE_E2E_DATA_DIR=/path/to/index   keep the e-lite index on a larger disk
#   E_LITE_E2E_MIN_FREE_GIB=2            disk guard pause threshold
#   E_LITE_E2E_TIMEOUT=86400             overall sync timeout in seconds
#   E_LITE_E2E_REQUIRED_FREE_GIB=15/20   refuse to start below this free space
#                                        (default 15 signet, 20 testnet4)
set -euo pipefail

cd "$(dirname "$0")/.."
BIN="${E_LITE_BIN:-$PWD/target/debug/e-lite}"
[ -x "$BIN" ] || { echo "build first: cargo build -p e-lite" >&2; exit 1; }
command -v bitcoind >/dev/null || { echo "bitcoind not found" >&2; exit 1; }

NETWORK="${E_LITE_NETWORK:-signet}"
case "$NETWORK" in
  signet) DEFAULT_RPC=38332 ;;
  testnet4) DEFAULT_RPC=48332 ;;
  *) echo "E_LITE_NETWORK must be signet or testnet4" >&2; exit 1 ;;
esac
RPC="${E_LITE_E2E_RPC_PORT:-$DEFAULT_RPC}"
ELECTRUM="${E_LITE_E2E_ELECTRUM_PORT:-15011}"
METRICS="${E_LITE_E2E_METRICS_PORT:-19194}"
TIMEOUT="${E_LITE_E2E_TIMEOUT:-86400}"
MIN_FREE="${E_LITE_E2E_MIN_FREE_GIB:-2}"
if [ "$NETWORK" = "testnet4" ]; then
  REQUIRED_FREE="${E_LITE_E2E_REQUIRED_FREE_GIB:-20}"
else
  REQUIRED_FREE="${E_LITE_E2E_REQUIRED_FREE_GIB:-15}"
fi

RUN="$(mktemp -d "${TMPDIR:-/tmp}/e-lite-net-XXXXXX")"
CORE="${E_LITE_CORE_DATADIR:-$RUN/core}"
DATA="${E_LITE_E2E_DATA_DIR:-$RUN/index}"
CONF="$RUN/e-lite.toml"

cleanup() {
  set +e
  [ -f "$RUN/serve.pid" ] && kill "$(cat "$RUN/serve.pid")" 2>/dev/null
  [ -f "$CORE/$NETWORK/bitcoind.pid" ] && kill "$(cat "$CORE/$NETWORK/bitcoind.pid")" 2>/dev/null
  sleep 1
  # CORE and DATA may point outside RUN (large disks); only remove RUN itself.
  rm -rf "$RUN"
}
trap cleanup EXIT INT TERM

mkdir -p "$CORE" "$DATA"
cat >"$CONF" <<EOF
[network]
name = "$NETWORK"
data_dir = "$DATA"

[core]
manage = "managed"
core_datadir = "$CORE"
rpc_url = "http://127.0.0.1:$RPC"
prune_mib = 1
extra_args = ["-listen=0"]

[index]
block_source = "auto"
batch_blocks = 256
reorg_window = 288

[pruning]
min_free_gib = $MIN_FREE
max_core_lag_blocks = 192
poll_seconds = 10

[server]
listen = "127.0.0.1:$ELECTRUM"

[metrics]
listen = "127.0.0.1:$METRICS"

[rawtx]
enabled = true
EOF

echo "== disk before $NETWORK =="
df -h "$DATA" "$CORE" | tail -n +2
FREE_GIB=$(( $(df -Pk "$DATA" | awk 'NR==2{print $4}') / 1024 / 1024 ))
if [ "$FREE_GIB" -lt "$REQUIRED_FREE" ]; then
  echo "not enough free space in $DATA: ${FREE_GIB} GiB < ${REQUIRED_FREE} GiB" >&2
  echo "free space, use another disk, or override E_LITE_E2E_REQUIRED_FREE_GIB" >&2
  exit 1
fi

echo "== $NETWORK: e-lite launches Core with prune=550 =="
"$BIN" run --config "$CONF" >"$RUN/serve.log" 2>&1 &
echo $! >"$RUN/serve.pid"

START=$(date +%s)
while :; do
  STATUS=$(curl -s "http://127.0.0.1:$METRICS/status" 2>/dev/null || true)
  read -r S H CH IBD <<<"$(printf '%s' "$STATUS" | python3 -c 'import json,sys
try:
 d=json.load(sys.stdin); print(d.get("state","DOWN"), d.get("indexed_height",0), d.get("core_height",0), d.get("core",{}).get("initial_block_download", False))
except Exception: print("DOWN 0 0 False")' 2>/dev/null)"
  echo "[$(( $(date +%s) - START ))s] state=$S height=$H core=$CH ibd=$IBD"
  if [ "$S" = READY ] && [ "$CH" -gt 0 ] && [ "$H" -ge $((CH - 1)) ] && [ "$IBD" = "False" ]; then
    break
  fi
  [ "$S" = FATAL ] && { tail -60 "$RUN/serve.log"; exit 1; }
  [ $(( $(date +%s) - START )) -gt "$TIMEOUT" ] && { echo "timeout"; tail -60 "$RUN/serve.log"; exit 1; }
  sleep 30
done

echo "== $NETWORK is READY at height $H (core $CH) =="
STATUS=$(curl -s "http://127.0.0.1:$METRICS/status" 2>/dev/null || true)
PRUNE=$(printf '%s' "$STATUS" | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("core",{}).get("pruneheight",0))
except Exception: print(0)' 2>/dev/null)
if [ "$H" -lt "$PRUNE" ]; then
  echo "prune gap: indexed=$H pruneheight=$PRUNE" >&2
  exit 1
fi
echo "pruneheight=$PRUNE indexed=$H (no gap)"
python3 - "$ELECTRUM" <<'PY'
import json, socket, sys
port = int(sys.argv[1])
s = socket.create_connection(("127.0.0.1", port), timeout=10)
f = s.makefile("rwb")
def call(i, method, params):
    f.write((json.dumps({"jsonrpc":"2.0","id":i,"method":method,"params":params})+"\n").encode())
    f.flush()
    r = json.loads(f.readline())
    assert "error" not in r, r
    return r["result"]
version = call(1, "server.version", ["e2e", "1.4"])
assert version[1] == "1.4", version
header = call(2, "blockchain.headers.subscribe", [])
assert header["height"] > 0, header
history = call(3, "blockchain.scripthash.get_history", ["00" * 32])
assert history == [], history
print("electrum protocol OK", version[0], "header", header["height"])
PY

kill -TERM "$(cat "$RUN/serve.pid")"
set +e
wait "$(cat "$RUN/serve.pid")"
RC=$?
set -e
[ "$RC" -eq 0 ] || { tail -30 "$RUN/serve.log"; exit 1; }
echo "$NETWORK OK"
