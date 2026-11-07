#!/usr/bin/env bash
# A pruned Core that e-lite did not start must not be used for a from-scratch
# IBD. e-lite must fail with instructions and must never pause the node.
set -euo pipefail

cd "$(dirname "$0")/.."
BIN="${E_LITE_BIN:-$PWD/target/debug/e-lite}"
[ -x "$BIN" ] || { echo "build first: cargo build -p e-lite" >&2; exit 1; }
command -v bitcoind >/dev/null || { echo "bitcoind not found" >&2; exit 1; }

RUN="$(mktemp -d "${TMPDIR:-/tmp}/e-lite-external-XXXXXX")"
CORE="$RUN/core"; DATA="$RUN/index"; CONF="$RUN/e-lite.toml"
RPC=18486; ELECTRUM=15009; METRICS=19192

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
bitcoind -regtest -datadir="$CORE" -daemon -server -listen=0 -prune=1 -fastprune \
  -rpcport="$RPC" -fallbackfee=0.0002 >/dev/null
CLI="bitcoin-cli -regtest -datadir=$CORE -rpcport=$RPC"
for _ in $(seq 1 100); do $CLI getblockcount >/dev/null 2>&1 && break; sleep 0.2; done
$CLI createwallet ext >/dev/null
ADDR=$($CLI -rpcwallet=ext getnewaddress)
$CLI -rpcwallet=ext generatetoaddress 101 "$ADDR" >/dev/null

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
tls = false

[metrics]
listen = "127.0.0.1:$METRICS"

[rawtx]
enabled = false
EOF

set +e
"$BIN" run --config "$CONF" >"$RUN/serve.log" 2>&1
RC=$?
set -e
echo "e-lite exit: $RC"
[ "$RC" -ne 0 ] || { echo "expected a failure" >&2; exit 1; }
grep -qi "did not start Core" "$RUN/serve.log" || {
  echo "missing the managed/pruned explanation" >&2
  tail -40 "$RUN/serve.log"
  exit 1
}

ACTIVE=$($CLI getnetworkinfo | python3 -c 'import json,sys;print(json.load(sys.stdin)["networkactive"])')
echo "core networkactive after e-lite failure: $ACTIVE"
[ "$ACTIVE" = "True" ] || { echo "e-lite paused a Core it does not own" >&2; exit 1; }

echo "external-pruned OK"
