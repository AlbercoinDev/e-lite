#!/usr/bin/env bash
# Two instances running at once on different Core datadirs and ports:
# listeners, TLS, metrics, heights and independent shutdown/restart.
set -euo pipefail

cd "$(dirname "$0")/.."
BIN="${E_LITE_BIN:-$PWD/target/debug/e-lite}"
[ -x "$BIN" ] || { echo "build first: cargo build -p e-lite" >&2; exit 1; }
command -v bitcoind >/dev/null || { echo "bitcoind not found" >&2; exit 1; }

RUN="$(mktemp -d "${TMPDIR:-/tmp}/e-lite-multi-XXXXXX")"
A="$RUN/a"; B="$RUN/b"
RPC_A=18490; RPC_B=18491
A_PLAIN=55311; A_TLS=55322; A_METRICS=55384
B_PLAIN=55411; B_TLS=55422; B_METRICS=55484

cleanup() {
  set +e
  for f in "$RUN/a.pid" "$RUN/b.pid"; do
    [ -f "$f" ] && kill -TERM "$(cat "$f")" 2>/dev/null
  done
  for d in "$A/core" "$B/core"; do
    [ -f "$d/regtest/bitcoind.pid" ] && kill "$(cat "$d/regtest/bitcoind.pid")" 2>/dev/null
  done
  sleep 2
  rm -rf "$RUN"
}
trap cleanup EXIT INT TERM

start_core() {
  local dir=$1 rpc=$2 blocks=$3
  mkdir -p "$dir"
  bitcoind -regtest -datadir="$dir" -daemon -server -listen=0 \
    -rpcport="$rpc" -fallbackfee=0.0002 >/dev/null
  local cli="bitcoin-cli -regtest -datadir=$dir -rpcport=$rpc"
  for _ in $(seq 1 100); do $cli getblockcount >/dev/null 2>&1 && break; sleep 0.2; done
  $cli createwallet multi >/dev/null
  local addr
  addr=$($cli -rpcwallet=multi getnewaddress)
  $cli -rpcwallet=multi generatetoaddress "$blocks" "$addr" >/dev/null
  $cli getblockcount
}

mkdir -p "$A/index" "$B/index"
cat >"$A/e-lite.toml" <<EOF
[network]
name = "regtest"
data_dir = "$A/index"

[core]
manage = "external"
core_datadir = "$A/core"
rpc_url = "http://127.0.0.1:$RPC_A"

[index]
batch_blocks = 32

[pruning]
min_free_gib = 1

[tor]
mode = "off"

[rawtx]
enabled = false
EOF

cat >"$B/e-lite.toml" <<EOF
[network]
name = "regtest"
data_dir = "$B/index"

[core]
manage = "external"
core_datadir = "$B/core"
rpc_url = "http://127.0.0.1:$RPC_B"

[index]
batch_blocks = 32

[pruning]
min_free_gib = 1

[server]
listen = "127.0.0.1:$B_PLAIN"
tls_listen = "127.0.0.1:$B_TLS"

[metrics]
listen = "127.0.0.1:$B_METRICS"

[tor]
mode = "off"

[rawtx]
enabled = false
EOF

HEIGHT_A=$(start_core "$A/core" "$RPC_A" 40)
HEIGHT_B=$(start_core "$B/core" "$RPC_B" 80)
echo "chains: A=$HEIGHT_A B=$HEIGHT_B"

"$BIN" run --config "$A/e-lite.toml" >"$RUN/a.log" 2>&1 &
echo $! >"$RUN/a.pid"
"$BIN" run --config "$B/e-lite.toml" >"$RUN/b.log" 2>&1 &
echo $! >"$RUN/b.pid"

wait_ready() {
  local metrics=$1
  for _ in $(seq 1 120); do
    [ "$(curl -s "http://127.0.0.1:$metrics/status" \
      | python3 -c 'import json,sys;print(json.load(sys.stdin).get("state",""))' 2>/dev/null)" = "READY" ] && return 0
    sleep 0.5
  done
  return 1
}
wait_ready "$A_METRICS" || { echo "A did not become READY" >&2; tail -30 "$RUN/a.log"; exit 1; }
wait_ready "$B_METRICS" || { echo "B did not become READY" >&2; tail -30 "$RUN/b.log"; exit 1; }

python3 - "$A_PLAIN" "$A_TLS" "$B_PLAIN" "$B_TLS" "$A_METRICS" "$B_METRICS" <<'PY'
import json, socket, ssl, sys, urllib.request
a_plain, a_tls, b_plain, b_tls, a_metrics, b_metrics = map(int, sys.argv[1:])

def height(port, use_tls):
    s = socket.create_connection(("127.0.0.1", port), timeout=10)
    if use_tls:
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        s = ctx.wrap_socket(s, server_hostname="localhost")
    f = s.makefile("rwb")
    f.write(json.dumps({"id": 1, "method": "blockchain.headers.subscribe",
                        "params": []}).encode() + b"\n")
    f.flush()
    h = json.loads(f.readline())["result"]["height"]
    s.close()
    return h

assert height(a_plain, False) == 40
assert height(a_tls, True) == 40
assert height(b_plain, False) == 80
assert height(b_tls, True) == 80
sa = json.load(urllib.request.urlopen(f"http://127.0.0.1:{a_metrics}/status"))
sb = json.load(urllib.request.urlopen(f"http://127.0.0.1:{b_metrics}/status"))
assert sa["indexed_height"] == 40 and sb["indexed_height"] == 80
assert sa["listen"] == "0.0.0.0:55311", sa["listen"]
assert sb["listen"] == "127.0.0.1:55411", sb["listen"]
assert sa["tls_listen"] == "0.0.0.0:55322", sa["tls_listen"]
assert sb["tls_listen"] == "127.0.0.1:55422", sb["tls_listen"]
print("A plain/tls 40, B plain/tls 80; listeners independent")
PY

# Distinct certificates: separate data dirs must not share identity.
FP_A=$(openssl x509 -in "$A/index/tls/self-signed.crt" -noout -fingerprint -sha256)
FP_B=$(openssl x509 -in "$B/index/tls/self-signed.crt" -noout -fingerprint -sha256)
[ "$FP_A" != "$FP_B" ] || { echo "instances share a TLS certificate" >&2; exit 1; }
echo "TLS certificates are distinct"

# Combined memory must stay under the process budget.
RSS_A=$(awk '/VmRSS/ {print $2}' "/proc/$(cat "$RUN/a.pid")/status")
RSS_B=$(awk '/VmRSS/ {print $2}' "/proc/$(cat "$RUN/b.pid")/status")
TOTAL_KB=$(awk '/MemTotal/ {print $2}' /proc/meminfo)
python3 - "$RSS_A" "$RSS_B" "$TOTAL_KB" <<'PY'
import sys
a, b, total = map(int, sys.argv[1:])
ceiling = total * 0.75
assert a < ceiling and b < ceiling, (a, b, ceiling)
assert a + b < total * 0.9, (a, b, total)
print(f"RSS A={a//1024} MiB B={b//1024} MiB combined={((a+b)//1024)} MiB (total {total//1024} MiB)")
PY

echo "== stop A, B must keep serving =="
kill -TERM "$(cat "$RUN/a.pid")"
for _ in $(seq 1 60); do kill -0 "$(cat "$RUN/a.pid")" 2>/dev/null || break; sleep 0.5; done
python3 - "$B_PLAIN" <<'PY'
import json, socket, sys
s = socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=10)
f = s.makefile("rwb")
f.write(json.dumps({"id": 1, "method": "blockchain.headers.subscribe",
                    "params": []}).encode() + b"\n")
f.flush()
assert json.loads(f.readline())["result"]["height"] == 80
print("B still serves height 80 while A is stopped")
PY
curl -sf "http://127.0.0.1:$A_METRICS/status" >/dev/null 2>&1 && {
  echo "A metrics still answering after shutdown" >&2; exit 1;
}

echo "== restart A =="
"$BIN" run --config "$A/e-lite.toml" >"$RUN/a2.log" 2>&1 &
echo $! >"$RUN/a.pid"
wait_ready "$A_METRICS" || { echo "A did not restart" >&2; tail -30 "$RUN/a2.log"; exit 1; }
python3 - "$A_PLAIN" "$B_PLAIN" <<'PY'
import json, socket, sys
for port, expected in ((int(sys.argv[1]), 40), (int(sys.argv[2]), 80)):
    s = socket.create_connection(("127.0.0.1", port), timeout=10)
    f = s.makefile("rwb")
    f.write(json.dumps({"id": 1, "method": "blockchain.headers.subscribe",
                        "params": []}).encode() + b"\n")
    f.flush()
    assert json.loads(f.readline())["result"]["height"] == expected
print("both instances serve after A restarted")
PY

echo "multi-instance OK"
