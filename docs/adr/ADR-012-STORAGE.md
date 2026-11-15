# ADR-012 — RocksDB as the storage engine

**Date:** 2026-11-01 · **Status:** accepted

## Context
The index stores hundreds of millions of keys on mainnet, is written mostly in
block order with random key inserts, is read by prefix ranges, and needs atomic
multi-family commits plus consistent read snapshots.

## Options
1. **RocksDB** — LSM, WriteBatch, snapshots, prefix bloom, compaction tuning.
   Used in production by electrs, Fulcrum and BWT.
2. **LMDB** — fastest point reads (mmap) but a fixed map size and no compaction;
   both are operational footguns for a growing index.
3. **redb** — pure Rust and simple, but unproven at this scale and with COW write
   amplification.

## Decision
RocksDB, pinned (`rocksdb 0.24`, `librocksdb-sys 0.17.3`, RocksDB 10.4.2),
statically linked. One column family per family: `meta`, `blocks`,
`tx_occurrences`, `history`, `utxo`, `utxo_by_script`, `undo`.

## Consequences
- Build requires C++ and libclang (bindgen) and takes minutes.
- Binary is self-contained (no shared RocksDB dependency).
- Compaction/compression tuning can be improved later without schema changes.
- No engine abstraction: the `Store` type is concrete. The oracle test provides
  the independent implementation instead of a second backend.

## Evidence
`cargo test --workspace`; 100k-occurrence corpus parity with the oracle;
1000 regtest blocks indexed in 3.5 s on a 7.7 GiB / HDD host (see
`docs/benchmarks/F0.md`).
