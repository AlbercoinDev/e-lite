#!/usr/bin/env bash
# A Core started and owned by e-lite, and e-lite restarting itself:
#   - e-lite starts Core from an empty datadir with manual pruning (-prune=1)
#     and indexes the chain;
#   - `run` is a watching parent plus the node; the node is killed twice
#     (SIGABRT, what a fatal internal error ends with; then SIGKILL, as the
#     out-of-memory killer does) and comes back by itself, with the same Core
#     process and no orphaned Tor;
#   - a clean stop stops Core too and is not followed by a restart; an error
#     exit is final; --no-supervise runs a single process.
. "$(dirname "$0")/../lib/e2e.sh"
e2e_init launch
CORE="$RUN/core"; DATA="$RUN/index"; CONF="$RUN/e-lite.toml"
RPC=18485; ELECTRUM=15008; METRICS=19191
e2e_conf "$CONF" core.manage='"managed"' core.prune_mib=1 \
  'core.extra_args=["-listen=0", "-fallbackfee=0.0002"]' \
  index.batch_blocks=32 pruning.max_core_lag_blocks=64
core_track "$CORE" "$RPC"

# The node is the only e-lite child of the watching parent.
node_of() { pgrep -P "$1" -x e-lite | head -1; }
NODE=""
node_changed() { local now; now="$(node_of "$ELITE" || true)"; [ -n "$now" ] && [ "$now" != "$NODE" ]; }
our_tors() { pgrep -f "tor -f $DATA" | wc -l; }

echo "== e-lite starts Core from an empty datadir =="
elite_start "$RUN/serve.log" run --config "$CONF"
wait_for 40 cli "$CORE" "$RPC" getblockcount || fail "Core did not start" "$RUN/serve.log"
PRUNED=$(cli "$CORE" "$RPC" getblockchaininfo | python3 -c 'import json,sys;print(json.load(sys.stdin)["pruned"])')
[ "$PRUNED" = "True" ] || fail "managed Core is not pruned"
ADDR=$(core_mine_new "$CORE" "$RPC" launch 101)
SH=$(scripthash_of "$CORE" "$RPC" launch "$ADDR")
wait_ready "$METRICS" "$RUN/serve.log" 101 120
NODE="$(node_of "$ELITE")"
[ -n "$NODE" ] || fail "no supervised node under pid $ELITE"
CORE_PID="$(cat "$CORE/regtest/bitcoind.pid")"
echo "parent $ELITE node $NODE core $CORE_PID"

echo "== the node aborts (what a fatal internal error ends with) =="
kill -ABRT "$NODE"
wait_for 60 node_changed || fail "the node was not restarted" "$RUN/serve.log"
wait_ready "$METRICS" "$RUN/serve.log"
grep -q "ended abnormally (signal 6" "$RUN/serve.log" || fail "no supervisor line" "$RUN/serve.log"
grep -q "restarted automatically after the previous run ended abnormally" "$RUN/serve.log" \
  || fail "the restarted node did not say why" "$RUN/serve.log"
grep -q "adopted Core pid $CORE_PID" "$RUN/serve.log" || fail "Core was not adopted" "$RUN/serve.log"
kill -0 "$CORE_PID" || fail "Core was lost"
[ "$(status "$METRICS" supervisor.restarts)" = 1 ] || fail "/status.supervisor.restarts is not 1"
[ "$(status "$METRICS" supervisor.supervised)" = true ] || fail "/status.supervisor.supervised"
status "$METRICS" supervisor.last_reason | grep -q "signal 6" || fail "/status.supervisor.last_reason"
curl -s "http://127.0.0.1:$METRICS/metrics" | grep -qx "e_lite_restarts_total 1" || fail "/metrics restarts"
NODE="$(node_of "$ELITE")"

echo "== the index keeps following the chain =="
core_mine "$CORE" "$RPC" launch 50 "$ADDR"
wait_ready "$METRICS" "$RUN/serve.log" 151

echo "== the node is killed outright (as the out-of-memory killer does) =="
kill -KILL "$NODE"
wait_for 60 node_changed || fail "the node was not restarted" "$RUN/serve.log"
wait_ready "$METRICS" "$RUN/serve.log" 151
kill -0 "$CORE_PID" || fail "Core died"
ENTRIES=$(electrum history "$ELECTRUM" "$SH")
echo "history: $ENTRIES entries"
[ "$ENTRIES" -ge 151 ] || fail "history incomplete"
if grep -q "Tor" "$RUN/serve.log"; then
  one_tor() { [ "$(our_tors)" -le 1 ]; }
  wait_for 30 one_tor || fail "$(our_tors) Tor processes for one instance"
fi

echo "== a clean stop stops Core and is final =="
NODE="$(node_of "$ELITE")"
elite_stop "$ELITE" "$RUN/serve.log"
sleep 1
kill -0 "$NODE" 2>/dev/null && fail "the node outlived the stop"
core_down "$CORE" "$RPC" || fail "Core still running after e-lite shutdown"
[ "$(our_tors)" -eq 0 ] || fail "Tor left behind after a clean stop"

echo "== an error exit is final =="
set +e
"$BIN" run --config "$RUN/does-not-exist.toml" --network not-a-network >"$RUN/bad.log" 2>&1
RC=$?
set -e
[ "$RC" -ne 0 ] || fail "expected an error exit"
grep -q "restarting in" "$RUN/bad.log" && fail "an error exit was restarted" "$RUN/bad.log"

echo "== --no-supervise: one process =="
elite_start "$RUN/single.log" run --config "$CONF" --no-supervise
wait_ready "$METRICS" "$RUN/single.log"
pgrep -P "$ELITE" -x e-lite >/dev/null && fail "--no-supervise still started a child node"
elite_stop "$ELITE" "$RUN/single.log"

echo "== [safety] supervise = false: one process too =="
elite_start "$RUN/single2.log" run --config "$CONF" --set safety.supervise=false
wait_ready "$METRICS" "$RUN/single2.log"
pgrep -P "$ELITE" -x e-lite >/dev/null && fail "supervise = false still started a child node"
[ "$(status "$METRICS" supervisor.supervised)" = false ] || fail "/status says supervised"
elite_stop "$ELITE" "$RUN/single2.log"
echo "launch-core OK"
