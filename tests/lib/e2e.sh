# Shared helpers for tests/e2e/*.sh. Source it first:
#
#   . "$(dirname "$0")/../lib/e2e.sh"
#   e2e_init <name>
#
# Everything a suite starts through these helpers is stopped by the EXIT
# trap, and the trap waits for each process to really end: the next suite
# reuses the same ports.
# shellcheck shell=bash
set -euo pipefail

E2E_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$E2E_ROOT"
BIN="${E_LITE_BIN:-$E2E_ROOT/target/debug/e-lite}"
E2E_PIDS=()      # e-lite processes to stop
E2E_CORES=()     # "datadir rpcport" of every bitcoind to stop
E2E_STEPS=()     # extra cleanup commands

# Fail with a message, and the tail of a log when one is given.
fail() {
  echo "FAIL: $1" >&2
  [ -n "${2:-}" ] && [ -f "$2" ] && { echo "--- tail $2" >&2; tail -40 "$2" >&2; }
  exit 1
}

# Create the run directory and install the cleanup trap.
e2e_init() {
  [ -x "$BIN" ] || fail "build first: cargo build -p e-lite (or set E_LITE_BIN)"
  command -v bitcoind >/dev/null || fail "bitcoind not found in PATH"
  RUN="$(mktemp -d "${TMPDIR:-/tmp}/e-lite-$1-XXXXXX")"
  trap e2e_cleanup EXIT INT TERM
}

# Run a command at cleanup (before the run directory is removed).
e2e_on_exit() { E2E_STEPS+=("$1"); }

e2e_cleanup() {
  set +e
  local step pid core
  for step in "${E2E_STEPS[@]+"${E2E_STEPS[@]}"}"; do eval "$step"; done
  for pid in "${E2E_PIDS[@]+"${E2E_PIDS[@]}"}"; do kill -TERM "$pid" 2>/dev/null; done
  for core in "${E2E_CORES[@]+"${E2E_CORES[@]}"}"; do
    # shellcheck disable=SC2086
    set -- $core
    bitcoin-cli -regtest -datadir="$1" -rpcport="$2" stop >/dev/null 2>&1
  done
  for _ in $(seq 1 60); do
    local alive=0
    for pid in "${E2E_PIDS[@]+"${E2E_PIDS[@]}"}"; do kill -0 "$pid" 2>/dev/null && alive=1; done
    [ "$alive" -eq 0 ] && break
    sleep 0.5
  done
  for pid in "${E2E_PIDS[@]+"${E2E_PIDS[@]}"}"; do kill -KILL "$pid" 2>/dev/null; done
  for core in "${E2E_CORES[@]+"${E2E_CORES[@]}"}"; do
    local dir="${core%% *}"
    for _ in $(seq 1 40); do pgrep -f "bitcoind .*-datadir=$dir( |$)" >/dev/null || break; sleep 0.5; done
    pkill -KILL -f "bitcoind .*-datadir=$dir( |$)" 2>/dev/null
  done
  pkill -f "tor -f $RUN/" 2>/dev/null
  rm -rf "$RUN"
}

# Poll `cmd...` every 0.5 s until it succeeds or `seconds` pass.
wait_for() {
  local seconds=$1; shift
  for _ in $(seq 1 $((seconds * 2))); do
    "$@" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  return 1
}

# ---------------------------------------------------------------- Core

# `cli <datadir> <rpcport> args...`: bitcoin-cli for a regtest node.
cli() {
  local dir=$1 rpc=$2; shift 2
  # -rpcconnect: always the JSON-RPC port, never the IPC socket newer
  # bitcoin-cli versions try first (which fails where unix sockets are denied).
  bitcoin-cli -regtest -datadir="$dir" -rpcconnect=127.0.0.1 -rpcport="$rpc" "$@"
}

# Start a regtest bitcoind and wait for its RPC. Extra args are passed on.
core_start() {
  local dir=$1 rpc=$2; shift 2
  mkdir -p "$dir"
  bitcoind -regtest -datadir="$dir" -daemon -server -listen=0 \
    -rpcport="$rpc" -fallbackfee=0.0002 "$@" >/dev/null
  core_track "$dir" "$rpc"
  wait_for 30 cli "$dir" "$rpc" getblockcount || fail "bitcoind did not answer RPC on $rpc"
}

# Stop it at cleanup even if e-lite started it (managed Core).
core_track() { E2E_CORES+=("$1 $2"); }

core_stop() {
  local dir=$1 rpc=$2
  cli "$dir" "$rpc" stop >/dev/null 2>&1 || true
  wait_for 30 core_down "$dir" "$rpc" || fail "bitcoind on $rpc did not stop"
}
core_down() { ! cli "$1" "$2" getblockcount >/dev/null 2>&1; }

# Create (or load) a wallet, mine `blocks` to a new address, print the address.
core_mine_new() {
  local dir=$1 rpc=$2 wallet=$3 blocks=$4 addr
  cli "$dir" "$rpc" createwallet "$wallet" >/dev/null 2>&1 \
    || cli "$dir" "$rpc" loadwallet "$wallet" >/dev/null 2>&1 || true
  addr=$(cli "$dir" "$rpc" -rpcwallet="$wallet" getnewaddress)
  [ "$blocks" -gt 0 ] && cli "$dir" "$rpc" -rpcwallet="$wallet" generatetoaddress "$blocks" "$addr" >/dev/null
  echo "$addr"
}

core_mine() {
  local dir=$1 rpc=$2 wallet=$3 blocks=$4 addr=$5
  cli "$dir" "$rpc" -rpcwallet="$wallet" generatetoaddress "$blocks" "$addr" >/dev/null
}

# Electrum script hash of a wallet address.
scripthash_of() {
  local dir=$1 rpc=$2 wallet=$3 addr=$4
  cli "$dir" "$rpc" -rpcwallet="$wallet" getaddressinfo "$addr" \
    | python3 -c 'import hashlib, json, sys
s = bytes.fromhex(json.load(sys.stdin)["scriptPubKey"])
print(hashlib.sha256(s).digest()[::-1].hex())'
}

# --------------------------------------------------------------- e-lite

# Write a regtest config. Defaults: external Core at $CORE/$RPC, data in
# $DATA, plain listener on $ELECTRUM, metrics on $METRICS, no TLS, no rawtx.
# Arguments override or add `section.key=<toml value>`; an empty value
# removes the key. Example: e2e_conf "$CONF" core.manage='"managed"'
e2e_conf() {
  local path=$1; shift
  python3 - "$path" "${DATA:-}" "${CORE:-}" "${RPC:-0}" "${ELECTRUM:-0}" "${METRICS:-0}" "$@" <<'PY'
import sys
path, data, core, rpc, electrum, metrics, *overrides = sys.argv[1:]
q = lambda s: '"' + s + '"'
conf = {
    "network": {"name": q("regtest"), "data_dir": q(data)},
    "core": {"manage": q("external"), "core_datadir": q(core),
             "rpc_url": q(f"http://127.0.0.1:{rpc}")},
    "index": {"batch_blocks": "16", "reorg_window": "8"},
    "pruning": {"min_free_gib": "1", "poll_seconds": "1"},
    "server": {"listen": q(f"127.0.0.1:{electrum}"), "tls": "false"},
    "metrics": {"listen": q(f"127.0.0.1:{metrics}")},
    "rawtx": {"enabled": "false"},
}
if not core:
    del conf["core"]["core_datadir"]
for item in overrides:
    key, _, value = item.partition("=")
    section, _, name = key.partition(".")
    if value == "":
        conf.get(section, {}).pop(name, None)
    else:
        conf.setdefault(section, {})[name] = value
with open(path, "w") as fh:
    for section, values in conf.items():
        if values:
            fh.write(f"[{section}]\n")
            fh.writelines(f"{k} = {v}\n" for k, v in values.items())
            fh.write("\n")
PY
}

# Start `e-lite args...` in the background, logging to `log`; the pid is in
# $ELITE (the watching parent when the restart supervisor is on).
elite_start() {
  local log=$1; shift
  "$BIN" "$@" >"$log" 2>&1 &
  ELITE=$!
  E2E_PIDS+=("$ELITE")
}

# SIGTERM `pid` and require a clean exit.
elite_stop() {
  local pid=$1 log=${2:-} rc
  kill -TERM "$pid"
  set +e
  wait "$pid"
  rc=$?
  set -e
  [ "$rc" -eq 0 ] || fail "e-lite exited with $rc on SIGTERM" "$log"
}

# One field of /status (dotted path); "DOWN" when it does not answer.
status() {
  local metrics=$1 field=$2
  curl -s --max-time 5 "http://127.0.0.1:$metrics/status" 2>/dev/null | python3 -c '
import json, sys
try:
    v = json.load(sys.stdin)
    for part in sys.argv[1].split("."):
        v = v[part]
    print(json.dumps(v) if isinstance(v, (dict, list, bool)) else v)
except Exception:
    print("DOWN")' "$field" 2>/dev/null || echo DOWN
}

is_state() { [ "$(status "$1" state)" = "$2" ]; }
height_at_least() {
  local h
  h=$(status "$1" indexed_height)
  [ "$h" != DOWN ] && [ "$h" -ge "$2" ]
}

# Wait for READY (and, optionally, an indexed height) or fail with the log.
wait_ready() {
  local metrics=$1 log=$2 height=${3:-0} seconds=${4:-90}
  wait_for "$seconds" is_state "$metrics" READY \
    || fail "not READY on $metrics (state $(status "$metrics" state))" "$log"
  [ "$height" -eq 0 ] || wait_for "$seconds" height_at_least "$metrics" "$height" \
    || fail "indexed height $(status "$metrics" indexed_height) < $height" "$log"
}

# Electrum client (tests/lib/e2e.py): `electrum history <port> <scripthash>`,
# `electrum height [--tls] <port>`, `electrum call [--tls] <port> <method>
# [json params]` (the result as JSON).
electrum() { python3 "$E2E_ROOT/tests/lib/e2e.py" "$@"; }
