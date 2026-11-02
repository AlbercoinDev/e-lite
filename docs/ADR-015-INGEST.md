# ADR-015 — Ingest pipeline: raw blocks, prefetch, group commit

**Date:** 2026-11-01 · **Status:** accepted

## Decision
- Blocks are fetched with `getblock <hash> 0` (raw hex) and decoded with
  `rust-bitcoin`; JSON verbosity is never used on the ingest path.
- One fetcher thread fills a bounded channel (16 blocks) while the writer applies
  blocks in order. Only one writer touches the index.
- Blocks are committed in groups of `batch_blocks` (default 256) with one
  `WriteBatch`, one `txids.dat` append/fsync and one `sync` write. `undo` records
  are per block; `committed_tip` advances by group.
- RPC responses are read using `Content-Length` instead of waiting for EOF.
  Waiting for the FIN added ~50 ms per call on loopback (measured); the change
  took 1000 blocks from 104 s to 3.5 s.

## Consequences
- A crash loses at most the uncommitted group (re-indexed on restart).
- `committed_tip` is only visible after the whole group is durable.
- Snapshot comparisons must stop at group boundaries; `export-snapshot` reads the
  committed tip.

## Update 2026-11-02 (ADR-019)
The RPC fetch is no longer the only block source. The default `block_source =
"auto"` reads the same raw blocks from Core's `blk*.dat` when the datadir is
readable, with the RPC path as fallback. The locator, XOR handling and
verification are documented in `ADR-019-BULK.md`; batching, prefetch and the
group-commit behaviour described here are unchanged.

## Evidence
`restart_midway_equals_continuous`; `docs/benchmarks/F0.md`; `docs/benchmarks/F1.md`.
