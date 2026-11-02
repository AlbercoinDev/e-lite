# ADR-019 — Bulk ingest from Bitcoin Core block files

**Date:** 2026-11-02 · **Status:** accepted

## Context

The RPC ingest path (`getblockhash` + `getblock <hash> 0`) is limited by a
round trip and hex/JSON serialization per block, and it competes with Core's IBD
for `cs_main` and the disk. On a pruned node it also races automatic pruning:
Core decides what to delete from its own tip, not from e-lite's committed tip.

Core stores raw blocks in `blocks/blkNNNNN.dat`. Since Core 28 the files are
obfuscated by default: an 8-byte key in `blocks/xor.dat` is XORed over the whole
file, repeating and aligned to the absolute file offset. A pre-XOR blocksdir or
`-blocksxor=0` uses a zero key.

## Options evaluated

1. **RPC pipeline** (keep-alive, batched calls, parallel fetch). Removes some
   overhead but still pays hex/JSON and cannot read ahead of Core's RPC view.
2. **Core REST `.bin`**. Raw bytes, but requires `-rest=1`, still copies through
   the HTTP server and is still bounded by Core's RPC path.
3. **Direct `blk*.dat` reader.** Reads the same bytes Core wrote, sequentially,
   with no JSON/hex and no `cs_main`. Requires read access to the datadir and
   depends on Core's on-disk format.
4. **Snapshot/import only.** Fast bootstrap, but not a live ingest solution.

## Decision

Implement a **read-only bulk reader** with RPC as the permanent fallback:

- `[index] block_source = "auto" | "rpc" | "files"` (CLI `--block-source`);
  `auto` uses files when readable and silently falls back to RPC.
- Heights are always resolved with batched `getblockhash`; only active-chain
  hashes are read from disk. Forks and reorgs keep using the RPC/undo path.
- A scanner indexes record headers into `hash -> (file, offset, size)` and
  persists the locator in `<data-dir>/blockfiles.idx` (schema/network/genesis and
  XOR fingerprint in the header; torn tails are truncated).
- Records are `magic[4] || size_le[4] || block[size]`; the XOR key is applied at
  the absolute file offset. Preallocated zeros are treated as the un-written
  tail and retried; the last record is only made durable once the next record
  starts (or is re-read after a restart).
- Fetches run through `readers` workers (default auto, 0 = CPU-based); the writer
  consumes them in height order through a bounded reorder buffer.
- Every block is verified by hash and Merkle root before `apply_block`; a stale
  or missing locator falls back to `getblock`.
- Undo records are only written for the reorg window (plus a safety margin).
  `prune_undo` deletes every record below the window in one pass so the canonical
  digest does not depend on the commit batch size.
- `doctor` reports the blocks directory, file count, XOR state and magic.
- For automatic pruning (`-prune=NNN`), pause Core downloads when the index lag
  exceeds the dynamic `max_core_lag_blocks` (configured 512, capped by Core's
  `blocks - pruneheight` retention) and resume at 75%. See ADR-020.

## Consequences and risks

- The index contents and every Electrum/rawtx/snapshot feature are unchanged;
  only the block source changes. A/B snapshot parity is the acceptance test.
- New requirement: read access to Core's blocks directory. A remote Core, a
  container without the datadir, or a custom `-blocksdir` needs `blocks_dir` or
  the RPC fallback.
- `blk*.dat` is not a stable public API. A format change must fail closed and
  fall back to RPC; the locator is derived data and may be rebuilt.
- Memory: roughly 100 MB of locators for 1M blocks. Disk: ~48 bytes per block in
  the sidecar. First run against a fully synced node scans the files once.
- The next bottleneck is the apply/store path; `multi_get_cf`, a shared RocksDB
  cache, bloom filters and adaptive group commits are part of this change.

## Evidence

- `crates/el-blocks` unit tests: XOR alignment, header hash vs rust-bitcoin,
  scanner/reader round trip, probe.
- A/B parity on regtest: RPC and file ingest produce the same canonical snapshot
  digest.
- Crash/restart parity: `SIGKILL` during a 20 000-block index, restart, same
  digest as a continuous run.
- `tests/e2e_regtest.sh` and `tests/e2e_pruned_ibd.sh` pass with bulk enabled.
- Regtest bulk throughput: ~20 000 blocks/s for 1-transaction blocks.

## Migration

No schema change. The sidecar is new and optional; `block_source` defaults to
`auto`, so existing configurations keep working and gain the fast path when the
datadir is readable. Snapshot import/export is unaffected.
