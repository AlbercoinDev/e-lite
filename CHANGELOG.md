# Changelog

## 0.1.0-alpha.8 — 2026-11-02

- **Silent payments opt-in gate**: `[silent_payments] enabled = false` by
  default. The subscribe/unsubscribe methods return an explicit server error
  instead of `unknown method`; enabling them reports that the BIP352 indexer is
  not in this build.
- **ADR-018** documents the protocol shape, the scan-key privacy constraints,
  the prevout-pubkey requirement (schema v2 or on-demand scan) and the future
  implementation/test plan.
- No protocol or schema change for normal wallets.

## 0.1.0-alpha.7 — 2026-11-01

- **Mode B (`pruned-ibd`)**: index while Core runs with `prune=1` (manual).
- **Disk supervisor** with hysteresis: pauses Core downloads
  (`setnetworkactive(false)`) below the low watermark and resumes above the high
  watermark.
- **Safe prune gate**: minimum of committed-tip minus undo margin, earliest
  active rawtx reader minus one, and Core tip minus recent-blocks margin. Never
  prunes while paused, behind, or with an active reader; `min_gain` prevents
  request storms. Only `pruneblockchain` is used; no block files are deleted by
  e-lite.
- Bounded indexing steps so the supervisor can act during long catch-ups.
- Reader tracking around `transaction.get` so pruning respects in-flight rawtx
  resolutions.
- Tests: gate formula, hysteresis, reader blocking, prune gain, and a regtest
  `prune=1 -fastprune` E2E: 2000 blocks indexed, Core pruned to 1530, old block
  unavailable in Core while e-lite still served 2000 history entries.

## 0.1.0-alpha.6 — 2026-11-01

- **BIP324 v2 P2P transport** (encrypted, via the `bip324` crate) with fallback
  to v1 for peers that do not support it. Per-peer Tor isolation is unchanged.
- Correct BIP324 application framing (1/13-byte message type) and short type IDs
  for block/ping/pong/getdata.
- New `[rawtx] p2p_v2` option (default true).
- Tests: v2 mock peer, v1 fallback, and live v2/v1 fetches against Bitcoin Core.

## 0.1.0-alpha.5 — 2026-11-01

- **Clean shutdown**: SIGTERM/SIGINT stop at a commit boundary, flush the pending
  group and exit 0.
- **Core outage resilience**: transient RPC failures set `CORE_UNAVAILABLE`,
  keep serving confirmed reads from the index and retry with backoff (1s→30s).
  Core cookies are reloaded on 401, so a Core restart (which rotates the cookie)
  recovers automatically.
- **Snapshot export/import**: `export-snapshot --output` writes a tar.gz with a
  manifest (network, genesis, tip, txnum, schema, canonical digest);
  `import-snapshot --input` restores into an empty data directory and verifies
  the digest.
- Rate-limited protocol error logging (method + code, no sensitive data).
- New resilience test: Core stop → reads still work → Core restart with rotated
  cookie → recovery → SIGTERM exit 0.
- E2E now also checks snapshot digest equality after restore.

## 0.1.0-alpha.4 — 2026-11-01

- **Mempool overlay** rebuilt from Core: per-script unconfirmed history, balance
  deltas, mempool UTXOs and spent outpoints (including chained transactions),
  refreshed every 3 s and immediately after a new block.
- **Fees**: `blockchain.estimatefee`, `blockchain.relayfee` and
  `mempool.get_fee_histogram`; `estimatefee` never returns `-1`.
- **Broadcast and verbose tx**: `blockchain.transaction.broadcast`,
  `transaction.get(..., true)` and `blockchain.block.stats`.
- **Push notifications**: headers and scripthash status are pushed to subscribed
  clients; status includes mempool entries.
- `scripthash.get_mempool`, unconfirmed balances and `listunspent` now include
  mempool data.
- `run` always follows the tip (`--no-follow` applies only to `index`).
- Per-request line limit (1 MiB) and notification counters in `/status` and
  `/metrics`.
- Tests: Sparrow-like connect sequence, broadcast, mempool overlay, notifications,
  fee fallbacks and block stats.

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
