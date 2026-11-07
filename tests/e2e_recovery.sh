#!/usr/bin/env bash
# Crash and reorg recovery on a disposable regtest chain:
#   - repeated SIGKILL while indexing (RocksDB/WAL recovery),
#   - a reorg at the indexed tip (undo records),
#   - a write failure (file-size limit) followed by a clean restart.
set -euo pipefail

cd "$(dirname "$0")/.."
BIN="${E_LITE_BIN:-$PWD/target/debug/e-lite}"
[ -x "$BIN" ] || { echo "build first: cargo build -p e-lite" >&2; exit 1; }
command -v bitcoind >/dev/null || { echo "bitcoind not found" >&2; exit 1; }

MODEL="$(mktemp -d "${TMPDIR:-/tmp}/e-lite-recovery-XXXXXX")"
CORE="$MODEL/core"; DATA="$MODEL/index"; DATA2="$MODEL/index-limit"
CONF="$MODEL/e-lite.toml"
RPC=18488
BLOCKS="${E_LITE_RECOVERY_BLOCKS:-20000}"

cleanup() {
  set +e
  [ -f "$MODEL/index.pid" ] && kill -9 "$(cat "$MODEL/index.pid")" 2>/dev/null
  [ -f "$CORE/regtest/bitcoind.pid" ] && kill "$(cat "$CORE/regtest/bitcoind.pid")" 2>/dev/null
  pkill -f "bitcoind .*$CORE" 2>/dev/null
  sleep 1
  rm -rf "$MODEL"
}
trap cleanup EXIT INT TERM

mkdir -p "$CORE" "$DATA" "$DATA2"
bitcoind -regtest -datadir="$CORE" -daemon -server -listen=0 \
  -rpcport="$RPC" -fallbackfee=0.0002 >/dev/null
CLI="bitcoin-cli -regtest -datadir=$CORE -rpcport=$RPC"
for _ in $(seq 1 100); do $CLI getblockcount >/dev/null 2>&1 && break; sleep 0.2; done
$CLI createwallet rec >/dev/null
ADDR=$($CLI -rpcwallet=rec getnewaddress)
$CLI -rpcwallet=rec generatetoaddress "$BLOCKS" "$ADDR" >/dev/null
TIP=$($CLI getblockcount)
echo "chain: $TIP blocks"

cat >"$CONF" <<EOF
[network]
name = "regtest"
data_dir = "$DATA"

[core]
manage = "external"
core_datadir = "$CORE"
rpc_url = "http://127.0.0.1:$RPC"

[index]
batch_blocks = 32
reorg_window = 32

[pruning]
min_free_gib = 1
poll_seconds = 1

[server]
tls = false

[metrics]
enabled = false

[rawtx]
enabled = false
EOF

height() {
  local out
  for _ in $(seq 1 25); do
    out=$("$BIN" status --config "$CONF" --data-dir "$1" --json 2>"$MODEL/status.err" || true)
    if [ -n "$out" ] && printf '%s' "$out" | python3 -c 'import json,sys;json.load(sys.stdin)' 2>/dev/null; then
      printf '%s' "$out" | python3 -c 'import json,sys;print(json.load(sys.stdin)["indexed_height"])'
      return 0
    fi
    sleep 0.2
  done
  echo "status failed:" >&2
  cat "$MODEL/status.err" >&2
  return 1
}

index_run() {
  "$BIN" index --config "$CONF" --data-dir "$1" --no-follow
}

echo "== SIGKILL while indexing =="
for i in $(seq 1 8); do
  # Run the binary directly: a background bash function would make `$!` the
  # subshell and the kill would miss the indexer.
  "$BIN" index --config "$CONF" --data-dir "$DATA" --no-follow \
    >"$MODEL/$i.log" 2>&1 &
  pid=$!
  echo "$pid" >"$MODEL/index.pid"
  sleep 0.25
  kill -9 "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  rm -f "$MODEL/index.pid"
  h=$(height "$DATA")
  echo "kill $i: committed height $h"
done
# The database must still open after every abrupt stop; finish to the tip.
"$BIN" index --config "$CONF" --data-dir "$DATA" --no-follow >"$MODEL/finish.log" 2>&1
[ "$(height "$DATA")" = "$TIP" ] || {
  echo "did not recover to $TIP" >&2
  tail -20 "$MODEL/finish.log"
  exit 1
}
echo "recovered to $TIP after SIGKILL"

echo "== reorg at the indexed tip =="
FORK=$((TIP - 11))
HASH=$($CLI getblockhash "$FORK")
$CLI invalidateblock "$HASH" >/dev/null
# A new address changes the coinbase; otherwise regtest can reproduce the
# exact hash of the invalidated block and Core rejects it as duplicate-invalid.
BRANCH_ADDR=$($CLI -rpcwallet=rec getnewaddress)
$CLI -rpcwallet=rec generatetoaddress 20 "$BRANCH_ADDR" >/dev/null
NEWTIP=$($CLI getblockcount)
echo "Core moved from $TIP to $NEWTIP (fork $FORK)"
index_run "$DATA" >"$MODEL/reorg.log" 2>&1
[ "$(height "$DATA")" = "$NEWTIP" ] || {
  echo "reorg not recovered: $(height "$DATA") != $NEWTIP" >&2
  tail -30 "$MODEL/reorg.log"
  exit 1
}
echo "reorg recovered to $NEWTIP"

echo "== write failure (file-size limit) then restart =="
set +e
( ulimit -f 8192; index_run "$DATA2" ) >"$MODEL/limit.log" 2>&1
LIMIT_RC=$?
set -e
echo "limited run exit: $LIMIT_RC (expected failure or partial progress)"
index_run "$DATA2" >"$MODEL/limit-restart.log" 2>&1
[ "$(height "$DATA2")" = "$NEWTIP" ] || {
  echo "did not recover after the write failure" >&2
  tail -20 "$MODEL/limit-restart.log"
  exit 1
}
echo "recovered to $NEWTIP after the write failure"

echo "recovery OK"
