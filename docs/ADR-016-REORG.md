# ADR-016 — Bounded undo window instead of unbounded history

**Date:** 2026-11-01 · **Status:** accepted

## Context
Keeping a full undo journal for every block is expensive and unnecessary: a
reorg deeper than the operator-configured window is an operational emergency,
not a normal event. Rebuilding from scratch is also not acceptable on demand.

## Decision
- Every block stores a compact `undo` record: spent outpoints with their UTXO
  value, created outpoints, touched history keys, and TXIDs, with a SHA-256
  checksum.
- Undo records older than `reorg_window` (default 288) are deleted during commit.
- On a fork, walk back to the common ancestor inside the window, rewind with
  `undo` (one committed batch per block), truncate `txids.dat`, then apply the new
  branch.
- Deeper than the window → stop with `NEEDS_RECOVERY`; never serve a forged
  history.

## Consequences
- Disk overhead is bounded and configurable.
- A corrupt undo record aborts the rewind (checksum) instead of producing a
  half-reverted state.
- Pruned Core (mode B) will need a different recovery source; out of scope for
  v0.1.

## Evidence
`reorg_rewind_and_reapply` (1/N blocks, different branch, oracle parity);
`undo_roundtrip_and_checksum` (tamper rejected).
