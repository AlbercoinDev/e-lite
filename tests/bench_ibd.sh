#!/usr/bin/env bash
# Joint Core IBD + e-lite indexing benchmark (Phase A).
#
# A disposable regtest feeder node provides the chain. e-lite starts a fresh
# managed Core (`prune_mib = 1`) that downloads from the feeder while e-lite
# indexes. The sampler records both processes, stage timings, commits, RocksDB
# compaction and disk.
#
#   BLOCKS=20000 tests/bench_ibd.sh
#   BLOCKS=100000 SEND_EVERY=16 tests/bench_ibd.sh
#
# Never point this at an existing datadir: it creates and deletes its own.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."
BIN="${E_LITE_BIN:-$PWD/target/release/e-lite}"
[ -x "$BIN" ] || { echo "build first: cargo build --release -p e-lite" >&2; exit 1; }
command -v bitcoind >/dev/null || { echo "bitcoind not found" >&2; exit 1; }

BLOCKS="${BLOCKS:-20000}"
SEND_EVERY="${SEND_EVERY:-0}"      # 0 = coinbase-only blocks
DBCACHE="${DBCACHE:-450}"          # Core UTXO cache for the syncer
KEEP="${KEEP:-0}"
TIMEOUT="${TIMEOUT:-3600}"

FEED_P2P=19555
FEED_RPC=19560
SYNC_RPC=19561
ELECTRUM=19562
METRICS=19563

RUN="$(mktemp -d "${TMPDIR:-/tmp}/e-lite-ibd-XXXXXX")"
FEEDER="$RUN/feeder"; SYNC="$RUN/sync-core"; DATA="$RUN/index"
CONF="$RUN/e-lite.toml"

cleanup() {
  set +e
  [ -f "$RUN/elite.pid" ] && kill -TERM "$(cat "$RUN/elite.pid")" 2>/dev/null
  [ -f "$SYNC/regtest/bitcoind.pid" ] && kill "$(cat "$SYNC/regtest/bitcoind.pid")" 2>/dev/null
  [ -f "$FEEDER/regtest/bitcoind.pid" ] && kill "$(cat "$FEEDER/regtest/bitcoind.pid")" 2>/dev/null
  pkill -f "bitcoind .*$SYNC" 2>/dev/null
  pkill -f "bitcoind .*$FEEDER" 2>/dev/null
  sleep 2
  if [ "$KEEP" = "1" ]; then
    echo "kept $RUN" >&2
  else
    rm -rf "$RUN"
  fi
}
trap cleanup EXIT INT TERM

for port in "$FEED_P2P" "$FEED_RPC" "$SYNC_RPC" "$ELECTRUM" "$METRICS"; do
  if ss -ltn 2>/dev/null | awk '{print $4}' | grep -qE "[:.]$port$"; then
    echo "port $port is already in use" >&2
    exit 1
  fi
done

echo "== feeder chain ($BLOCKS blocks) =="
mkdir -p "$FEEDER" "$SYNC" "$DATA"
bitcoind -regtest -datadir="$FEEDER" -daemon -server -listen=1 -port="$FEED_P2P" \
  -rpcport="$FEED_RPC" -fallbackfee=0.0002 -dbcache=450 >/dev/null
FCLI="bitcoin-cli -regtest -datadir=$FEEDER -rpcport=$FEED_RPC"
for _ in $(seq 1 100); do $FCLI getblockcount >/dev/null 2>&1 && break; sleep 0.2; done
$FCLI createwallet bench >/dev/null
ADDR=$($FCLI -rpcwallet=bench getnewaddress)
$FCLI -rpcwallet=bench generatetoaddress 110 "$ADDR" >/dev/null
LEFT=$BLOCKS
i=0
while [ "$LEFT" -gt 0 ]; do
  N=$((LEFT > 1000 ? 1000 : LEFT))
  if [ "$SEND_EVERY" -gt 0 ]; then
    for _ in $(seq 1 "$N"); do
      i=$((i + 1))
      if [ $((i % SEND_EVERY)) -eq 0 ]; then
        $FCLI -rpcwallet=bench sendtoaddress "$ADDR" 0.0001 >/dev/null
      fi
    done
    $FCLI -rpcwallet=bench generatetoaddress "$N" "$ADDR" >/dev/null
  else
    $FCLI -rpcwallet=bench generatetoaddress "$N" "$ADDR" >/dev/null
  fi
  LEFT=$((LEFT - N))
done
TIP=$($FCLI getblockcount)
echo "feeder tip: $TIP"

cat >"$CONF" <<EOF
[network]
name = "regtest"
data_dir = "$DATA"

[core]
manage = "managed"
core_datadir = "$SYNC"
rpc_url = "http://127.0.0.1:$SYNC_RPC"
prune_mib = 1
extra_args = ["-connect=127.0.0.1:$FEED_P2P", "-listen=0", "-dnsseed=0", "-maxconnections=8", "-dbcache=$DBCACHE"]

[index]
batch_blocks = 256
profile = "auto"

[pruning]
min_free_gib = 1
poll_seconds = 1

[server]
listen = "127.0.0.1:$ELECTRUM"
tls = false

[metrics]
listen = "127.0.0.1:$METRICS"

[tor]
mode = "off"

[rawtx]
enabled = false
EOF

echo "== e-lite run =="
"$BIN" run --config "$CONF" >"$RUN/run.log" 2>&1 &
echo $! >"$RUN/elite.pid"

set +e
python3 "$SCRIPT_DIR/bench_sample.py" \
  --status-url "http://127.0.0.1:$METRICS/status" \
  --core-cli "bitcoin-cli -regtest -datadir=$SYNC -rpcport=$SYNC_RPC getblockchaininfo" \
  --core-datadir "$SYNC" \
  --data-dir "$DATA" \
  --core-pid-file "$SYNC/regtest/bitcoind.pid" \
  --elite-pid-file "$RUN/elite.pid" \
  --tip "$TIP" \
  --csv "$RUN/samples.csv" \
  --summary "$RUN/summary.json" \
  --timeout "$TIMEOUT"
SAMPLER_RC=$?
set -e

echo "== summary =="
cat "$RUN/summary.json" 2>/dev/null || echo "(no summary)"
echo
echo "log: $RUN/run.log"
if [ "$KEEP" = "1" ]; then
  echo "artifacts kept in $RUN"
else
  # Keep the summary in the temp dir only when requested; print the key paths.
  cp "$RUN/summary.json" "${TMPDIR:-/tmp}/e-lite-ibd-summary.json" 2>/dev/null || true
  cp "$RUN/samples.csv" "${TMPDIR:-/tmp}/e-lite-ibd-samples.csv" 2>/dev/null || true
fi
if [ "$SAMPLER_RC" -ne 0 ]; then
  echo "benchmark did not reach the tip (exit $SAMPLER_RC)" >&2
  exit "$SAMPLER_RC"
fi
