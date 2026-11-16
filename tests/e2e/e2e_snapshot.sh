#!/usr/bin/env bash
# Snapshot export/import: export-snapshot --with-core bundles the network's
# index and Core datadir in one archive; import-snapshot restores and
# verifies the pair. Uses the default per-network layout (<data-dir>/bitcoin)
# so the nested-datadir exclusion is exercised too.
. "$(dirname "$0")/../lib/e2e.sh"
e2e_init snapshot
DATA="$RUN/main"; DATA2="$RUN/restored"; DATA3="$RUN/verified"
CONF="$RUN/e-lite.toml"; CONF2="$RUN/restored.toml"; CONF3="$RUN/verified.toml"
RPC=18487; ELECTRUM=15010; METRICS=19193
CORE=""   # nested default: <data-dir>/bitcoin
make_conf() {
  DATA="$2" ELECTRUM="$3" e2e_conf "$1" core.manage='"managed"' core.prune_mib=1 \
    'core.extra_args=["-listen=0", "-fallbackfee=0.0002"]' \
    index.batch_blocks=32 pruning.max_core_lag_blocks=64
}
make_conf "$CONF" "$DATA" "$ELECTRUM"
make_conf "$CONF2" "$DATA2" "$((ELECTRUM + 1))"
make_conf "$CONF3" "$DATA3" "$((ELECTRUM + 2))"
for d in "$DATA" "$DATA2" "$DATA3"; do core_track "$d/bitcoin" "$RPC"; done
CLI=(cli "$DATA/bitcoin" "$RPC")

echo "== e-lite indexes a small regtest chain =="
elite_start "$RUN/serve.log" run --config "$CONF"
wait_for 40 "${CLI[@]}" getblockcount || fail "managed Core did not start" "$RUN/serve.log"
core_mine_new "$DATA/bitcoin" "$RPC" snapshot_e2e 151 >/dev/null
wait_ready "$METRICS" "$RUN/serve.log" 151 60
elite_stop "$ELITE" "$RUN/serve.log"

echo "== e-lite's Core shuts down with it =="
wait_for 180 core_down "$DATA/bitcoin" "$RPC" || fail "Core still running after e-lite shutdown"
INDEX_TIP=$("$BIN" export-snapshot --config "$CONF" | python3 -c 'import sys
for line in sys.stdin:
    if line.startswith("tip:"): print(line.split()[1]); break')
echo "index tip: $INDEX_TIP"

echo "== export with Core running must be refused =="
# Start a standalone Core on the same datadir, then try to export.
core_start "$DATA/bitcoin" "$RPC"
if "$BIN" export-snapshot --config "$CONF" --output "$RUN/should-fail.tar.gz" \
    --with-core >"$RUN/export-fail.log" 2>&1; then
  fail "export while Core runs must fail"
fi
grep -q "Bitcoin Core is running" "$RUN/export-fail.log" || fail "unexpected refusal reason" "$RUN/export-fail.log"
echo "refused as expected: $(tail -1 "$RUN/export-fail.log")"
core_stop "$DATA/bitcoin" "$RPC"

echo "== index-only export must not contain the nested Core datadir =="
"$BIN" export-snapshot --config "$CONF" --output "$RUN/index-only.tar.gz" >"$RUN/export1.log"
tar -tzf "$RUN/index-only.tar.gz" | grep -q '^bitcoin/' && fail "index-only snapshot still packs the Core datadir"
echo "index-only archive: $(tar -tzf "$RUN/index-only.tar.gz" | wc -l) entries, no bitcoin/"

echo "== full export (--with-core, hashes by default) =="
"$BIN" export-snapshot --config "$CONF" --output "$RUN/full.tar.gz" \
  --with-core >"$RUN/export2.log"
cat "$RUN/export2.log"
TARLIST=$(tar -tzf "$RUN/full.tar.gz")
echo "$TARLIST" | grep -q '^bitcoin/regtest/chainstate/' || fail "chainstate missing from the archive"
echo "$TARLIST" | grep -E '^bitcoin/[^/]*/?(wallets/|peers\.dat|\.cookie|mempool\.dat|debug\.log)' \
  && fail "runtime state leaked into the archive"
python3 - "$RUN" "$INDEX_TIP" <<'PY'
import json, subprocess, sys, tarfile
run, index_tip = sys.argv[1], sys.argv[2]
with tarfile.open(f"{run}/full.tar.gz") as t:
    names = t.getnames()
    m = json.load(t.extractfile("e-lite-snapshot.json"))
    head = json.load(t.extractfile("e-lite-snapshot.head.json"))
# Format 3: the manifest is also the first entry, for an early check.
assert names[0] == "e-lite-snapshot.head.json", names[0]
assert names[-1] == "e-lite-snapshot.json", names[-1]
assert head["network"] == "regtest" and head["digest"] == m["digest"], head
assert m["format"] == 3, m["format"]
assert m["network"] == "regtest", m["network"]
assert int(m["height"]) == int(index_tip), (m["height"], index_tip)
core = m["core"]
assert core["tip_height"] >= int(index_tip), core
assert core["version"].startswith("/Satoshi:"), core["version"]
assert m["files"] and all(e["sha256"] is not None for e in m["files"]), "hashes missing"
print(f"manifest ok: format 3, core tip {core['tip_height']}, {len(m['files'])} hashed files")
PY

echo "== import (hash verification) into an empty target =="
"$BIN" import-snapshot --config "$CONF2" --input "$RUN/full.tar.gz" --verify >"$RUN/import1.log"
cat "$RUN/import1.log"
EXPORTED_DIGEST=$(grep -oP 'logical_snapshot_hash: \K[0-9a-f]+' "$RUN/export2.log")
RESTORED_DIGEST=$(grep -oP 'logical_snapshot_hash: \K[0-9a-f]+' "$RUN/import1.log")
[ "$EXPORTED_DIGEST" = "$RESTORED_DIGEST" ] || fail "digest mismatch after restore: $EXPORTED_DIGEST != $RESTORED_DIGEST"
[ -d "$DATA2/bitcoin/regtest/chainstate" ] || fail "Core tree not restored"

echo "== a second import into the same target must fail =="
if "$BIN" import-snapshot --config "$CONF2" --input "$RUN/full.tar.gz" >"$RUN/import-fail.log" 2>&1; then
  fail "import into a non-empty target must fail"
fi
grep -q "is not empty" "$RUN/import-fail.log" || fail "unexpected import refusal" "$RUN/import-fail.log"

echo "== import --verify=full opens the restored Core and verifies it =="
"$BIN" import-snapshot --config "$CONF3" --input "$RUN/full.tar.gz" \
  --verify=full >"$RUN/import2.log"
cat "$RUN/import2.log"
grep -q "verify=full: OK" "$RUN/import2.log" || fail "full verification failed" "$RUN/import2.log"

echo "e2e_snapshot: OK"
