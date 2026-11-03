#!/usr/bin/env bash
# e-lite all-in-one: install/update, configure, run and report.
#
# Local checkout:
#   bash scripts/e-lite-all-in-one.sh --network signet --acceptance --with-core --yes
#
# Published one-liner:
#   curl -fsSL https://raw.githubusercontent.com/AlbercoinDev/e-lite/main/scripts/e-lite-all-in-one.sh \
#     | bash -s -- --network signet --acceptance --with-core --yes
#
# Modes:
#   quick      regtest: start e-lite + Core, mine 101 blocks, query, report
#   suite      run the local e2e suite (launch, wait, pruned, regtest, resilience)
#   acceptance full signet/testnet4 sync from zero, no prune gaps, Electrum query
set -euo pipefail

REPO_URL="https://github.com/AlbercoinDev/e-lite.git"
NETWORK="regtest"
MODE="quick"
WITH_CORE="ask"       # ask | yes | no
ASSUME_YES=0
PREFIX="${E_LITE_PREFIX:-$HOME/.local/bin}"
RUN_ROOT="${E_LITE_RUN_ROOT:-$HOME/e-lite-test}"
RESET=1               # start from zero in quick/acceptance
CORE_DIR=""
DATA_DIR=""
RPC_PORT=""
ELECTRUM_PORT=""
METRICS_PORT=""
ROOT=""
TMP=""
ACCEPTANCE_LOG=""

log()  { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }
info() { printf '    %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

usage() {
  cat <<'EOF'
e-lite all-in-one

Usage:
  e-lite-all-in-one.sh [options]

Options:
  --network <name>       regtest | signet | testnet4 | mainnet (default regtest)
  --mode <mode>          quick | suite | acceptance (default quick)
  --acceptance           same as --mode acceptance
  --suite                same as --mode suite
  --quick                same as --mode quick
  --with-core            install/update Bitcoin Core
  --no-core              use an existing bitcoind only
  --yes, -y              non-interactive
  --prefix <dir>         e-lite install prefix (default ~/.local/bin)
  --run-root <dir>       test data root (default ~/e-lite-test)
  --core-datadir <dir>   explicit Core datadir
  --data-dir <dir>       explicit e-lite data dir
  --keep                 do not wipe quick/acceptance dirs before starting
  --reset                wipe quick/acceptance dirs (default)
  --help, -h             this help

Examples:
  # local regtest smoke test
  bash scripts/e-lite-all-in-one.sh --network regtest --quick --yes

  # full signet acceptance (needs >=15 GiB free)
  bash scripts/e-lite-all-in-one.sh --network signet --acceptance --with-core --yes

  # full testnet4 acceptance (needs >=20 GiB free)
  bash scripts/e-lite-all-in-one.sh --network testnet4 --acceptance --with-core --yes
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --network) shift; [ $# -gt 0 ] || die "--network needs a value"; NETWORK="$1" ;;
    --network=*) NETWORK="${1#--network=}" ;;
    --mode) shift; [ $# -gt 0 ] || die "--mode needs a value"; MODE="$1" ;;
    --mode=*) MODE="${1#--mode=}" ;;
    --acceptance) MODE="acceptance" ;;
    --suite) MODE="suite" ;;
    --quick) MODE="quick" ;;
    --with-core) WITH_CORE="yes" ;;
    --no-core) WITH_CORE="no" ;;
    --yes|-y) ASSUME_YES=1 ;;
    --prefix) shift; [ $# -gt 0 ] || die "--prefix needs a value"; PREFIX="$1" ;;
    --prefix=*) PREFIX="${1#--prefix=}" ;;
    --run-root) shift; [ $# -gt 0 ] || die "--run-root needs a value"; RUN_ROOT="$1" ;;
    --run-root=*) RUN_ROOT="${1#--run-root=}" ;;
    --core-datadir) shift; [ $# -gt 0 ] || die "--core-datadir needs a value"; CORE_DIR="$1" ;;
    --core-datadir=*) CORE_DIR="${1#--core-datadir=}" ;;
    --data-dir) shift; [ $# -gt 0 ] || die "--data-dir needs a value"; DATA_DIR="$1" ;;
    --data-dir=*) DATA_DIR="${1#--data-dir=}" ;;
    --keep) RESET=0 ;;
    --reset) RESET=1 ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1 (use --help)" ;;
  esac
  shift
done

case "$MODE" in quick|suite|acceptance) ;; *) die "bad --mode: $MODE" ;; esac
case "$NETWORK" in regtest|signet|testnet4|mainnet) ;; *) die "bad --network: $NETWORK" ;; esac
if [ "$MODE" = "quick" ] && [ "$NETWORK" != "regtest" ]; then
  die "quick mode only supports regtest; use --mode acceptance for $NETWORK"
fi
if [ "$MODE" = "acceptance" ] && { [ "$NETWORK" = "regtest" ] || [ "$NETWORK" = "mainnet" ]; }; then
  die "acceptance mode supports signet or testnet4 (mainnet needs a snapshot/full node)"
fi

export PATH="$PREFIX:$PATH"
if [ "$ASSUME_YES" -eq 1 ] && [ "$WITH_CORE" = "ask" ] && ! have bitcoind; then
  WITH_CORE="yes"
fi

TMP="$(mktemp -d "${TMPDIR:-/tmp}/e-lite-all-XXXXXX")"
cleanup() {
  set +e
  if [ -f "$TMP/serve.pid" ]; then
    kill -TERM "$(cat "$TMP/serve.pid")" 2>/dev/null
    wait "$(cat "$TMP/serve.pid")" 2>/dev/null
  fi
  if [ -n "$CORE_DIR" ] && [ -f "$CORE_DIR/$NETWORK/bitcoind.pid" ]; then
    kill -TERM "$(cat "$CORE_DIR/$NETWORK/bitcoind.pid")" 2>/dev/null
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT INT TERM

# --------------------------------------------------------------- source/tools
detect_root() {
  local src="${BASH_SOURCE[0]:-}"
  if [ -n "$src" ] && [ -f "$src" ]; then
    local d
    d="$(cd "$(dirname "$src")/.." && pwd)"
    if [ -f "$d/Cargo.toml" ] && [ -f "$d/tests/e2e_networks.sh" ]; then
      ROOT="$d"
      return
    fi
  fi
  mkdir -p "$RUN_ROOT"
  ROOT="$RUN_ROOT/src"
  have git || die "git is required to fetch the source; install git"
  if [ -d "$ROOT/.git" ]; then
    log "Updating source in $ROOT"
    git -C "$ROOT" fetch --depth 1 origin main
    git -C "$ROOT" reset --hard origin/main
  else
    rm -rf -- "$ROOT"
    log "Fetching e-lite source into $ROOT"
    git clone --depth 1 "$REPO_URL" "$ROOT"
  fi
}

ensure_tools() {
  local missing=()
  for c in curl tar sha256sum python3 git; do
    have "$c" || missing+=("$c")
  done
  if [ "${#missing[@]}" -gt 0 ]; then
    if [ -f /etc/debian_version ] && have apt-get; then
      log "Installing base tools: ${missing[*]}"
      if [ "$(id -u)" -eq 0 ]; then
        apt-get update -qq
        apt-get install -y -qq --no-install-recommends ca-certificates curl tar coreutils python3 git
      elif have sudo; then
        sudo apt-get update -qq
        sudo apt-get install -y -qq --no-install-recommends ca-certificates curl tar coreutils python3 git
      else
        die "missing ${missing[*]}; install them or run as root"
      fi
    else
      die "missing ${missing[*]}; install them manually"
    fi
  fi

  if ! have cargo; then
    log "Installing Rust (rustup, user-space)"
    have curl || die "curl is required to install Rust"
    curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi
  have cargo || die "cargo is still not available"

  if [ -f /etc/debian_version ] && have apt-get; then
    local build_missing=0
    for c in cc clang pkg-config; do have "$c" || build_missing=1; done
    if [ "$build_missing" -eq 1 ]; then
      log "Installing build dependencies (RocksDB/bindgen)"
      if [ "$(id -u)" -eq 0 ]; then
        apt-get update -qq
        apt-get install -y -qq --no-install-recommends build-essential clang libclang-dev pkg-config
      elif have sudo; then
        sudo apt-get update -qq
        sudo apt-get install -y -qq --no-install-recommends build-essential clang libclang-dev pkg-config
      else
        die "missing build tools; install build-essential clang libclang-dev pkg-config"
      fi
    fi
  fi
}

# ------------------------------------------------------------------ Core/e-lite
ensure_core() {
  if [ "$WITH_CORE" = "no" ]; then
    have bitcoind || die "bitcoind not found and --no-core was requested"
    return
  fi
  if [ "$WITH_CORE" = "ask" ] && have bitcoind; then
    info "Bitcoin Core: $(bitcoind -version 2>/dev/null | head -1)"
    return
  fi
  if [ "$WITH_CORE" = "ask" ] && [ ! -t 0 ]; then
    die "bitcoind not found; rerun with --with-core or install Bitcoin Core"
  fi
  if [ "$WITH_CORE" = "ask" ]; then
    printf 'Bitcoin Core not found. Install/update the latest official Core? [y/N] '
    read -r ans </dev/tty || ans=""
    case "$ans" in y|Y|yes|YES) ;; *) die "Bitcoin Core is required" ;; esac
  fi
  log "Installing/updating Bitcoin Core via install.sh"
  sh "$ROOT/install.sh" --with-core --yes --prefix "$PREFIX"
  have bitcoind || die "Bitcoin Core installation did not produce bitcoind in PATH"
}

build_elite() {
  log "Building e-lite (release)"
  ( cd "$ROOT" && cargo build --release --locked -p e-lite )
  mkdir -p "$PREFIX"
  install -m 0755 "$ROOT/target/release/e-lite" "$PREFIX/e-lite"
  "$PREFIX/e-lite" --version
}

free_port() {
  python3 - "$1" <<'PY'
import socket, sys
start = int(sys.argv[1])
for port in range(start, start + 200):
    s = socket.socket()
    try:
        s.bind(("127.0.0.1", port))
        print(port)
        s.close()
        break
    except OSError:
        s.close()
else:
    raise SystemExit("no free port")
PY
}

choose_ports() {
  case "$NETWORK" in
    regtest)  RPC_PORT="${RPC_PORT:-18443}"; ELECTRUM_PORT="${ELECTRUM_PORT:-50001}"; METRICS_PORT="${METRICS_PORT:-9184}" ;;
    signet)   RPC_PORT="${RPC_PORT:-38332}"; ELECTRUM_PORT="${ELECTRUM_PORT:-50001}"; METRICS_PORT="${METRICS_PORT:-9184}" ;;
    testnet4) RPC_PORT="${RPC_PORT:-48332}"; ELECTRUM_PORT="${ELECTRUM_PORT:-50001}"; METRICS_PORT="${METRICS_PORT:-9184}" ;;
    mainnet)  RPC_PORT="${RPC_PORT:-8332}";  ELECTRUM_PORT="${ELECTRUM_PORT:-50001}"; METRICS_PORT="${METRICS_PORT:-9184}" ;;
  esac
  # The quick/acceptance runs own Core, so choose free ports if the defaults
  # are busy. Existing external Cores are not used in these modes.
  if [ "$MODE" != "suite" ]; then
    RPC_PORT="$(free_port "$RPC_PORT")"
    ELECTRUM_PORT="$(free_port "$ELECTRUM_PORT")"
    METRICS_PORT="$(free_port "$METRICS_PORT")"
  fi
}

prepare_dirs() {
  if [ -z "$CORE_DIR" ]; then CORE_DIR="$RUN_ROOT/$NETWORK/core"; fi
  if [ -z "$DATA_DIR" ]; then DATA_DIR="$RUN_ROOT/$NETWORK/index"; fi
  mkdir -p "$RUN_ROOT"
  if [ "$RESET" -eq 1 ]; then
    local core_inside=0 data_inside=0
    case "$CORE_DIR" in "$RUN_ROOT"/*) core_inside=1 ;; esac
    case "$DATA_DIR" in "$RUN_ROOT"/*) data_inside=1 ;; esac
    if [ "$core_inside" -eq 1 ] && [ "$data_inside" -eq 1 ]; then
      log "Resetting test data: $CORE_DIR and $DATA_DIR"
      rm -rf -- "$CORE_DIR" "$DATA_DIR"
    else
      warn "not wiping explicit dirs outside $RUN_ROOT; delete them manually for a from-zero run"
    fi
  fi
  mkdir -p "$CORE_DIR" "$DATA_DIR"
}

write_config() {
  local min_free=2
  [ "$NETWORK" = "regtest" ] && min_free=1
  [ "$NETWORK" = "mainnet" ] && min_free=10
  CONF="$RUN_ROOT/$NETWORK-e-lite.toml"
  cat >"$CONF" <<EOF
[network]
name = "$NETWORK"
data_dir = "$DATA_DIR"

[core]
manage = "managed"
bitcoind = "$(command -v bitcoind)"
core_datadir = "$CORE_DIR"
rpc_url = "http://127.0.0.1:$RPC_PORT"
prune_mib = 1
extra_args = ["-listen=0"]
timeout_seconds = 30

[index]
block_source = "auto"
reorg_window = 288
batch_blocks = 256

[pruning]
min_free_gib = $min_free
max_core_lag_blocks = 192
poll_seconds = 10

[server]
listen = "127.0.0.1:$ELECTRUM_PORT"

[metrics]
listen = "127.0.0.1:$METRICS_PORT"

[rawtx]
enabled = true
EOF
  info "Config: $CONF"
}

cli() {
  local chain_arg="-$NETWORK"
  [ "$NETWORK" = "mainnet" ] && chain_arg="-chain=main"
  bitcoin-cli "$chain_arg" -datadir="$CORE_DIR" -rpcport="$RPC_PORT" "$@"
}
status_json() { curl -s "http://127.0.0.1:$METRICS_PORT/status" 2>/dev/null || true; }
wait_core() {
  for _ in $(seq 1 200); do
    if cli getblockcount >/dev/null 2>&1; then return 0; fi
    sleep 0.5
  done
  return 1
}
wait_ready() {
  for _ in $(seq 1 240); do
    local s
    s="$(status_json)"
    printf '%s' "$s" | grep -q '"state": "READY"' && return 0
    sleep 0.5
  done
  return 1
}
start_elite() {
  log "Starting e-lite (Core managed by e-lite)"
  "$PREFIX/e-lite" run --config "$CONF" >"$TMP/serve.log" 2>&1 &
  echo $! >"$TMP/serve.pid"
}

scripthash_for_address() {
  local addr="$1"
  python3 - "$addr" "$CORE_DIR" "$RPC_PORT" "$NETWORK" <<'PY'
import hashlib, json, subprocess, sys
addr, core, rpc, network = sys.argv[1:5]
chain_arg = "-chain=main" if network == "mainnet" else f"-{network}"
out = subprocess.check_output([
    "bitcoin-cli", chain_arg, f"-datadir={core}", f"-rpcport={rpc}",
    "getaddressinfo", addr,
])
print(hashlib.sha256(bytes.fromhex(json.loads(out)["scriptPubKey"])).digest()[::-1].hex())
PY
}

report_common() {
  local report="$RUN_ROOT/last-report.txt"
  {
    echo "e-lite all-in-one report"
    echo "date: $(date -Is)"
    echo "network: $NETWORK"
    echo "mode: $MODE"
    echo "core dir: $CORE_DIR"
    echo "data dir: $DATA_DIR"
    echo "config: $CONF"
    echo "e-lite: $("$PREFIX/e-lite" --version 2>&1 | head -1)"
    echo "core: $(bitcoind -version 2>/dev/null | head -1)"
    echo "rpc: http://127.0.0.1:$RPC_PORT"
    echo "electrum: 127.0.0.1:$ELECTRUM_PORT"
    echo "metrics: 127.0.0.1:$METRICS_PORT"
    if [ -n "$ACCEPTANCE_LOG" ]; then echo "acceptance log: $ACCEPTANCE_LOG"; fi
    echo
    echo "disk:"
    df -h "$CORE_DIR" "$DATA_DIR" 2>/dev/null | tail -n +2
    echo
    if [ -f "$TMP/results.txt" ]; then
      echo "tests:"
      cat "$TMP/results.txt"
      echo
    fi
    echo "status:"
    status_json | python3 -m json.tool 2>/dev/null || echo "(no status)"
    if [ -f "$TMP/serve.log" ]; then
      echo
      echo "last log lines:"
      tail -20 "$TMP/serve.log"
    fi
  } | tee "$report"
  info "Report: $report"
}

run_quick() {
  prepare_dirs
  write_config
  start_elite
  log "Waiting for Core RPC"
  wait_core || { tail -60 "$TMP/serve.log"; die "Core did not start"; }
  cli createwallet allinone >/dev/null 2>&1 || cli loadwallet allinone >/dev/null 2>&1 || true
  local addr sh
  addr="$(cli -rpcwallet=allinone getnewaddress)"
  log "Mining 101 regtest blocks"
  cli -rpcwallet=allinone generatetoaddress 101 "$addr" >/dev/null
  log "Waiting for READY"
  wait_ready || { tail -80 "$TMP/serve.log"; die "index did not reach READY"; }
  sh="$(scripthash_for_address "$addr")"
  local entries
  entries="$(python3 "$ROOT/tests/query_history.py" "$ELECTRUM_PORT" "$sh")"
  info "history entries: $entries"
  printf 'quick regtest: OK (%s history entries)\n' "$entries" >"$TMP/results.txt"
  report_common
  kill -TERM "$(cat "$TMP/serve.pid")" 2>/dev/null || true
  wait "$(cat "$TMP/serve.pid")" 2>/dev/null || true
  rm -f "$TMP/serve.pid"
  [ "$entries" -ge 101 ] || die "history incomplete: $entries"
  log "quick regtest OK"
}

run_suite() {
  # The suite scripts use their own temporary dirs; only create a report config.
  local saved_reset="$RESET"
  RESET=0
  prepare_dirs
  write_config
  RESET="$saved_reset"
  export E_LITE_BIN="$PREFIX/e-lite"
  local failed=0
  : >"$TMP/results.txt"
  local scripts=(
    tests/e2e_launch_core.sh
    tests/e2e_wait_core.sh
    tests/e2e_external_pruned.sh
    tests/e2e_pruned_ibd.sh
    tests/e2e_regtest.sh
    tests/e2e_resilience.sh
  )
  for s in "${scripts[@]}"; do
    log "Running $s"
    if ( cd "$ROOT" && "$s" >"$TMP/$(basename "$s").log" 2>&1 ); then
      info "PASS $s"
      echo "PASS $s" >>"$TMP/results.txt"
    else
      info "FAIL $s"
      echo "FAIL $s" >>"$TMP/results.txt"
      tail -30 "$TMP/$(basename "$s").log" >&2 || true
      failed=1
    fi
  done
  report_common
  [ "$failed" -eq 0 ] || die "one or more e2e tests failed"
  log "suite OK"
}

run_acceptance() {
  if [ -z "$CORE_DIR" ]; then CORE_DIR="$RUN_ROOT/$NETWORK/core"; fi
  if [ -z "$DATA_DIR" ]; then DATA_DIR="$RUN_ROOT/$NETWORK/index"; fi
  mkdir -p "$RUN_ROOT" "$(dirname "$CORE_DIR")" "$(dirname "$DATA_DIR")"
  local required=15
  [ "$NETWORK" = "testnet4" ] && required=20
  local free_core free_data free
  free_core=$(df -Pk "$(dirname "$CORE_DIR")" | awk 'NR==2{print $4}')
  free_data=$(df -Pk "$(dirname "$DATA_DIR")" | awk 'NR==2{print $4}')
  free=$(( (free_core < free_data ? free_core : free_data) / 1024 / 1024 ))
  info "free space for Core/index: ${free} GiB (required ${required} GiB)"
  [ "$free" -ge "$required" ] || die "not enough free space: ${free} GiB < ${required} GiB"
  prepare_dirs
  write_config
  log "Running full $NETWORK acceptance (Core launched by e-lite, manual prune=1)"
  local log="$RUN_ROOT/$NETWORK-acceptance.log"
  ACCEPTANCE_LOG="$log"
  if ( cd "$ROOT" && \
       E_LITE_BIN="$PREFIX/e-lite" \
       E_LITE_NETWORK="$NETWORK" \
       E_LITE_CORE_DATADIR="$CORE_DIR" \
       E_LITE_E2E_DATA_DIR="$DATA_DIR" \
       E_LITE_E2E_REQUIRED_FREE_GIB="$required" \
       E_LITE_E2E_MIN_FREE_GIB="${E_LITE_E2E_MIN_FREE_GIB:-2}" \
       E_LITE_E2E_RPC_PORT="$RPC_PORT" \
       E_LITE_E2E_ELECTRUM_PORT="$ELECTRUM_PORT" \
       E_LITE_E2E_METRICS_PORT="$METRICS_PORT" \
       tests/e2e_networks.sh >"$log" 2>&1 ); then
    tail -20 "$log"
    printf '%s acceptance: OK\n' "$NETWORK" >"$TMP/results.txt"
    report_common
    log "$NETWORK acceptance OK"
  else
    tail -60 "$log" >&2 || true
    printf '%s acceptance: FAIL\n' "$NETWORK" >"$TMP/results.txt"
    report_common
    die "$NETWORK acceptance failed (log: $log)"
  fi
}

# ------------------------------------------------------------------------ main
log "e-lite all-in-one: network=$NETWORK mode=$MODE"
ensure_tools
detect_root
ensure_core
build_elite
choose_ports
case "$MODE" in
  quick) run_quick ;;
  suite) run_suite ;;
  acceptance) run_acceptance ;;
esac
