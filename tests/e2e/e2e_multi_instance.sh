#!/usr/bin/env bash
# Two instances side by side (default regtest ports for A, explicit ports for
# B): independent listeners, data, certificates and heights; stopping one
# leaves the other serving; the stopped one comes back.
. "$(dirname "$0")/../lib/e2e.sh"
e2e_init multi
A="$RUN/a"; B="$RUN/b"
RPC_A=18490; RPC_B=18491
A_PLAIN=55311; A_TLS=55322; A_METRICS=55384
B_PLAIN=55411; B_TLS=55422; B_METRICS=55484

core_start "$A/core" "$RPC_A"; core_mine_new "$A/core" "$RPC_A" multi 40 >/dev/null
core_start "$B/core" "$RPC_B"; core_mine_new "$B/core" "$RPC_B" multi 80 >/dev/null
# A: no [server]/[metrics] sections, so the regtest defaults apply.
DATA="$A/index" CORE="$A/core" RPC=$RPC_A e2e_conf "$A/e-lite.toml" \
  index.batch_blocks=32 index.reorg_window= pruning.poll_seconds= \
  server.listen= server.tls= metrics.listen= tor.mode='"off"'
DATA="$B/index" CORE="$B/core" RPC=$RPC_B e2e_conf "$B/e-lite.toml" \
  index.batch_blocks=32 index.reorg_window= pruning.poll_seconds= \
  server.listen="\"127.0.0.1:$B_PLAIN\"" server.tls= \
  server.tls_listen="\"127.0.0.1:$B_TLS\"" metrics.listen="\"127.0.0.1:$B_METRICS\"" \
  tor.mode='"off"'

elite_start "$RUN/a.log" run --config "$A/e-lite.toml"; PID_A=$ELITE
elite_start "$RUN/b.log" run --config "$B/e-lite.toml"; PID_B=$ELITE
wait_ready "$A_METRICS" "$RUN/a.log"
wait_ready "$B_METRICS" "$RUN/b.log"

expect_height() {
  local got
  got=$(electrum height "${@:3}" "$1")
  [ "$got" = "$2" ] || fail "port $1 serves height $got, expected $2"
}
expect_height "$A_PLAIN" 40; expect_height "$A_TLS" 40 --tls
expect_height "$B_PLAIN" 80; expect_height "$B_TLS" 80 --tls
[ "$(status "$A_METRICS" listen)" = "127.0.0.1:$A_PLAIN" ] || fail "A plain is not the loopback default"
[ "$(status "$A_METRICS" tls_listen)" = "0.0.0.0:$A_TLS" ] || fail "A TLS is not the default"
[ "$(status "$B_METRICS" listen)" = "127.0.0.1:$B_PLAIN" ] || fail "B plain listen"
[ "$(status "$B_METRICS" tls_listen)" = "127.0.0.1:$B_TLS" ] || fail "B TLS listen"
echo "A plain/tls 40, B plain/tls 80; listeners independent"

# Separate data dirs must not share a TLS identity.
FP_A=$(openssl x509 -in "$A/index/tls/self-signed.crt" -noout -fingerprint -sha256)
FP_B=$(openssl x509 -in "$B/index/tls/self-signed.crt" -noout -fingerprint -sha256)
[ "$FP_A" != "$FP_B" ] || fail "instances share a TLS certificate"
echo "TLS certificates are distinct"

# Memory is in the nodes (children of the watching parents).
rss_of() {
  local node
  node="$(pgrep -P "$1" -x e-lite | head -1 || true)"
  awk '/VmRSS/ {print $2}' "/proc/${node:-$1}/status"
}
python3 - "$(rss_of "$PID_A")" "$(rss_of "$PID_B")" "$(awk '/MemTotal/ {print $2}' /proc/meminfo)" <<'PY'
import sys
a, b, total = map(int, sys.argv[1:])
assert a < total * 0.75 and b < total * 0.75 and a + b < total * 0.9, (a, b, total)
print(f"RSS A={a//1024} MiB B={b//1024} MiB (total {total//1024} MiB)")
PY

echo "== stop A, B keeps serving =="
elite_stop "$PID_A" "$RUN/a.log"
expect_height "$B_PLAIN" 80
curl -sf "http://127.0.0.1:$A_METRICS/status" >/dev/null 2>&1 && fail "A metrics still answering after shutdown"
echo "B still serves height 80 while A is stopped"

echo "== restart A =="
elite_start "$RUN/a2.log" run --config "$A/e-lite.toml"
wait_ready "$A_METRICS" "$RUN/a2.log"
expect_height "$A_PLAIN" 40; expect_height "$B_PLAIN" 80
echo "both instances serve after A restarted"
echo "multi-instance OK"
