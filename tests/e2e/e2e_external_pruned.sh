#!/usr/bin/env bash
# A pruned Core that e-lite did not start must not be used for a from-scratch
# IBD: e-lite fails with instructions and never pauses the node.
. "$(dirname "$0")/../lib/e2e.sh"
e2e_init external
CORE="$RUN/core"; DATA="$RUN/index"; CONF="$RUN/e-lite.toml"
RPC=18486; ELECTRUM=15009; METRICS=19192
core_start "$CORE" "$RPC" -prune=1 -fastprune
core_mine_new "$CORE" "$RPC" ext 101 >/dev/null
e2e_conf "$CONF"

set +e
"$BIN" run --config "$CONF" >"$RUN/serve.log" 2>&1
RC=$?
set -e
echo "e-lite exit: $RC"
[ "$RC" -ne 0 ] || fail "expected a failure"
grep -qi "did not start Core" "$RUN/serve.log" \
  || fail "missing the managed/pruned explanation" "$RUN/serve.log"

ACTIVE=$(cli "$CORE" "$RPC" getnetworkinfo | python3 -c 'import json,sys;print(json.load(sys.stdin)["networkactive"])')
echo "core networkactive after e-lite failure: $ACTIVE"
[ "$ACTIVE" = "True" ] || fail "e-lite paused a Core it does not own"
echo "external-pruned OK"
