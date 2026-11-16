#!/usr/bin/env bash
# Automatic restart without an init system: `e-lite run` is a watching parent
# plus the node as its child. The node is killed twice (the abort a fatal
# internal error ends with, then SIGKILL as the out-of-memory killer would)
# and must come back by itself, with the managed Core left running and no
# orphaned Tor. A clean stop, an error exit and --no-supervise are never
# followed by a restart.
set -euo pipefail

cd "$(dirname "$0")/../.."
BIN="${E_LITE_BIN:-$PWD/target/debug/e-lite}"
[ -x "$BIN" ] || { echo "build first: cargo build -p e-lite" >&2; exit 1; }
command -v bitcoind >/dev/null || { echo "bitcoind not found" >&2; exit 1; }

RUN="$(mktemp -d "${TMPDIR:-/tmp}/e-lite-supervisor-XXXXXX")"
CORE="$RUN/core"; DATA="$RUN/index"; CONF="$RUN/e-lite.toml"
RPC=18495; ELECTRUM=15018; METRICS=19195

cleanup() {
  set +e
  [ -f "$RUN/serve.pid" ] && kill "$(cat "$RUN/serve.pid")" 2>/dev/null
  sleep 1
  pkill -f "bitcoind .*$CORE" 2>/dev/null
  pkill -f "tor -f $DATA" 2>/dev/null
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
prune_mib = 1
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
tls = false

[metrics]
listen = "127.0.0.1:$METRICS"

[rawtx]
enabled = false
EOF

CLI="bitcoin-cli -regtest -datadir=$CORE -rpcport=$RPC"
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
except Exception: print(-1)' 2>/dev/null || echo -1
}
wait_ready() {
  for _ in $(seq 1 240); do [ "$(state)" = READY ] && return 0; sleep 0.5; done
  echo "not READY (state $(state))" >&2; tail -40 "$RUN/serve.log" >&2; return 1
}
# The node: the only e-lite child of the watching parent.
node_pid() { pgrep -P "$1" -x e-lite | head -1; }
wait_new_node() {
  local parent=$1 old=$2 now
  for _ in $(seq 1 120); do
    now="$(node_pid "$parent" || true)"
    [ -n "$now" ] && [ "$now" != "$old" ] && { echo "$now"; return 0; }
    sleep 0.25
  done
  echo "the node was not restarted" >&2; tail -40 "$RUN/serve.log" >&2; return 1
}
our_tors() { pgrep -f "tor -f $DATA" | wc -l; }

echo "== start: a parent and the node as its child =="
"$BIN" run --config "$CONF" >"$RUN/serve.log" 2>&1 &
PARENT=$!
echo "$PARENT" >"$RUN/serve.pid"
for _ in $(seq 1 200); do $CLI getblockcount >/dev/null 2>&1 && break; sleep 0.2; done
$CLI createwallet sup >/dev/null
ADDR=$($CLI -rpcwallet=sup getnewaddress)
$CLI -rpcwallet=sup generatetoaddress 101 "$ADDR" >/dev/null
wait_ready
NODE="$(node_pid "$PARENT")"
[ -n "$NODE" ] || { echo "no supervised node under pid $PARENT" >&2; exit 1; }
CORE_PID="$(cat "$CORE/regtest/bitcoind.pid")"
echo "parent $PARENT node $NODE core $CORE_PID height $(height)"

echo "== the node aborts (what a fatal internal error ends with) =="
kill -ABRT "$NODE"
NODE2="$(wait_new_node "$PARENT" "$NODE")"
wait_ready
grep -q "ended abnormally (signal 6" "$RUN/serve.log" || { echo "no supervisor line" >&2; exit 1; }
grep -q "restarted automatically after the previous run ended abnormally" "$RUN/serve.log" \
  || { echo "the restarted node did not say why" >&2; exit 1; }
[ "$(cat "$CORE/regtest/bitcoind.pid")" = "$CORE_PID" ] && kill -0 "$CORE_PID" \
  || { echo "Core was restarted or lost" >&2; exit 1; }
grep -q "adopted Core pid $CORE_PID" "$RUN/serve.log" || { echo "Core was not adopted" >&2; exit 1; }
echo "node $NODE -> $NODE2, Core $CORE_PID untouched, height $(height)"

echo "== the index still follows the chain =="
$CLI -rpcwallet=sup generatetoaddress 20 "$ADDR" >/dev/null
for _ in $(seq 1 120); do [ "$(height)" -ge 121 ] && break; sleep 0.5; done
[ "$(height)" -ge 121 ] || { echo "index stuck at $(height)" >&2; tail -30 "$RUN/serve.log" >&2; exit 1; }

echo "== the node is killed outright (as the out-of-memory killer does) =="
kill -KILL "$NODE2"
NODE3="$(wait_new_node "$PARENT" "$NODE2")"
wait_ready
[ "$(height)" -ge 121 ] || { echo "index lost blocks: $(height)" >&2; exit 1; }
kill -0 "$CORE_PID" || { echo "Core died" >&2; exit 1; }
echo "node $NODE2 -> $NODE3, height $(height)"

if command -v tor >/dev/null && grep -q "Tor" "$RUN/serve.log"; then
  # One Tor for this instance: the ones of the killed nodes are gone.
  for _ in $(seq 1 60); do [ "$(our_tors)" -le 1 ] && break; sleep 0.5; done
  [ "$(our_tors)" -le 1 ] || { echo "$(our_tors) Tor processes for one instance" >&2; exit 1; }
  echo "tor processes for this instance: $(our_tors)"
fi

echo "== a clean stop is not followed by a restart =="
kill -TERM "$PARENT"
set +e
wait "$PARENT"
RC=$?
set -e
[ "$RC" -eq 0 ] || { echo "parent exit $RC" >&2; tail -20 "$RUN/serve.log" >&2; exit 1; }
sleep 1
if pgrep -P "$PARENT" >/dev/null 2>&1 || kill -0 "$NODE3" 2>/dev/null; then
  echo "the node outlived the stop" >&2; exit 1
fi
if $CLI getblockcount >/dev/null 2>&1; then
  echo "Core still running after a clean stop" >&2; exit 1
fi
[ "$(our_tors)" -eq 0 ] || { echo "Tor left behind after a clean stop" >&2; exit 1; }

echo "== an error exit is final =="
set +e
"$BIN" run --config "$RUN/does-not-exist.toml" --network not-a-network >"$RUN/bad.log" 2>&1
RC=$?
set -e
[ "$RC" -ne 0 ] || { echo "expected an error exit" >&2; exit 1; }
if grep -q "restarting in" "$RUN/bad.log"; then
  echo "an error exit was restarted" >&2; cat "$RUN/bad.log" >&2; exit 1
fi

echo "== --no-supervise: one process, as before =="
"$BIN" run --config "$CONF" --no-supervise >"$RUN/single.log" 2>&1 &
SINGLE=$!
echo "$SINGLE" >"$RUN/serve.pid"
wait_ready
if pgrep -P "$SINGLE" -x e-lite >/dev/null; then
  echo "--no-supervise still started a child node" >&2; exit 1
fi
kill -TERM "$SINGLE"
set +e
wait "$SINGLE"
RC=$?
set -e
[ "$RC" -eq 0 ] || { echo "single-process exit $RC" >&2; tail -20 "$RUN/single.log" >&2; exit 1; }

echo "supervisor OK"
