#!/usr/bin/env bash
# e-lite starts first with no Core, waits in WAITING_FOR_CORE, then Core
# appears and e-lite initializes and indexes without a restart.
. "$(dirname "$0")/../lib/e2e.sh"
e2e_init wait
CORE="$RUN/core"; DATA="$RUN/index"; CONF="$RUN/e-lite.toml"
RPC=18484; ELECTRUM=15007; METRICS=19190
e2e_conf "$CONF"

echo "== start e-lite with no Core =="
elite_start "$RUN/serve.log" run --config "$CONF"
# No network identity before Core answers, so no HTTP transport yet: the
# process logs the wait and retries with backoff.
wait_for 30 grep -q "waiting for Bitcoin Core" "$RUN/serve.log" \
  || fail "no 'waiting for Bitcoin Core' line" "$RUN/serve.log"

echo "== start Core =="
core_start "$CORE" "$RPC"
ADDR=$(core_mine_new "$CORE" "$RPC" wait 101)
SH=$(scripthash_of "$CORE" "$RPC" wait "$ADDR")
wait_ready "$METRICS" "$RUN/serve.log" 101 60
ENTRIES=$(electrum history "$ELECTRUM" "$SH")
echo "history: $ENTRIES entries"
[ "$ENTRIES" -ge 101 ] || fail "history incomplete"

elite_stop "$ELITE" "$RUN/serve.log"
echo "wait-for-core OK"
