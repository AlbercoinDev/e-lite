#!/usr/bin/env bash
# Privacy settings and the per-txid path without Esplora:
#   - `privacy check` prints the effective values and where each comes from,
#     as text and as JSON; `--set privacy.<key>` overrides one and an
#     out-of-range value is an error that names the key and the range;
#   - with `esplora = []` and an Electrum list, a pruned node serves and
#     reports the per-txid mode with its server count (the provider gate is
#     never held on regtest: its rule is a unit test, `provider_gate_*`);
#   - `server.features` announces the genesis of the network served.
. "$(dirname "$0")/../lib/e2e.sh"
e2e_init privacy
CORE="$RUN/core"; DATA="$RUN/index"; CONF="$RUN/e-lite.toml"
RPC=18492; ELECTRUM=15012; METRICS=19195
e2e_conf "$CONF" core.manage='"managed"' core.prune_mib=1 \
  'core.extra_args=["-listen=0", "-fallbackfee=0.0002"]' \
  rawtx.enabled=true rawtx.use_tor=false tor.mode='"off"'
core_track "$CORE" "$RPC"
# No Esplora endpoint; one Electrum server (local, so plain text is allowed)
# that nobody listens on: it is configured, which is all the gate looks at.
cat >"$RUN/public-servers.toml" <<'TOML'
[regtest]
esplora = []
electrum = ["127.0.0.1:9:t"]
TOML

echo "== privacy check: text =="
"$BIN" privacy check --config "$CONF" >"$RUN/check.txt" 2>"$RUN/check.err" \
  || fail "privacy check failed" "$RUN/check.err"
grep -Eq '^preset +balanced +preset' "$RUN/check.txt" || fail "preset row" "$RUN/check.txt"
grep -Eq '^sessions\.size +50 +preset +1\.\.500' "$RUN/check.txt" || fail "sessions.size row" "$RUN/check.txt"
grep -Eq '^blocks\.decoys +[0-9]+ +rawtx\.decoys' "$RUN/check.txt" || fail "blocks.decoys origin" "$RUN/check.txt"

echo "== privacy check: JSON and --set =="
"$BIN" privacy check --config "$CONF" --json --set privacy.sessions.size=30 \
  >"$RUN/check.json" 2>"$RUN/check.err" || fail "privacy check --json failed" "$RUN/check.err"
python3 - "$RUN/check.json" <<'PY'
import json, sys
doc = json.load(open(sys.argv[1]))
assert doc["valid"] is True, doc
rows = {r["key"]: r for r in doc["values"]}
assert rows["sessions.size"]["value"] == "30", rows["sessions.size"]
assert rows["sessions.size"]["from"] != "preset", rows["sessions.size"]
assert rows["preset"]["value"] == "balanced" and rows["preset"]["from"] == "preset"
assert rows["mode"]["value"] == "tx"
print("sessions.size 30 from", rows["sessions.size"]["from"])
PY

echo "== an out-of-range value is refused with the key and the range =="
set +e
"$BIN" privacy check --config "$CONF" --set privacy.sessions.size=9999 >"$RUN/bad.txt" 2>&1
RC=$?
set -e
[ "$RC" -ne 0 ] || fail "an out-of-range value was accepted" "$RUN/bad.txt"
grep -q "sessions.size = 9999: out of range 1..500" "$RUN/bad.txt" || fail "error text" "$RUN/bad.txt"

echo "== no Esplora, an Electrum list: the per-txid mode is what is reported =="
elite_start "$RUN/serve.log" run --config "$CONF"
wait_for 40 cli "$CORE" "$RPC" getblockcount || fail "Core did not start" "$RUN/serve.log"
core_mine_new "$CORE" "$RPC" privacy 101 >/dev/null
wait_ready "$METRICS" "$RUN/serve.log" 101 120
grep -q "public servers: $RUN/public-servers.toml (file, 0 for regtest)" "$RUN/serve.log" \
  || fail "the file next to the config was not the one used" "$RUN/serve.log"
grep -q "per-txid mode over 1 servers (1 electrum, 0 esplora)" "$RUN/serve.log" \
  || fail "the log does not name the per-txid mode" "$RUN/serve.log"
[ "$(status "$METRICS" rawtx.tx_mode.active)" = "true" ] || fail "/status: per-txid mode not active"
[ "$(status "$METRICS" rawtx.tx_mode.servers)" = "1" ] || fail "/status: per-txid server count"
[ "$(status "$METRICS" rawtx.tx_mode.preset)" = "balanced" ] || fail "/status: preset"

echo "== server.features announces this network's genesis =="
GENESIS=$(cli "$CORE" "$RPC" getblockhash 0)
SERVED=$(electrum call "$ELECTRUM" server.features | python3 -c 'import json,sys;print(json.load(sys.stdin)["genesis_hash"])')
[ "$SERVED" = "$GENESIS" ] || fail "genesis_hash $SERVED, Core has $GENESIS"
[ "$(status "$METRICS" genesis)" = "$GENESIS" ] || fail "/status.genesis differs"

elite_stop "$ELITE" "$RUN/serve.log"
echo "privacy OK"
