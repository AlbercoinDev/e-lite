# Changelog

## 0.1.0-alpha.3 — 2026-11-01

- **Pruned Core support for serving**: the index must already cover the chain;
  `doctor` and the indexer refuse a prune gap with a clear message.
- **Raw transaction resolver** with a strict order: local Core -> validated cache
  -> external sources. Whole blocks only, never TXID lookups.
- **Verification**: block hash/header against the local chain, Merkle root, TXID
  and BIP141 witness commitment. Invalid data is never served or cached.
- **Sources**: Esplora-compatible `/block/:hash/raw` endpoints and a minimal
  Bitcoin P2P client (version/verack, getdata, NODE_NETWORK filtering).
- **Privacy defaults**: Tor SOCKS5h with stream isolation and fail-closed without
  Tor; 3 decoys per external fetch; 10 GiB validated LRU cache; no sensitive
  labels in metrics.
- `transaction.get` serves from the cache on the second access.
- New `[rawtx]` configuration and `rawtx` counters in `/status` and `/metrics`.
- Tests: witness/Merkle verification, cache eviction, source fallback, decoys,
  fail-closed Tor, P2P handshake, prune-gap guard, plus live Esplora and live P2P
  checks against real endpoints.

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
