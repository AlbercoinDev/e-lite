#!/usr/bin/env bash
# Crash and reorg recovery on a disposable regtest chain:
#   - repeated SIGKILL while indexing (RocksDB/WAL recovery),
#   - a reorg at the indexed tip (undo records),
#   - a write failure (file-size limit) followed by a clean restart.
. "$(dirname "$0")/../lib/e2e.sh"
e2e_init recovery
CORE="$RUN/core"; DATA="$RUN/index"; DATA2="$RUN/index-limit"; CONF="$RUN/e-lite.toml"
RPC=18488
BLOCKS="${E_LITE_RECOVERY_BLOCKS:-20000}"
core_start "$CORE" "$RPC"
core_mine_new "$CORE" "$RPC" rec "$BLOCKS" >/dev/null
TIP=$(cli "$CORE" "$RPC" getblockcount)
echo "chain: $TIP blocks"
e2e_conf "$CONF" index.batch_blocks=32 index.reorg_window=32 \
  server.listen= metrics.listen= metrics.enabled=false

# Committed height of an index, from `e-lite status --json` (the database is
# free again once the indexer is gone).
height() {
  local out
  for _ in $(seq 1 25); do
    out=$("$BIN" status --config "$CONF" --data-dir "$1" --json 2>"$RUN/status.err" || true)
    if printf '%s' "$out" | python3 -c 'import json,sys;print(json.load(sys.stdin)["indexed_height"])' 2>/dev/null; then
      return 0
    fi
    sleep 0.2
  done
  fail "status failed" "$RUN/status.err"
}
index_to_tip() { "$BIN" index --config "$CONF" --data-dir "$1" --no-follow >"$2" 2>&1; }

echo "== SIGKILL while indexing =="
for i in $(seq 1 8); do
  # The binary itself, not a function: `$!` must be the indexer.
  "$BIN" index --config "$CONF" --data-dir "$DATA" --no-follow >"$RUN/$i.log" 2>&1 &
  pid=$!
  sleep 0.25
  kill -9 "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  echo "kill $i: committed height $(height "$DATA")"
done
# The database must open after every abrupt stop; finish to the tip.
index_to_tip "$DATA" "$RUN/finish.log"
[ "$(height "$DATA")" = "$TIP" ] || fail "did not recover to $TIP" "$RUN/finish.log"
echo "recovered to $TIP after SIGKILL"

echo "== reorg at the indexed tip =="
FORK=$((TIP - 11))
cli "$CORE" "$RPC" invalidateblock "$(cli "$CORE" "$RPC" getblockhash "$FORK")" >/dev/null
# A new address changes the coinbase; otherwise regtest can rebuild the exact
# invalidated block and Core rejects it as duplicate-invalid.
core_mine_new "$CORE" "$RPC" rec 20 >/dev/null
NEWTIP=$(cli "$CORE" "$RPC" getblockcount)
echo "Core moved from $TIP to $NEWTIP (fork $FORK)"
index_to_tip "$DATA" "$RUN/reorg.log"
[ "$(height "$DATA")" = "$NEWTIP" ] || fail "reorg not recovered: $(height "$DATA") != $NEWTIP" "$RUN/reorg.log"
echo "reorg recovered to $NEWTIP"

echo "== write failure (file-size limit) then restart =="
set +e
# SIGXFSZ is the expected outcome; no core dump.
( ulimit -c 0; ulimit -f 8192; index_to_tip "$DATA2" "$RUN/limit.log" )
LIMIT_RC=$?
set -e
echo "limited run exit: $LIMIT_RC (expected failure or partial progress)"
index_to_tip "$DATA2" "$RUN/limit-restart.log"
[ "$(height "$DATA2")" = "$NEWTIP" ] || fail "did not recover after the write failure" "$RUN/limit-restart.log"
echo "recovered to $NEWTIP after the write failure"
echo "recovery OK"
