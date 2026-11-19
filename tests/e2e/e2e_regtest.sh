#!/usr/bin/env bash
# The main path end to end on regtest: doctor, init, index, run; Electrum
# 1.4 queries over plain and TLS, /status and /metrics, the Sparrow-like
# sequence and the 1.6/1.7 checks (tests/e2e/*.py), clean shutdown, the
# rawtx cache commands and a snapshot round trip.
. "$(dirname "$0")/../lib/e2e.sh"
e2e_init regtest
CORE="$RUN/core"; DATA="$RUN/index"; TLS_DIR="$RUN/tls"
RPC_PORT="${E_LITE_E2E_RPC_PORT:-18443}"
ELECTRUM_PORT="${E_LITE_E2E_ELECTRUM_PORT:-15001}"
TLS_PORT="${E_LITE_E2E_TLS_PORT:-15002}"
METRICS_PORT="${E_LITE_E2E_METRICS_PORT:-19184}"

mkdir -p "$DATA" "$TLS_DIR"
cp crates/el-serve/tests/fixtures/tls-cert.pem "$TLS_DIR/cert.pem"
cp crates/el-serve/tests/fixtures/tls-key.pem "$TLS_DIR/key.pem"
cp crates/el-serve/tests/fixtures/tls-ca.pem "$TLS_DIR/ca.pem"
core_start "$CORE" "$RPC_PORT"
ADDR=$(core_mine_new "$CORE" "$RPC_PORT" e2e 101)
HEIGHT=$(cli "$CORE" "$RPC_PORT" getblockcount)
SCRIPT_HASH=$(scripthash_of "$CORE" "$RPC_PORT" e2e "$ADDR")
# Python blocks below import the shared client from tests/lib/e2e.py.
export PYTHONPATH="$E2E_ROOT/tests/lib${PYTHONPATH:+:$PYTHONPATH}"

COMMON=(--network regtest --core-datadir "$CORE" --core-rpc "http://127.0.0.1:$RPC_PORT" --data-dir "$DATA" --set pruning.min_free_gib=1 --public-servers "$RUN/public-servers.toml")
if "$BIN" doctor "${COMMON[@]}" --core-rpc "http://192.0.2.1:8332" \
  --set core.require_loopback_rpc=true >/dev/null 2>&1; then
  fail "strict Core RPC check did not reject a non-loopback endpoint"
fi
"$BIN" init "${COMMON[@]}"
"$BIN" doctor "${COMMON[@]}" --listen "127.0.0.1:$ELECTRUM_PORT" --metrics "127.0.0.1:$METRICS_PORT" \
  --tls-listen "127.0.0.1:$TLS_PORT" --set server.tls_cert="$TLS_DIR/cert.pem" --set server.tls_key="$TLS_DIR/key.pem" \
  >"$RUN/doctor.log" 2>&1
grep -q "public servers:" "$RUN/doctor.log" || fail "doctor did not report the public servers" "$RUN/doctor.log"
[ -f "$RUN/public-servers.toml" ] || fail "public-servers.toml was not created"
"$BIN" index "${COMMON[@]}" --no-follow --set index.batch_blocks=16 --set index.reorg_window=8 --set index.milestone_blocks=50 \
  >"$RUN/index.log" 2>&1
grep -q "milestone" "$RUN/index.log" || fail "no milestone line" "$RUN/index.log"
grep -q "%" "$RUN/index.log" || fail "no progress percentage" "$RUN/index.log"
[ -f "$DATA/e-lite.log" ] || fail "missing e-lite.log"
grep -q "tuning profile" "$DATA/e-lite.log" || fail "no tuning line" "$DATA/e-lite.log"

cat >"$DATA/banner.txt" <<'EOF'
[indexing]
CUSTOM INDEX {height} {percent} {state}
[ready]
CUSTOM READY {height} {percent} {state}
EOF

elite_start "$RUN/serve.log" run "${COMMON[@]}" --no-follow --set index.reorg_window=8 \
  --log-level debug \
  --listen "127.0.0.1:$ELECTRUM_PORT" --metrics "127.0.0.1:$METRICS_PORT" \
  --tls-listen "127.0.0.1:$TLS_PORT" --set server.tls_cert="$TLS_DIR/cert.pem" --set server.tls_key="$TLS_DIR/key.pem" \
  --set server.banner_file="$DATA/banner.txt"
# `run` waits up to 30 s for a freshly created managed Tor to bootstrap
# before opening the listeners.
healthy() { curl -sf "http://127.0.0.1:$METRICS_PORT/healthz"; }
wait_for 50 healthy || fail "no /healthz" "$RUN/serve.log"
grep -q "started at" "$RUN/serve.log" || fail "no 'started at' line" "$RUN/serve.log"
grep -qE "p2p discovery failed|Connection refused|memory pressure" "$RUN/serve.log" \
  && fail "unexpected startup error" "$RUN/serve.log"

curl -sf "http://127.0.0.1:$METRICS_PORT/status" >"$RUN/status.json"
curl -sf "http://127.0.0.1:$METRICS_PORT/metrics" >"$RUN/metrics.txt"
grep -q '"state":"READY"' "$RUN/status.json"
grep -q 'e_lite_indexed_height' "$RUN/metrics.txt"
grep -q 'e_lite_rate_limited_total' "$RUN/metrics.txt"
python3 - "$RUN/status.json" <<'PY'
import json, sys
st = json.load(open(sys.argv[1]))
mem = st["memory"]
assert mem["total_mib"] > 0, mem
assert mem["available_mib"] > 0, mem
assert st["tuning"]["cache_mib"] >= 128, st["tuning"]
assert "public_servers" in st, st
assert isinstance(st["public_servers"], list), st
assert st["rate_limit_rps"] == 200, st  # uniform soft throttle
assert st["rate_burst"] == 5000, st
assert "disk" in st and st["disk"]["total_bytes"] > 0, st
assert st["core_rpc_loopback"] is True, st
assert st["provider_ready"] is False, st  # regtest has no public providers
assert st["clearnet_fallback"] is False, st
assert st["ready"] is True and st["fully_synced"] is True, st
print("memory accounting OK:", mem["total_mib"], mem["available_mib"], "MiB")
PY

python3 - "$ELECTRUM_PORT" "$SCRIPT_HASH" "$HEIGHT" <<'PY'
import sys
from e2e import Electrum
port, sh, height = int(sys.argv[1]), sys.argv[2], int(sys.argv[3])
el = Electrum(port)
call = lambda _id, method, params: el.call(method, params)
assert call(1, "server.version", ["e2e", "1.4"])[1] == "1.4"
assert call(2, "blockchain.headers.subscribe", [])["height"] == height
h = call(3, "blockchain.scripthash.get_history", [sh])
assert h and len(h) == height, len(h)  # heights 1..height pay to the mined address
bal = call(4, "blockchain.scripthash.get_balance", [sh])
assert bal["confirmed"] > 0
assert len(call(5, "blockchain.scripthash.listunspent", [sh])) == len(h)
assert len(call(6, "blockchain.block.header", [0])) == 160
txid, txheight = h[0]["tx_hash"], h[0]["height"]
raw1 = call(7, "blockchain.transaction.get", [txid])
assert len(raw1) > 100
assert call(8, "blockchain.transaction.get", [txid]) == raw1
assert call(9, "blockchain.transaction.get_merkle", [txid, txheight])["pos"] >= 0
assert call(10, "blockchain.transaction.id_from_pos", [txheight, 0, False]) == txid
assert call(11, "blockchain.scripthash.get_history", ["00"*32]) == []
banner = call(12, "server.banner", [])
assert "CUSTOM" in banner and str(height) in banner, banner
missing = el.request("blockchain.transaction.get", ["12" * 32])
assert missing["error"]["code"] == -32000, missing
assert "(code -5)" in missing["error"]["message"], missing
print("electrum queries OK")
PY

echo "== Electrum over TLS =="
python3 - "$TLS_PORT" "$HEIGHT" "$TLS_DIR/ca.pem" <<'PY'
import sys
from e2e import Electrum
port, height, ca = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
el = Electrum(port, tls=True, cafile=ca)   # full certificate validation
assert el.call("server.version", ["e2e-tls", "1.4"])[1] == "1.4"
assert el.call("blockchain.headers.subscribe")["height"] == height
assert el.call("blockchain.scripthash.get_history", ["00" * 32]) == []
print("electrum TLS queries OK")
PY
curl -sf "http://127.0.0.1:$METRICS_PORT/status" >"$RUN/status-tls.json"
python3 - "$RUN/status-tls.json" "$TLS_PORT" <<'PY'
import json, sys
st = json.load(open(sys.argv[1]))
assert st["tls_listen"] == f"127.0.0.1:{int(sys.argv[2])}", st.get("tls_listen")
assert st["tls_handshakes"] >= 1, st.get("tls_handshakes")
assert st["tls_handshake_failures"] == 0, st.get("tls_handshake_failures")
print("TLS status counters OK")
PY

echo "== Sparrow-like sequence: fees, broadcast, mempool, notifications =="
python3 tests/e2e/e2e_sparrow.py "$ELECTRUM_PORT" "$CORE" "$RPC_PORT" "$ADDR"

echo "== Electrum 1.6: headers array, mempool info, fee mode, package ==="
python3 tests/e2e/e2e_electrum16.py "$ELECTRUM_PORT" "$CORE" "$RPC_PORT" "$ADDR"

echo "== Electrum 1.7: scriptpubkey, outpoint, testmempoolaccept =="
python3 tests/e2e/e2e_electrum17.py "$ELECTRUM_PORT" "$CORE" "$RPC_PORT" "$ADDR"

STATUS=$(curl -s "http://127.0.0.1:$METRICS_PORT/status")
python3 - "$STATUS" <<'PY'
import json, sys
raw = json.loads(sys.argv[1])["rawtx"]
assert raw["cache_puts"] >= 1, raw
assert raw["cache_hits"] >= 1, raw
assert raw["unavailable"] == 0, raw
assert raw["fetches"] == 0, "regtest must use only local Core, no public server: {raw}"
print("rawtx cache:", raw)
PY

echo "== graceful shutdown (SIGTERM) =="
elite_stop "$ELITE" "$RUN/serve.log"
grep -q "stopped at" "$RUN/serve.log" || fail "no 'stopped at' line" "$RUN/serve.log"
# The application log tells the whole run by itself (no commands needed).
for line in "started at" "stopped at" "debug: server: banner" "debug: rawtx cache:" \
    "debug: rawtx summary:"; do
  grep -q "$line" "$DATA/e-lite.log" || fail "e-lite.log lacks '$line'" "$DATA/e-lite.log"
done
python3 - "$RUN/serve.log" "$DATA/e-lite.log" <<'PY'
import re, sys
from datetime import datetime
from pathlib import Path

console, saved = (Path(path).read_text().splitlines() for path in sys.argv[1:])
debug = [line for line in console if "[e-lite] debug:" in line]
assert debug
for line in debug:
    at, _ = line.split(" [e-lite] debug:", 1)
    assert datetime.strptime(at, "%Y-%m-%dT%H:%M:%SZ"), line
    assert line in saved, line
lookup = next(line for line in debug if "index_miss mempool_view_miss core_rpc_code=-5" in line)
conn = re.search(r"conn=(\d+)", lookup).group(1)
assert int(conn) > 0, lookup
assert any(f"wallet: conn={conn} closed after " in line for line in debug), conn
assert "12" * 32 not in "\n".join(debug)
print("UTC debug timestamps match console and e-lite.log; Core -5 and connection ID verified")
print(lookup)
PY
grep -qE "READY|FULLY SYNCED" "$DATA/e-lite.log" || fail "e-lite.log lacks READY" "$DATA/e-lite.log"
tail -n 5 "$RUN/serve.log" | grep -qE "Connection refused|mempool refresh" \
  && fail "unexpected shutdown error" "$RUN/serve.log"
"$BIN" status --network regtest --data-dir "$DATA" --metrics "127.0.0.1:$METRICS_PORT" --json \
  | grep -q '"state": "STOPPED"'

echo "== rawtx cache status/clear =="
"$BIN" cache status --network regtest --data-dir "$DATA" --json > "$RUN/cache-before.json"
python3 - "$RUN/cache-before.json" <<'PY'
import json, sys
st = json.load(open(sys.argv[1]))
assert st["network"] == "regtest", st
assert st["total"]["files"] >= 1, st
assert st["total"]["bytes"] > 0, st
print("cache status:", st["total"], "at", st["path"])
PY
# Without --yes nothing is deleted.
"$BIN" cache clear --network regtest --data-dir "$DATA" > "$RUN/cache-dry.txt"
grep -q "nothing was deleted" "$RUN/cache-dry.txt"
cache_status=$("$BIN" cache status --network regtest --data-dir "$DATA" --json)
grep -qE '"files": [1-9]' <<<"$cache_status" || fail "the rawtx cache is empty"
# With --yes the cache goes and the index stays.
"$BIN" cache clear --network regtest --data-dir "$DATA" --yes --json > "$RUN/cache-clear.json"
python3 - "$RUN/cache-clear.json" <<'PY'
import json, sys
st = json.load(open(sys.argv[1]))
assert st["removed"]["files"] >= 1, st
assert st["remaining"]["files"] == 0, st
print("cache cleared:", st["removed"])
PY
[ -f "$DATA/CURRENT" ] || fail "cache clear removed the index"
[ -d "$CORE/regtest/blocks" ] || fail "cache clear removed the Core datadir"
# Capture first: `| grep -q` closes the pipe at the first match and the
# command would then write to a closed stdout.
second_clear=$("$BIN" cache clear --network regtest --data-dir "$DATA" --yes --json)
grep -q '"files": 0' <<<"$second_clear" || fail "a second cache clear found files"

echo "== snapshot export/import =="
SNAP="$RUN/snapshot.tar.gz"
"$BIN" export-snapshot --network regtest --data-dir "$DATA" --output "$SNAP"
D1=$("$BIN" export-snapshot --network regtest --data-dir "$DATA" | sed -n 's/^logical_snapshot_hash: //p')
"$BIN" import-snapshot --network regtest --data-dir "$RUN/imported" --input "$SNAP"
D2=$("$BIN" export-snapshot --network regtest --data-dir "$RUN/imported" | sed -n 's/^logical_snapshot_hash: //p')
{ [ -n "$D1" ] && [ "$D1" = "$D2" ]; } || fail "snapshot digest mismatch: $D1 vs $D2"
echo "snapshot digest: $D1"

echo "e2e regtest OK (height $HEIGHT)"
