#!/usr/bin/env bash
# End-to-end regtest check: index, serve, query with an Electrum client.
# Self-cleaning: stops its own bitcoind/e-lite and deletes every temporary file.
set -euo pipefail

cd "$(dirname "$0")/.."
BIN="${E_LITE_BIN:-$PWD/target/debug/e-lite}"
[ -x "$BIN" ] || { echo "build first: cargo build -p e-lite" >&2; exit 1; }
command -v bitcoind >/dev/null || { echo "bitcoind not found" >&2; exit 1; }

RUN="$(mktemp -d "${TMPDIR:-/tmp}/e-lite-e2e-XXXXXX")"
CORE="$RUN/core"; DATA="$RUN/index"; TLS_DIR="$RUN/tls"
RPC_PORT="${E_LITE_E2E_RPC_PORT:-18443}"
ELECTRUM_PORT="${E_LITE_E2E_ELECTRUM_PORT:-15001}"
TLS_PORT="${E_LITE_E2E_TLS_PORT:-15002}"
METRICS_PORT="${E_LITE_E2E_METRICS_PORT:-19184}"

cleanup() {
  set +e
  [ -f "$CORE/regtest/bitcoind.pid" ] && kill "$(cat "$CORE/regtest/bitcoind.pid")" 2>/dev/null
  [ -f "$RUN/serve.pid" ] && kill "$(cat "$RUN/serve.pid")" 2>/dev/null
  sleep 1
  rm -rf "$RUN"
}
trap cleanup EXIT INT TERM

mkdir -p "$CORE" "$DATA" "$TLS_DIR"
cp crates/el-serve/tests/fixtures/tls-cert.pem "$TLS_DIR/cert.pem"
cp crates/el-serve/tests/fixtures/tls-key.pem "$TLS_DIR/key.pem"
cp crates/el-serve/tests/fixtures/tls-ca.pem "$TLS_DIR/ca.pem"
bitcoind -regtest -datadir="$CORE" -daemon -server -listen=0 \
  -rpcport="$RPC_PORT" -fallbackfee=0.0002 >/dev/null
CLI="bitcoin-cli -regtest -datadir=$CORE -rpcport=$RPC_PORT"
for _ in $(seq 1 100); do $CLI getblockcount >/dev/null 2>&1 && break; sleep 0.2; done
$CLI createwallet e2e >/dev/null
ADDR=$($CLI -rpcwallet=e2e getnewaddress)
$CLI -rpcwallet=e2e generatetoaddress 101 "$ADDR" >/dev/null
HEIGHT=$($CLI getblockcount)

COMMON=(--network regtest --core-datadir "$CORE" --core-rpc "http://127.0.0.1:$RPC_PORT" --data-dir "$DATA" --min-free-gib 1 --public-servers "$RUN/public-servers.toml")
if "$BIN" doctor "${COMMON[@]}" --core-rpc "http://192.0.2.1:8332" \
  --set core.require_loopback_rpc=true >/dev/null 2>&1; then
  echo "strict Core RPC check did not reject a non-loopback endpoint" >&2
  exit 1
fi
"$BIN" init "${COMMON[@]}"
"$BIN" doctor "${COMMON[@]}" --listen "127.0.0.1:$ELECTRUM_PORT" --metrics "127.0.0.1:$METRICS_PORT" \
  --tls-listen "127.0.0.1:$TLS_PORT" --tls-cert "$TLS_DIR/cert.pem" --tls-key "$TLS_DIR/key.pem" \
  >"$RUN/doctor.log" 2>&1
grep -q "public servers:" "$RUN/doctor.log" || { cat "$RUN/doctor.log"; exit 1; }
test -f "$RUN/public-servers.toml" || { echo "public-servers.toml was not created"; exit 1; }
"$BIN" index "${COMMON[@]}" --no-follow --batch 16 --reorg-window 8 --milestone-blocks 50 \
  >"$RUN/index.log" 2>&1
grep -q "milestone" "$RUN/index.log" || { cat "$RUN/index.log"; exit 1; }
grep -q "%" "$RUN/index.log" || { cat "$RUN/index.log"; exit 1; }
test -f "$DATA/e-lite.log" || { echo "missing e-lite.log"; exit 1; }
grep -q "tuning profile" "$DATA/e-lite.log" || { cat "$DATA/e-lite.log"; exit 1; }

cat >"$DATA/banner.txt" <<'EOF'
[indexing]
CUSTOM INDEX {height} {percent} {state}
[ready]
CUSTOM READY {height} {percent} {state}
EOF

"$BIN" run "${COMMON[@]}" --no-follow --reorg-window 8 \
  --listen "127.0.0.1:$ELECTRUM_PORT" --metrics "127.0.0.1:$METRICS_PORT" \
  --tls-listen "127.0.0.1:$TLS_PORT" --tls-cert "$TLS_DIR/cert.pem" --tls-key "$TLS_DIR/key.pem" \
  --banner-file "$DATA/banner.txt" \
  >"$RUN/serve.log" 2>&1 &
echo $! >"$RUN/serve.pid"
# `run` waits up to 30 s for a freshly created managed Tor to bootstrap before
# opening the listeners; on a slow Tor network that exceeded the old 20 s.
for _ in $(seq 1 250); do
  curl -sf "http://127.0.0.1:$METRICS_PORT/healthz" >/dev/null 2>&1 && break
  sleep 0.2
done
grep -q "started at" "$RUN/serve.log" || { tail -40 "$RUN/serve.log"; exit 1; }
if grep -qE "p2p discovery failed|Connection refused|memory pressure" "$RUN/serve.log"; then
  echo "unexpected startup error in serve.log:" >&2
  cat "$RUN/serve.log" >&2
  exit 1
fi

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

SCRIPT_HEX=$($CLI -rpcwallet=e2e getaddressinfo "$ADDR" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["scriptPubKey"])')
SCRIPT_HASH=$(python3 - "$SCRIPT_HEX" <<'PY'
import hashlib, sys
print(hashlib.sha256(bytes.fromhex(sys.argv[1])).digest()[::-1].hex())
PY
)

python3 - "$ELECTRUM_PORT" "$SCRIPT_HASH" "$HEIGHT" <<'PY'
import json, socket, sys
port, sh, height = int(sys.argv[1]), sys.argv[2], int(sys.argv[3])
s = socket.create_connection(("127.0.0.1", port), timeout=10)
f = s.makefile("rwb")
def call(i, m, p):
    f.write((json.dumps({"jsonrpc":"2.0","id":i,"method":m,"params":p})+"\n").encode()); f.flush()
    r = json.loads(f.readline())
    assert "error" not in r, r
    return r["result"]
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
print("electrum queries OK")
PY

echo "== Electrum over TLS =="
python3 - "$TLS_PORT" "$HEIGHT" "$TLS_DIR/ca.pem" <<'PY'
import json, socket, ssl, sys
port, height, ca = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
ctx = ssl.create_default_context(cafile=ca)
with socket.create_connection(("127.0.0.1", port), timeout=10) as raw:
    with ctx.wrap_socket(raw, server_hostname="localhost") as s:
        f = s.makefile("rwb")
        def call(i, m, p):
            f.write((json.dumps({"jsonrpc":"2.0","id":i,"method":m,"params":p})+"\n").encode()); f.flush()
            r = json.loads(f.readline())
            assert "error" not in r, r
            return r["result"]
        assert call(1, "server.version", ["e2e-tls", "1.4"])[1] == "1.4"
        assert call(2, "blockchain.headers.subscribe", [])["height"] == height
        assert call(3, "blockchain.scripthash.get_history", ["00"*32]) == []
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
python3 "$(dirname "$0")/e2e_sparrow.py" "$ELECTRUM_PORT" "$CORE" "$RPC_PORT" "$ADDR"

echo "== Electrum 1.6: headers array, mempool info, fee mode, package ==="
python3 "$(dirname "$0")/e2e_electrum16.py" "$ELECTRUM_PORT" "$CORE" "$RPC_PORT" "$ADDR"

echo "== Electrum 1.7: scriptpubkey, outpoint, testmempoolaccept =="
python3 "$(dirname "$0")/e2e_electrum17.py" "$ELECTRUM_PORT" "$CORE" "$RPC_PORT" "$ADDR"

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
kill -TERM "$(cat "$RUN/serve.pid")"
set +e
wait "$(cat "$RUN/serve.pid")"
RC=$?
set -e
echo "shutdown exit=$RC"
[ "$RC" -eq 0 ] || { cat "$RUN/serve.log"; exit 1; }
grep -q "stopped at" "$RUN/serve.log" || { cat "$RUN/serve.log"; exit 1; }
grep -q "started at" "$DATA/e-lite.log" || { tail -40 "$DATA/e-lite.log"; exit 1; }
grep -q "stopped at" "$DATA/e-lite.log" || { tail -40 "$DATA/e-lite.log"; exit 1; }
grep -qE "READY|FULLY SYNCED" "$DATA/e-lite.log" || { tail -40 "$DATA/e-lite.log"; exit 1; }
if tail -n 5 "$RUN/serve.log" | grep -qE "Connection refused|mempool refresh"; then
  echo "unexpected shutdown error in serve.log:" >&2
  cat "$RUN/serve.log" >&2
  exit 1
fi
"$BIN" status --network regtest --data-dir "$DATA" --metrics "127.0.0.1:$METRICS_PORT" --json \
  | grep -q '"state": "STOPPED"'

echo "== snapshot export/import =="
SNAP="$RUN/snapshot.tar.gz"
"$BIN" export-snapshot --network regtest --data-dir "$DATA" --output "$SNAP"
D1=$("$BIN" export-snapshot --network regtest --data-dir "$DATA" | sed -n 's/^logical_snapshot_hash: //p')
"$BIN" import-snapshot --network regtest --data-dir "$RUN/imported" --input "$SNAP"
D2=$("$BIN" export-snapshot --network regtest --data-dir "$RUN/imported" | sed -n 's/^logical_snapshot_hash: //p')
[ -n "$D1" ] && [ "$D1" = "$D2" ] || { echo "snapshot digest mismatch: $D1 vs $D2" >&2; exit 1; }
echo "snapshot digest: $D1"

echo "e2e regtest OK (height $HEIGHT)"
