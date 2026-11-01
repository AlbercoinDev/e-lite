# ADR-013 — Derived `tx_ref` and flat `txids.dat`

**Date:** 2026-11-01 · **Status:** accepted

## Context
The original contract stored an allocated `u64` occurrence id in three places
(`txref_position`, `tx_occurrences`, `history`), which costs 8 bytes per
occurrence and per history relation and makes snapshots depend on allocation
state.

## Decision
- `tx_ref = (height << 24) | tx_pos`, derived, never stored.
- `tx_occurrences` is keyed by `(txid, height, tx_pos)` with an **empty value**
  (BIP30 occurrences preserved).
- `history` stores only `received_sat`, `spent_sat`, `flags`.
- Ordered TXIDs live in `txids.dat`, a flat file at `txnum * 32`; the DB stores
  `txnum_base` and `tx_count` per block.
- Crash ordering: append + fsync `txids.dat`, then commit the DB batch. On open,
  the file is truncated to `committed_tip.txnum`; a shorter file is fatal.

## Consequences
- 24-bit position cap (16.7 M tx/block; consensus bound is far lower) and
  40-bit height.
- Deterministic snapshots (no allocator state) and cheaper merkle proofs.
- Reorgs must truncate `txids.dat` to the new tip's `txnum`.

## Evidence
`txref_roundtrip`; `oracle_parity_100k_transactions`; `reopen_keeps_tip_and_truncates_extra_txids`.
