#!/usr/bin/env bash
# Core outage, start while Core is down, cookie rotation, clean shutdown.
. "$(dirname "$0")/../lib/e2e.sh"
e2e_init resil
CORE="$RUN/core"; DATA="$RUN/index"
RPC=18463; ELECTRUM=15005; METRICS=19188
core_start "$CORE" "$RPC"
ADDR=$(core_mine_new "$CORE" "$RPC" resil 101)
SH=$(scripthash_of "$CORE" "$RPC" resil "$ADDR")
ARGS=(--network regtest --core-datadir "$CORE" --core-rpc "http://127.0.0.1:$RPC"
  --data-dir "$DATA" --set pruning.min_free_gib=1 --manage external
  --listen "127.0.0.1:$ELECTRUM" --metrics "127.0.0.1:$METRICS" --no-tls)
"$BIN" init --network regtest --core-datadir "$CORE" --core-rpc "http://127.0.0.1:$RPC" \
  --data-dir "$DATA" >/dev/null
elite_start "$RUN/serve.log" run "${ARGS[@]}"
wait_ready "$METRICS" "$RUN/serve.log"
echo "initial state: READY"

history_at_least() {
  local n
  n=$(electrum history "$ELECTRUM" "$SH") || return 1
  echo "history: $n entries"
  [ "$n" -ge "$1" ] || fail "history missing: $n < $1"
}

echo "== stop Core =="
core_stop "$CORE" "$RPC"
wait_for 15 is_state "$METRICS" CORE_UNAVAILABLE || true
echo "state while Core down: $(status "$METRICS" state)"
history_at_least 101   # confirmed reads keep working from the local index

echo "== restart e-lite while Core is still down =="
elite_stop "$ELITE" "$RUN/serve.log"
elite_start "$RUN/serve2.log" run "${ARGS[@]}"
up() { is_state "$METRICS" READY || is_state "$METRICS" CORE_UNAVAILABLE; }
wait_for 45 up || fail "no status while Core is down" "$RUN/serve2.log"
echo "offline start state: $(status "$METRICS" state)"
history_at_least 101

echo "== restart Core (the cookie rotates) =="
core_start "$CORE" "$RPC"
wait_ready "$METRICS" "$RUN/serve2.log" 0 90
echo "state after Core restart: READY"

elite_stop "$ELITE" "$RUN/serve2.log"
grep -q "stopped at" "$RUN/serve2.log" || fail "no 'stopped at' line" "$RUN/serve2.log"
echo "resilience OK"
