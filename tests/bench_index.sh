#!/usr/bin/env bash
# Indexing benchmark. Mines a disposable regtest chain and indexes it with
# several tuning variants, printing one CSV row per run.
#
#   BLOCKS=5000 tests/bench_index.sh
#   VARIANTS="default readers-4 batch-64" tests/bench_index.sh
#
# Never point this at an existing datadir: it creates and deletes its own.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."
BIN="${E_LITE_BIN:-$PWD/target/release/e-lite}"
[ -x "$BIN" ] || { echo "build first: cargo build --release -p e-lite" >&2; exit 1; }
command -v bitcoind >/dev/null || { echo "bitcoind not found" >&2; exit 1; }

BLOCKS="${BLOCKS:-3000}"
OUTPUTS_PER_BLOCK="${OUTPUTS_PER_BLOCK:-0}"
RPC=19443
RUN="$(mktemp -d "${TMPDIR:-/tmp}/e-lite-bench-XXXXXX")"
CORE="$RUN/core"

cleanup() {
  set +e
  [ -f "$CORE/regtest/bitcoind.pid" ] && kill "$(cat "$CORE/regtest/bitcoind.pid")" 2>/dev/null
  pkill -f "bitcoind .*$CORE" 2>/dev/null
  sleep 1
  if [ "${KEEP:-0}" = "1" ]; then
    echo "kept $RUN" >&2
  else
    rm -rf "$RUN"
  fi
}
trap cleanup EXIT INT TERM

mkdir -p "$CORE"
bitcoind -regtest -datadir="$CORE" -daemon -server -listen=0 \
  -rpcport="$RPC" -fallbackfee=0.0002 >/dev/null
CLI="bitcoin-cli -regtest -datadir=$CORE -rpcport=$RPC"
for _ in $(seq 1 100); do $CLI getblockcount >/dev/null 2>&1 && break; sleep 0.2; done
$CLI createwallet bench >/dev/null
ADDR=$($CLI -rpcwallet=bench getnewaddress)

# `OUTPUTS_PER_BLOCK=0` sends a transaction every 8 blocks (block overhead);
# a positive value puts one transaction with that many outputs in every block,
# which exercises scripts/UTXOs without 100k wallet RPC calls.
$CLI -rpcwallet=bench generatetoaddress 110 "$ADDR" >/dev/null
BENCH_MAP=""
if [ "$OUTPUTS_PER_BLOCK" -gt 0 ]; then
  for _t in $(seq 1 "$OUTPUTS_PER_BLOCK"); do
    a=$($CLI -rpcwallet=bench getnewaddress)
    BENCH_MAP="$BENCH_MAP${BENCH_MAP:+,}\"$a\":0.0001"
  done
  BENCH_MAP="{$BENCH_MAP}"
fi
for i in $(seq 1 "$BLOCKS"); do
  if [ "$OUTPUTS_PER_BLOCK" -gt 0 ]; then
    $CLI -rpcwallet=bench sendmany "" "$BENCH_MAP" >/dev/null
  elif [ $((i % 8)) -eq 0 ]; then
    $CLI -rpcwallet=bench sendtoaddress "$ADDR" 0.0001 >/dev/null
  fi
  $CLI -rpcwallet=bench generatetoaddress 1 "$ADDR" >/dev/null
done
TIP=$($CLI getblockcount)
echo "bench chain: $TIP blocks" >&2

# pipe-separated name:readers:batch
VARIANTS="${VARIANTS:-default::|readers-4:4:|readers-2:2:|batch-64::64|batch-1024::1024}"
echo "variant,blocks,txs,seconds,blocks_per_s,max_rss_kib,index_bytes"

IFS='|' read -ra specs <<<"$VARIANTS"
for spec in "${specs[@]}"; do
  IFS=: read -r name readers batch <<<"$spec"
  data="$RUN/index-$name"
  args=(index --network regtest --core-datadir "$CORE" --core-rpc "http://127.0.0.1:$RPC"
        --data-dir "$data" --manage external --min-free-gib 1 --no-tls --no-follow --no-rawtx)
  [ -n "${readers:-}" ] && args+=(--readers "$readers")
  [ -n "${batch:-}" ] && args+=(--batch "$batch")
  timeout "${RUN_TIMEOUT:-900}" python3 "$SCRIPT_DIR/bench_run.py" \
    "$RUN/$name.log" "$RUN/$name.meta" -- "$BIN" "${args[@]}"
  read -r secs rss <"$RUN/$name.meta"
  bytes=$(du -sb "$data" | cut -f1)
  # The index starts at genesis, so it processes TIP + 1 blocks (the 110
  # premine blocks are part of the chain, not excluded).
  blocks=$((TIP + 1))
  txs=$(sed -n 's/.*txnum \([0-9]*\).*/\1/p' "$RUN/$name.log" | tail -1)
  txs=${txs:-0}
  bps=$(python3 -c "print(f'{$blocks/$secs:.1f}')")
  echo "$name,$blocks,$txs,$secs,$bps,$rss,$bytes"
  tail -1 "$RUN/$name.log" >&2
done
