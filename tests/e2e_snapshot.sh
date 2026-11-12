#!/usr/bin/env bash
# Snapshot export/import across networks: export-snapshot --with-core bundles
# the network's index and Core datadir in one archive; import-snapshot
# restores and verifies the pair. Uses the default per-network layout
# (<data-dir>/bitcoin) so the nested-datadir exclusion is exercised too.
set -euo pipefail

cd "$(dirname "$0")/.."
BIN="${E_LITE_BIN:-$PWD/target/debug/e-lite}"
[ -x "$BIN" ] || { echo "build first: cargo build -p e-lite" >&2; exit 1; }
command -v bitcoind >/dev/null || { echo "bitcoind not found" >&2; exit 1; }

RUN="$(mktemp -d "${TMPDIR:-/tmp}/e-lite-snapshot-XXXXXX")"
DATA="$RUN/main"; DATA2="$RUN/restored"; DATA3="$RUN/verified"
CONF="$RUN/e-lite.toml"; CONF2="$RUN/restored.toml"; CONF3="$RUN/verified.toml"
RPC=18487; ELECTRUM=15010; METRICS=19193

cleanup() {
  set +e
  for f in "$RUN/serve.pid" "$RUN/extra.pid"; do
    [ -f "$f" ] && kill "$(cat "$f")" 2>/dev/null
  done
  pkill -f "bitcoind .*$RUN" 2>/dev/null
  sleep 1
  rm -rf "$RUN"
}
trap cleanup EXIT INT TERM

mkdir -p "$DATA" "$DATA2" "$DATA3"
make_conf() {
  local conf="$1" datadir="$2" electrum="$3"
  cat >"$conf" <<EOF
[network]
name = "regtest"
data_dir = "$datadir"

[core]
manage = "managed"
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
listen = "127.0.0.1:$electrum"
tls = false

[metrics]
listen = "127.0.0.1:$METRICS"

[rawtx]
enabled = false
EOF
}
make_conf "$CONF" "$DATA" "$ELECTRUM"
# Restore configs reuse the default nested layout (<data-dir>/bitcoin).
make_conf "$CONF2" "$DATA2" "$((ELECTRUM + 1))"
make_conf "$CONF3" "$DATA3" "$((ELECTRUM + 2))"

echo "== e-lite indexes a small regtest chain =="
"$BIN" run --config "$CONF" >"$RUN/serve.log" 2>&1 &
echo $! >"$RUN/serve.pid"

CLI="bitcoin-cli -regtest -datadir=$DATA/bitcoin -rpcport=$RPC"
for _ in $(seq 1 200); do $CLI getblockcount >/dev/null 2>&1 && break; sleep 0.2; done
$CLI createwallet snapshot_e2e >/dev/null
ADDR=$($CLI -rpcwallet=snapshot_e2e getnewaddress)
$CLI -rpcwallet=snapshot_e2e generatetoaddress 151 "$ADDR" >/dev/null

state() {
  curl -s "http://127.0.0.1:$METRICS/status" 2>/dev/null \
    | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["state"])
except Exception: print("DOWN")' 2>/dev/null || echo DOWN
}
for _ in $(seq 1 120); do [ "$(state)" = READY ] && break; sleep 0.5; done
echo "state: $(state)"
[ "$(state)" = READY ] || { tail -40 "$RUN/serve.log"; exit 1; }

kill -TERM "$(cat "$RUN/serve.pid")"
set +e; wait "$(cat "$RUN/serve.pid")"; set -e

echo "== wait for e-lite's Core to shut down =="
for _ in $(seq 1 360); do $CLI getblockcount >/dev/null 2>&1 || break; sleep 0.5; done
if $CLI getblockcount >/dev/null 2>&1; then
  echo "Core still running after e-lite shutdown" >&2
  exit 1
fi
echo "core tip was: $($CLI -datadir=$DATA/bitcoin getblockcount 2>/dev/null || echo stopped)"
INDEX_TIP=$("$BIN" export-snapshot --config "$CONF" | python3 -c 'import sys
for line in sys.stdin:
    if line.startswith("tip:"): print(line.split()[1]); break')
echo "index tip: $INDEX_TIP"

echo "== export with Core running must be refused =="
# Start a standalone Core on the same datadir, then try to export.
bitcoind -datadir="$DATA/bitcoin" -chain=regtest -rpcport=$RPC -listen=0 -daemon >/dev/null 2>&1
for _ in $(seq 1 100); do $CLI getblockcount >/dev/null 2>&1 && break; sleep 0.2; done
if "$BIN" export-snapshot --config "$CONF" --output "$RUN/should-fail.tar.gz" \
    --with-core >"$RUN/export-fail.log" 2>&1; then
  echo "export while Core runs must fail" >&2
  exit 1
fi
grep -q "Bitcoin Core is running" "$RUN/export-fail.log" \
  || { cat "$RUN/export-fail.log"; echo "unexpected refusal reason" >&2; exit 1; }
echo "refused as expected: $(tail -1 "$RUN/export-fail.log")"
bitcoin-cli -regtest -datadir="$DATA/bitcoin" -rpcport=$RPC stop >/dev/null
sleep 1

echo "== index-only export must not contain the nested Core datadir =="
"$BIN" export-snapshot --config "$CONF" --output "$RUN/index-only.tar.gz" >"$RUN/export1.log"
if tar -tzf "$RUN/index-only.tar.gz" | grep -q '^bitcoin/'; then
  echo "index-only snapshot still packs the Core datadir" >&2
  tar -tzf "$RUN/index-only.tar.gz" | grep '^bitcoin/' | head -5 >&2
  exit 1
fi
echo "index-only archive: $(tar -tzf "$RUN/index-only.tar.gz" | wc -l) entries, no bitcoin/"

echo "== full export (--with-core, hashes by default) =="
"$BIN" export-snapshot --config "$CONF" --output "$RUN/full.tar.gz" \
  --with-core >"$RUN/export2.log"
cat "$RUN/export2.log"
TARLIST=$(tar -tzf "$RUN/full.tar.gz")
echo "$TARLIST" | grep -q '^bitcoin/regtest/chainstate/' \
  || { echo "chainstate missing from the archive" >&2; exit 1; }
if echo "$TARLIST" \
    | grep -E '^bitcoin/[^/]*/?(wallets/|peers\.dat|\.cookie|mempool\.dat|debug\.log)'; then
  echo "runtime state leaked into the archive" >&2
  exit 1
fi
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
[ "$EXPORTED_DIGEST" = "$RESTORED_DIGEST" ] \
  || { echo "digest mismatch after restore: $EXPORTED_DIGEST != $RESTORED_DIGEST" >&2; exit 1; }
[ -d "$DATA2/bitcoin/regtest/chainstate" ] || { echo "Core tree not restored" >&2; exit 1; }

echo "== a second import into the same target must fail =="
if "$BIN" import-snapshot --config "$CONF2" --input "$RUN/full.tar.gz" \
    >"$RUN/import-fail.log" 2>&1; then
  echo "import into a non-empty target must fail" >&2
  exit 1
fi
grep -q "is not empty" "$RUN/import-fail.log" || { cat "$RUN/import-fail.log"; exit 1; }

echo "== import --verify=full opens the restored Core and verifies it =="
"$BIN" import-snapshot --config "$CONF3" --input "$RUN/full.tar.gz" \
  --verify=full >"$RUN/import2.log"
cat "$RUN/import2.log"
grep -q "verify=full: OK" "$RUN/import2.log" || { echo "full verification failed" >&2; exit 1; }

echo "e2e_snapshot: OK"
