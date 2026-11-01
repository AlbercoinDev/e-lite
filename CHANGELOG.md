# Changelog

## 0.1.0-alpha.2 — 2026-11-01

- Fix: the undo-record reader required 85 bytes per spent input instead of the
  81 written by the encoder, which aborted indexing with `impossible spent count`
  once the reorg window started pruning old records (seen on Signet at ~4.6k).
- Pruning no longer decodes old undo records: deleting them at the window
  boundary is idempotent and cannot stop indexing.
- Undo layout sizes are now shared constants for writer and reader; the minimum
  record length check is exact (92 bytes).
- Tests: spent-heavy undo round-trip, minimal record, prune boundary with
  input-heavy records.
- No schema change: existing databases are read as-is.

## 0.1.0-alpha.1 — 2026-11-01

- First public alpha: full-core indexing from Bitcoin Core (RocksDB).
- Electrum 1.4 server on loopback: headers, scripthash history/balance/UTXOs,
  transaction.get, get_merkle, id_from_pos, server.features/version.
- Networks: regtest, signet, testnet4, mainnet (explicit opt-in).
- Reorg handling with a bounded undo window (288 blocks).
- Prometheus /metrics and JSON /status on loopback.
- One-line installer/updater with optional official Bitcoin Core install.
- Debian 12+ package (.deb) built in debian:12; no systemd unit.

### Known limitations
- Confirmed chain only: no mempool, no push notifications, no broadcast.
- `full-core` only (Core prune=0); rawtx requires the block to exist in Core.
