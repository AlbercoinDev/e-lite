#!/usr/bin/env bash
# Defaults for a network: plain/TLS/metrics ports, an automatically generated
# self-signed certificate and TLS serving while Tor is off.
set -euo pipefail

cd "$(dirname "$0")/.."
BIN="${E_LITE_BIN:-$PWD/target/debug/e-lite}"
[ -x "$BIN" ] || { echo "build first: cargo build -p e-lite" >&2; exit 1; }
command -v bitcoind >/dev/null || { echo "bitcoind not found" >&2; exit 1; }

RUN="$(mktemp -d "${TMPDIR:-/tmp}/e-lite-defaults-XXXXXX")"
CORE="$RUN/core"; DATA="$RUN/index"; CONF="$RUN/e-lite.toml"
RPC=18487; PLAIN=55311; TLS=55322; METRICS=55384

cleanup() {
  set +e
  [ -f "$RUN/serve.pid" ] && kill "$(cat "$RUN/serve.pid")" 2>/dev/null
  [ -f "$CORE/regtest/bitcoind.pid" ] && kill "$(cat "$CORE/regtest/bitcoind.pid")" 2>/dev/null
  pkill -f "bitcoind .*$CORE" 2>/dev/null
  sleep 1
  rm -rf "$RUN"
}
trap cleanup EXIT INT TERM

for port in "$PLAIN" "$TLS" "$METRICS"; do
  if (echo >"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
    echo "port $port is already in use; stop the other instance first" >&2
    exit 1
  fi
done

mkdir -p "$CORE" "$DATA"
bitcoind -regtest -datadir="$CORE" -daemon -server -listen=0 \
  -rpcport="$RPC" -fallbackfee=0.0002 >/dev/null
CLI="bitcoin-cli -regtest -datadir=$CORE -rpcport=$RPC"
for _ in $(seq 1 100); do $CLI getblockcount >/dev/null 2>&1 && break; sleep 0.2; done
$CLI createwallet default >/dev/null
ADDR=$($CLI -rpcwallet=default getnewaddress)
$CLI -rpcwallet=default generatetoaddress 101 "$ADDR" >/dev/null

# Deliberately no [server], [metrics] or TLS sections: the per-network
# defaults must fill them in.
cat >"$CONF" <<EOF
[network]
name = "regtest"
data_dir = "$DATA"

[core]
manage = "external"
core_datadir = "$CORE"
rpc_url = "http://127.0.0.1:$RPC"

[index]
batch_blocks = 16
reorg_window = 8

[pruning]
min_free_gib = 1
poll_seconds = 1

[tor]
mode = "off"

[rawtx]
enabled = false
EOF

"$BIN" run --config "$CONF" >"$RUN/serve.log" 2>&1 &
echo $! >"$RUN/serve.pid"
for _ in $(seq 1 120); do
  [ "$(curl -s "http://127.0.0.1:$METRICS/status" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("state",""))' 2>/dev/null)" = "READY" ] && break
  sleep 0.5
done
[ -f "$DATA/tls/self-signed.crt" ] || { echo "certificate not generated" >&2; tail -40 "$RUN/serve.log"; exit 1; }

python3 - "$PLAIN" "$TLS" "$DATA/tls/self-signed.crt" "$METRICS" <<'PY'
import json, socket, ssl, sys, urllib.request
plain, tls, cert, metrics = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3], int(sys.argv[4])

def call(port, use_tls):
    s = socket.create_connection(("127.0.0.1", port), timeout=5)
    if use_tls:
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        s = ctx.wrap_socket(s, server_hostname="localhost")
    f = s.makefile("rwb")
    f.write(json.dumps({"id": 1, "method": "server.version",
                        "params": ["e2e-defaults", "1.4"]}).encode() + b"\n")
    f.flush()
    reply = json.loads(f.readline())
    s.close()
    return reply

def negotiated(reply):
    result = reply["result"]
    return result[1] if isinstance(result, list) else result

assert negotiated(call(plain, False)) == "1.4"
assert negotiated(call(tls, True)) == "1.4"
st = json.load(urllib.request.urlopen(f"http://127.0.0.1:{metrics}/status"))
assert st["tls_listen"] == f"0.0.0.0:{tls}", st.get("tls_listen")
assert st["listen"] == f"0.0.0.0:{plain}", st.get("listen")
assert st["tls_handshakes"] >= 1, st.get("tls_handshakes")
print("plain", st["listen"], "tls", st["tls_listen"], "state", st["state"])
PY

openssl x509 -in "$DATA/tls/self-signed.crt" -noout -text | grep -q "DNS:localhost" || {
  echo "self-signed certificate lacks the localhost SAN" >&2
  exit 1
}
# Operator PEMs must not be replaced by the auto path.
grep -q "TLS certificate:" "$RUN/serve.log" || { echo "missing TLS log line" >&2; exit 1; }

kill -TERM "$(cat "$RUN/serve.pid")"
wait "$(cat "$RUN/serve.pid")" || true
grep -q "stopped at" "$RUN/serve.log"
echo "defaults OK"
