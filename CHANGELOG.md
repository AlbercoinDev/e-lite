# Changelog

## 0.1.0-alpha.16 — 2026-11-03

### Security audit and documentation rewrite

- **Electrum request lines are capped at 1 MiB** before allocation; a client can
  no longer grow a per-connection buffer indefinitely without a newline.
- **`max_clients` is reserved atomically before spawning a client thread**, so a
  burst cannot exceed the configured limit.
- **Snapshot import rejects symlinks, hardlinks and special files**, preventing
  archive extraction outside the target or special-file creation.
- **Esplora endpoints must be http/https**; invalid schemes are skipped.
- **Non-loopback Electrum/metrics listeners log a startup warning** (no TLS or
  authentication is implemented).
- **Project memory added**: `AGENTS.md` and `docs/PROJECT_MEMORY.md` lock the
  one-line install/update UX, Core variant, documentation contract, current/
  future feature sections and audit rules.
- **Documentation regenerated** around `INSTALL`, `ARCHITECTURE`, `OPERATIONS`,
  `PRIVACY`, `FEATURES`, `ROADMAP`, `TUNING`, `AUDIT` and `GETTING-STARTED`.
- **Audit report** added in `docs/AUDIT.md` with fixed findings and residual
  risks (TLS/auth, Core RPC TLS, snapshot archive limits, provider trust).

## 0.1.0-alpha.15 — 2026-11-03

### Container safety and conservative reader scaling

- **Automatic readers are capped by profile and memory**: 4 (HDD), 8 (SSD),
  16 (NVMe), never 28+ just because a read was fast. The controller now adds
  readers only when read latency is high and the apply stage is not the
  bottleneck.
- **Explicit `readers`, `cache_mib` and `commit_interval_ms` are locked**: the
  controller does not override them, except a critical memory floor.
- **Container-aware memory accounting**: `MemTotal`/`MemAvailable` are min'd
  with the cgroup v2/v1 limit and usage, so LXC/Docker do not over-allocate
  cache or workers.
- **Memory pressure backs off cache first**, keeps a reader floor of 2, and no
  longer collapses to one reader/64 MiB on a transient dip.
- **Docs**: `docs/TUNING.md` now documents the reader caps, locks and LXC
  OOM troubleshooting.

## 0.1.0-alpha.14 — 2026-11-03

### Autotuning, profiles and hot reload

- **Startup profiles**: `auto | hdd | ssd | nvme | manual`, selected from the
  `rotational` hint and RAM. Profiles set readers, commit interval, batch size
  and block-cache size.
- **Event-driven controller**: only changes one knob every 30 s, with hard
  bounds and no action while the disk/prune guards or reorg repair are active.
  It adjusts readers, commit interval/batch and block cache from measured
  read/apply/commit latency, tx/s and cache hit rate.
- **Hot reload with `SIGUSR1`**: readers, cache, commit interval, batch,
  profile, adaptive flag and pruning thresholds are applied to the running
  process. Invalid files keep the previous config; cold fields are reported.
- **Dynamic RocksDB cache**: the shared block cache is retained by `Store` and
  resized with `Cache::set_capacity`; mutable `max_background_jobs` is applied
  at runtime when RocksDB accepts it.
- **Bigger automatic cache/jobs ceilings**: auto cache is no longer capped at
  2 GiB, and background jobs are no longer capped at 4.
- **Instrumentation**: `/status` exposes a `tuning` object with effective
  readers/commit/batch/cache/profile and read/apply/commit/tx/cache EWMA
  metrics; `/metrics` exports the same values.
- **Documentation**: `docs/TUNING.md` and updated README/config examples.
- **CLI help** corrected for the new manual-prune default.

## 0.1.0-alpha.13 — 2026-11-03

### From-zero pruned IBD is manual by default

- **`prune_mib` defaults to `1` (manual pruning).** Core no longer deletes block
  files on its own during IBD. e-lite's prune gate is the only pruning
  authority: it never requests a cut above `committed - 288`, so Core cannot
  prune a block the index still needs. `0` remains a full node.
- **Automatic pruning requires an explicit opt-in.** `prune_mib >= 550` is
  rejected unless `[pruning] allow_auto_prune = true`. This prevents the
  from-zero default from recreating the prune-gap race.
- **Lag guard is manual-mode aware.** It is skipped for `prune_mib = 1`. For
  automatic pruning, `lag_limit` is hard-capped at 224 blocks (64 below Core's
  288-block floor), the default is 192, it resumes at 50 %, and the live check
  runs every 100 ms instead of once per second.
- **Disk backpressure keeps draining.** Below `min_free_gib` Core downloads
  pause but indexing continues, so the prune gate can delete old block files and
  free space. Only below `critical_free_gib` does indexing stop. This removes
  the old all-stop deadlock.
- **Prune requests are cheaper.** `min_gain_blocks` defaults to 288 and a new
  `prune_interval_seconds` (default 30) throttles no-op `pruneblockchain` calls
  that made Core flush during IBD. Low disk bypasses both as an emergency prune.
- **Prune-gap repair.** If automatic pruning still leaves a gap (or an existing
  index is behind an externally pruned Core), the indexer can fetch only the
  missing blocks from the validated rawtx cache, P2P or Esplora sources,
  verify hash/merkle, and continue. `repair_max_gap_blocks` (default 2048)
  bounds the work. No snapshot or full-chain backup is created.
- **Progress lines show live Core/prune height.** The per-call snapshot is no
  longer used as live lag, so a stale `lag 0` cannot hide a prune race.
- **Scripts and docs** use `prune_mib = 1` for from-zero signet/testnet4 runs.
  No snapshot is required or created by the flow.

## 0.1.0-alpha.12 — 2026-11-02

### Download and prune fixes

- **The lag pause is no longer undone by the crash-recovery marker.** The
  `core-paused-by-e-lite` recovery ran on every catch-up call, so a live lag
  pause was resumed on the next iteration, bypassing the 75% hysteresis and the
  reader check. Recovery now only runs when no live lag pause is in progress.
- **Prune requests are throttled by the requested height.** Core prunes at
  block-file granularity and can return the same height for thousands of
  blocks; retrying on every supervisor tick forced Core to flush during IBD.
  The supervisor now records the last requested height and only retries after
  `min_gain` blocks of safe-height progress.
- **Default `max_core_lag_blocks` is 512** (still capped by `blocks -
  pruneheight - 32`). On signet with `prune=550` the retention is thousands of
  blocks, so the old 256 limit paused Core unnecessarily.
- **Progress lines include `core`, `lag` and elapsed seconds**, so the catch-up
  burst rate is interpretable. All-in-one and network tests now use 512.

## 0.1.0-alpha.11 — 2026-11-02

### Autonomous startup and Core lifecycle

- **`run` waits for Core and auto-initializes.** With an empty database it binds
  the ports, reports `WAITING_FOR_CORE` and initializes as soon as Core appears.
  With an existing database it serves cached history while Core is down.
- **`[core] manage = external | auto | managed`.** `auto` uses a running Core
  read-only or starts it; `managed` always starts it. Only a Core started by
  e-lite can be paused, pruned or stopped.
- **Core launcher/supervisor**: `bitcoind`, `core_datadir`, `prune_mib`
  (`550` max pruning, `1` manual, `0` full), `extra_args`, `restart`,
  `stop_on_exit`, `log_file`, PID tracking and adoption after a crash.
- **Local-only pruned IBD.** A pruned Core that e-lite did not start, or a
  remote pruned Core, is rejected for a from-scratch index with instructions;
  e-lite never pauses a node it does not own.
- **Dynamic lag guard** based on `blocks - pruneheight` (default 256, resume at
  75%) instead of a fixed 32. It is reader-aware and does not pause while
  `pruneheight == 0`.
- **Disk guard on both filesystems** (index and Core datadir): below
  `min_free_gib` it pauses Core downloads and indexing, with adaptive
  hysteresis. The check is independent of Core RPC (`disk_tick`) and the
  indexer checks again before Core RPC and during block application; an
  uncommitted group is discarded so the disk is not pushed further. A starting
  warning reports free space below the threshold.
- **Reorg safety:** after a rewind the indexer re-reads the committed tip
  before replaying blocks.
- **From-zero disk requirement:** signet needs ≥15 GiB free and testnet4 ≥20 GiB
  for a full acceptance run; `tests/e2e_networks.sh` refuses to start below
  `E_LITE_E2E_REQUIRED_FREE_GIB` and documents how to override it.
- **Pause marker** so a crash while Core was paused is recovered on restart.
- **Prune height** now records the height Core actually returns, not the
  requested one.
- **Error classification**: transient “pruned data”/Core outages are retried;
  genesis/schema/txids/prune-gap failures are fatal.

### Configuration and networks

- `network = "auto"` detects the chain from Core or an existing database.
- Per-network Esplora defaults (mainnet, signet, testnet4; none on regtest).
- `rpc_user`/`rpc_password` for a remote Core; WAN is documented as HTTP over
  SSH/VPN.
- `parse_url` uses each network's default RPC port and supports IPv6 literals.
- Simplified config: removed `[index] mode`, `[pruning] enabled`,
  `undo_margin_blocks`, `core_recent_blocks_margin`, `min_gain_blocks`,
  `resume_core_lag_blocks`, `max_group_txs`, `commit_interval_ms` and
  `p2p_timeout_seconds`; renamed `bulk_readers`→`readers`,
  `db_cache_mib`→`cache_mib`, `cache_gib`→`cache_mib`.
- Validation: `prune_mib` must be `0`, `1` or `>= 550`; `data_dir` cannot be
  inside the Core datadir; `managed` requires an explicit network and loopback
  RPC.

### Performance and correctness

- The global index lock is no longer held across Core or external network I/O;
  a slow `transaction.get` cannot block index commits.
- Mempool refresh builds the candidate outside the write lock.
- `db_bytes` is cached; header checkpoint proofs use a cached header chain;
  history is capped at 100k entries per script.
- Notifications use a dirty-set: the indexer marks scripts touched by each
  confirmed block and the mempool marks additions/removals/height changes, so
  only affected subscriptions are recomputed. Reorgs force a full refresh.
- RocksDB tuning is automatic (`cache_mib = 0`, `readers = 0`) and open files are
  bounded.
- `family_digests` streams the flat TXID file instead of allocating it whole.

### Bulk block ingest

- **Bulk block ingest**: e-lite can read Core's `blk*.dat` directly
  (`[index] block_source = "auto" | "rpc" | "files"`, `--block-source`,
  `--blocks-dir`). Handles Core's default `-blocksxor` key (`blocks/xor.dat`),
  preallocated files, partial tails and file pruning. A persistent locator
  (`blockfiles.idx`) makes restarts cheap. RPC is always the fallback.
- **Faster ingest**: one batched `getblockhash` per window instead of one RPC per
  block; `readers` parallel fetch/read workers with an ordered writer;
  adaptive group commits (`batch_blocks`, internal TX/time limits); no fixed 1 s
  sleep between windows; TXIDs computed once and reused for the Merkle root.
- **Undo only inside the reorg window**: historical blocks no longer write undo
  records that would be deleted a few commits later. `prune_undo` now removes
  every record below the window in one pass, so the canonical digest is
  independent of the commit batch size.
- **RocksDB tuning**: shared block cache (`cache_mib`), bloom filters, larger
  write buffers and batched prevout reads (`multi_get_cf`).
- **Prune/ingest guard**: with automatic pruning, e-lite pauses Core downloads
  when the index lags more than the dynamic `max_core_lag_blocks` limit, then
  resumes at 75% of that limit.
- `doctor` reports the blocks directory, file count, XOR state and magic.
- Verified: A/B digest parity (RPC vs files), crash/restart parity, existing
  regtest and pruned-IBD E2E.

### Documentation

- New `scripts/e-lite-all-in-one.sh`: one command to install/update
  dependencies, Rust, e-lite and Bitcoin Core; configure; run a regtest quick
  test, the local e2e suite or the full signet/testnet4 acceptance; and write a
  report (`~/e-lite-test/last-report.txt`).
- New `docs/GETTING-STARTED.md`: prerequisites, disk planning, the one-command
  flow, regtest quickstart, full signet/testnet4 acceptance, troubleshooting and
  checklist.
- `docs/benchmarks/F2.md` records the two full-sync attempts that filled the
  disk, the root cause, the fix and the remaining manual acceptance.
- `docs/ADR-020-CORE-LIFECYCLE.md` documents the Core lifecycle and guards.

## 0.1.0-alpha.10 — 2026-11-02

- **Reorg both ways**: when Core's chain is shorter than the index
  (`invalidateblock`, manual rollback), the index now rewinds to the common
  ancestor instead of staying on the orphaned tip. Longer-chain reorgs keep
  using the undo window.
- **Mempool-aware history**: `blockchain.scripthash.get_history` appends
  unconfirmed transactions (`height` 0, or -1 when a parent is unconfirmed).
- **Correct balance semantics**: `confirmed` ignores mempool events and
  `unconfirmed` carries the net delta; previously a mempool spend was subtracted
  twice.
- **Chained mempool height**: `get_mempool`/`get_history` report `-1` for
  transactions with unconfirmed parents.
- **Checkpoint proofs**: `blockchain.block.header` and `blockchain.block.headers`
  accept `cp_height` and return the header merkle `root` plus `branch`.
- E2E additions: transaction merkle proof against the header root, checkpoint
  proof over the whole header chain, sender balance semantics, and a full
  reorg cycle (confirm -> invalidate -> rollback -> re-mine).

## 0.1.0-alpha.9 — 2026-11-02

- **Offline startup**: `e-lite run` now starts while Core is down. The database
  identity is trusted from disk, the Electrum/metrics ports bind, and confirmed
  history/balances are served from the cached index (`CORE_UNAVAILABLE`).
- **Lazy Core cookie**: construction no longer fails when the cookie file is
  missing (Core deletes it on shutdown); the cookie is loaded on the first
  request and refreshed on 401.
- Disk backpressure is scoped to `pruned-ibd`, so a full-core node is never
  paused by e-lite.
- Resilience E2E now restarts e-lite while Core is down and verifies 101 history
  entries, then recovery and SIGTERM exit 0.

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
