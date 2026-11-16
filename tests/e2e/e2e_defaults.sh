#!/usr/bin/env bash
# Defaults for a network: plain/TLS/metrics ports, an automatically generated
# local CA + leaf certificate, and TLS serving while Tor is off.
. "$(dirname "$0")/../lib/e2e.sh"
e2e_init defaults
CORE="$RUN/core"; DATA="$RUN/index"; CONF="$RUN/e-lite.toml"
RPC=18487; PLAIN=55311; TLS=55322; METRICS=55384

for port in "$PLAIN" "$TLS" "$METRICS"; do
  if (echo >"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
    fail "port $port is already in use; stop the other instance first"
  fi
done

core_start "$CORE" "$RPC"
core_mine_new "$CORE" "$RPC" default 101 >/dev/null
# Deliberately no [server] or [metrics] section: the per-network defaults
# must fill them in.
e2e_conf "$CONF" server.listen= server.tls= metrics.listen= tor.mode='"off"'

elite_start "$RUN/serve.log" run --config "$CONF"
wait_ready "$METRICS" "$RUN/serve.log"
[ -f "$DATA/tls/self-signed.crt" ] || fail "certificate not generated" "$RUN/serve.log"

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
# Transport of the external path (rawtx.use_tor defaults to true).
assert st["rawtx"]["path"] == "tor", st["rawtx"].get("path")

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
openssl x509 -in "$CA" -noout -text | grep -q "CA:TRUE" \
  || fail "local CA lacks BasicConstraints CA:TRUE"
openssl verify -CAfile "$CA" "$LEAF" >/dev/null || fail "server chain does not verify against the local CA"
TEXT=$(openssl x509 -in "$LEAF" -noout -text)
echo "$TEXT" | grep -q "DNS:localhost" || fail "certificate lacks the localhost SAN"
# A LAN address, when the host has one, must be covered so a wallet connecting
# by IP can validate the certificate.
if [ -n "$LAN_IP" ]; then
  echo "$TEXT" | grep -q "IP Address:$LAN_IP" || fail "certificate lacks the LAN IP SAN $LAN_IP"
fi
# The logged fingerprint must be the DER fingerprint openssl prints.
FP=$(openssl x509 -in "$LEAF" -noout -fingerprint -sha256 \
  | sed 's/.*=//; s/://g' | tr 'A-F' 'a-f')
grep -qi "sha256 $FP" "$RUN/serve.log" || fail "logged fingerprint does not match openssl ($FP)" "$RUN/serve.log"

elite_stop "$ELITE" "$RUN/serve.log"
grep -q "stopped at" "$RUN/serve.log" || fail "no 'stopped at' line" "$RUN/serve.log"
echo "defaults OK"
