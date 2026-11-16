#!/usr/bin/env bash
# e-lite starts Core with manual pruning (`-prune=1 -fastprune`), indexes from
# genesis while both run, the prune gate prunes safely, a SIGKILL after the
# prune loses nothing, and the server keeps answering from its index.
. "$(dirname "$0")/../lib/e2e.sh"
e2e_init pruned
CORE="$RUN/core"; DATA="$RUN/index"; CONF="$RUN/e-lite.toml"
RPC=18483; ELECTRUM=15006; METRICS=19189

# Seed the datadir (wallet + 101 blocks), then let e-lite own and restart Core.
core_start "$CORE" "$RPC" -prune=1 -fastprune
ADDR=$(core_mine_new "$CORE" "$RPC" pruned 101)
SH=$(scripthash_of "$CORE" "$RPC" pruned "$ADDR")
core_stop "$CORE" "$RPC"
e2e_conf "$CONF" core.manage='"managed"' core.prune_mib=1 \
  'core.extra_args=["-fastprune", "-listen=0"]' core.timeout_seconds=30 \
  index.block_source='"auto"' pruning.max_core_lag_blocks=64 \
  pruning.min_gain_blocks=288 pruning.prune_defer_free_gib=0

elite_start "$RUN/serve.log" run --config "$CONF"
wait_for 30 cli "$CORE" "$RPC" getblockcount || fail "managed Core did not start" "$RUN/serve.log"
cli "$CORE" "$RPC" loadwallet pruned >/dev/null 2>&1 || true
wait_ready "$METRICS" "$RUN/serve.log"

echo "== mine 2000 blocks (Core is managed with manual prune) =="
core_mine "$CORE" "$RPC" pruned 2000 "$ADDR"
wait_ready "$METRICS" "$RUN/serve.log" 2101 180
echo "indexed height: $(status "$METRICS" indexed_height)"

echo "== wait for the prune gate =="
pruneheight() { cli "$CORE" "$RPC" getblockchaininfo | python3 -c 'import json,sys;print(json.load(sys.stdin)["pruneheight"])'; }
pruned() { [ "$(pruneheight)" -gt 0 ]; }
wait_for 120 pruned || fail "the prune gate did not prune" "$RUN/serve.log"
echo "core pruneheight: $(pruneheight)"
if cli "$CORE" "$RPC" getblock "$(cli "$CORE" "$RPC" getblockhash 10)" >/dev/null 2>&1; then
  fail "expected block 10 to be pruned from Core"
fi
ENTRIES=$(electrum history "$ELECTRUM" "$SH")
echo "history from the e-lite index: $ENTRIES entries"
[ "$ENTRIES" -ge 2000 ] || fail "history incomplete"

echo "== pipeline work accounting =="
status "$METRICS" pipeline | python3 -c '
import json, sys
d = json.load(sys.stdin)
print("pipeline:", d)
assert d["prepared"] >= 2000 and d["applied"] >= 2000, d
# Discarded work is the window in flight of each 512-block call.
assert d["discarded"] * 20 < d["prepared"], d'

echo "== SIGKILL after a prune, then restart =="
# The prune barrier made the tip durable. Kill without any cleanup, restart
# and check the index still serves the full history.
kill -KILL "$ELITE"
wait "$ELITE" 2>/dev/null || true
elite_start "$RUN/serve2.log" run --config "$CONF"
wait_ready "$METRICS" "$RUN/serve2.log" 2101 180
ENTRIES=$(electrum history "$ELECTRUM" "$SH")
echo "history after recovery: $ENTRIES entries"
[ "$ENTRIES" -ge 2000 ] || fail "history lost after recovery"

elite_stop "$ELITE" "$RUN/serve2.log"
echo "pruned-ibd OK"
