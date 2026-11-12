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

LAN_IP=$(hostname -I 2>/dev/null | awk '{print $1}')
LEAF="$DATA/tls/self-signed.crt"
CA="$DATA/tls/ca.crt"
python3 - "$PLAIN" "$TLS" "$CA" "$METRICS" "$LAN_IP" <<'PY'
import json, socket, ssl, sys, urllib.request
plain, tls, cert, metrics = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3], int(sys.argv[4])
lan_ip = sys.argv[5]

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
# AUD-147: cleartext is not offered on the network unless --plain-lan / listen.
if lan_ip and not lan_ip.startswith("127."):
    try:
        socket.create_connection((lan_ip, plain), timeout=2).close()
    except OSError:
        pass
    else:
        raise SystemExit(f"plain listener reachable on {lan_ip} by default")
    socket.create_connection((lan_ip, tls), timeout=2).close()
st = json.load(urllib.request.urlopen(f"http://127.0.0.1:{metrics}/status"))
assert st["tls_listen"] == f"0.0.0.0:{tls}", st.get("tls_listen")
assert st["listen"] == f"127.0.0.1:{plain}", st.get("listen")  # plain is loopback-only by default
assert st["tls_handshakes"] >= 1, st.get("tls_handshakes")

def verified(host, server_hostname):
    """Full wallet-style validation: trust the generated cert as CA and check
    the name the client asked for."""
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.load_verify_locations(cafile=cert)
    ctx.check_hostname = True
    ctx.verify_mode = ssl.CERT_REQUIRED
    s = ctx.wrap_socket(socket.create_connection((host, tls), timeout=5),
                        server_hostname=server_hostname)
    f = s.makefile("rwb")
    f.write(json.dumps({"id": 1, "method": "server.version",
                        "params": ["verified", "1.4"]}).encode() + b"\n")
    f.flush()
    reply = json.loads(f.readline())
    s.close()
    return negotiated(reply)

assert verified("127.0.0.1", "localhost") == "1.4"
if lan_ip:
    assert verified(lan_ip, lan_ip) == "1.4"
try:
    verified("127.0.0.1", "wrong.example")
except ssl.SSLCertVerificationError:
    pass
else:
    raise AssertionError("the certificate validated a hostname it does not cover")
print("verified TLS: localhost +", lan_ip or "no LAN IP", "+ wrong host rejected")
print("plain", st["listen"], "tls", st["tls_listen"], "state", st["state"])
PY

# The chain must validate against the local CA and the CA must be a real CA
# (Java/Sparrow rejects a non-CA certificate used as a TrustAnchor).
CA_TEXT=$(openssl x509 -in "$CA" -noout -text)
echo "$CA_TEXT" | grep -q "CA:TRUE" || {
  echo "local CA lacks BasicConstraints CA:TRUE" >&2
  exit 1
}
openssl verify -CAfile "$CA" "$LEAF" >/dev/null || {
  echo "server chain does not verify against the local CA" >&2
  openssl verify -CAfile "$CA" "$LEAF" >&2
  exit 1
}
TEXT=$(openssl x509 -in "$LEAF" -noout -text)
echo "$TEXT" | grep -q "DNS:localhost" || {
  echo "self-signed certificate lacks the localhost SAN" >&2
  exit 1
}
# A LAN address, when the host has one, must be covered so a wallet connecting
# by IP can validate the certificate.
if [ -n "$LAN_IP" ]; then
  echo "$TEXT" | grep -q "IP Address:$LAN_IP" || {
    echo "self-signed certificate lacks the LAN IP SAN $LAN_IP" >&2
    exit 1
  }
fi
# The logged fingerprint must be the DER fingerprint openssl prints.
FP=$(openssl x509 -in "$LEAF" -noout -fingerprint -sha256 \
  | sed 's/.*=//; s/://g' | tr 'A-F' 'a-f')
grep -qi "sha256 $FP" "$RUN/serve.log" || {
  echo "logged fingerprint does not match openssl ($FP)" >&2
  grep "TLS certificate" "$RUN/serve.log" >&2
  exit 1
}

kill -TERM "$(cat "$RUN/serve.pid")"
wait "$(cat "$RUN/serve.pid")" || true
grep -q "stopped at" "$RUN/serve.log"
echo "defaults OK"
