# Code audit

**Base:** `0.1.0-alpha.29.4.5` → current release · **Scope:** `bin/e-lite`,
`crates/*`, snapshots, the Electrum server and the external path.
**Status:** open findings and residual risks as of `0.1.0-alpha.29.17.0`.

This audit is not a claim of formal verification. It records static checks,
manual review areas, concrete findings and residual risks. Fixed findings
(AUD-001..167) are in [`audit/FIXED.md`](audit/FIXED.md).

## Baseline

```text
cargo fmt --check                                      OK
cargo clippy --workspace --all-targets -- -D warnings  OK
cargo test --workspace                                 350 tests OK
scripts/check-docs.sh                                  OK
tests/e2e/e2e_launch_core.sh                           OK
tests/e2e/e2e_pruned_ibd.sh                            OK
tests/e2e/e2e_regtest.sh                               OK
tests/e2e/e2e_defaults.sh                              OK
tests/e2e/e2e_snapshot.sh                              OK
tests/e2e/e2e_recovery.sh                              OK
tests/e2e/e2e_multi_instance.sh                        OK
E_LITE_REAL_TOR=... managed_tor_real_binary_creates_onion (ignored by default) OK
install.sh --with-tor / --without-tor                  OK
```

Additional checks performed:

- grep of `unsafe`, `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!`,
  filesystem writes and process spawning;
- review of the Electrum request path, client limits, snapshot import, Core RPC,
  rawtx external sources, pruning state machine and storage commit path;
- review of adaptive tuning and cgroup/LXC memory handling (fixed in alpha.15);
- the 29.15.0 wallet path (AUD-167) and the external-path race (AUD-160).

## Open findings

| ID | Sev. | Area | Location | Finding | Proposed fix / status |
|---|---|---|---|---|---|
| AUD-130 | P1 | Disk footprint | `crates/el-model/src/codec.rs` (`history_key` 40 B + 18 B value, `occurrence_key` 40 B, `utxo_by_script_key` 68 B), `crates/el-store/src/lib.rs` (LZ4 on every level), `docs/TUNING.md:132` | The mainnet index is documented at ~260 GiB (total with pruned Core ~300 GB), which dominates the minimal-disk goal. The structure is right for a pruned node (the index must answer without blocks, unlike electrs, which keeps 8-byte prefixes and re-reads each block from an unpruned bitcoind: ~802 GB of blocks + 56 GB of index per its `doc/usage.md`; Fulcrum: ~133 GB + full node with `txindex`), but it is oversized: full 32-byte hashes everywhere, the txid stored twice (`tx_occurrences` and `txids.dat`), and an 18-byte history value of which responses use nothing (`received_sat` and `flags` are never read; `spent_sat` only as `!= 0` in `el-serve` `outpoint_status`). | Analysis 2026-11-14 (code reading; sizes per row before compression). Proposed schema v3: `txnum` (u40) as the transaction reference everywhere, height/position from an in-memory `txnum_base` vector; `history` = scripthash[0..16] + txnum, 1-byte flags (58 -> 22 B/row); `tx_occurrences` = txid[0..8] + txnum, every candidate checked against `txids.dat` (40 -> 13 B, also handles BIP30); `utxo` keeps the full 36-byte outpoint key (spends must be exact; a prefix could collide) with a trimmed value (81 -> ~66 B); `utxo_by_script` = scripthash[0..16] + txnum + varint vout (81 -> ~31 B); zstd on the bottommost level. Estimate: index ~260 GiB -> ~130-150 GiB, total ~150-160 GB. Prerequisite: measure per-CF SST size (`/status.rocksdb.cf`) and row counts on the owner's node and reindex testnet4 with both schemas; go only if it saves >= 35 %. | open - deferred by the owner (2026-11-14): worth doing, not urgent; do it during the alpha (a mandatory reindex costs more with more users). From-zero reindex, no in-place migration (decision 2026-11-13). |
| AUD-140 | P2 | Snapshot export | `bin/e-lite/src/snapshot.rs`, `main.rs` | Export still requires the DB lock (cannot run while `run` holds it); an online RocksDB checkpoint is not implemented. | Open: online export via a control endpoint. Digest/dedup/gzip work landed in 29.12.0. |
| AUD-143 | P3 | Snapshot details | `bin/e-lite/src/snapshot.rs` | Core file hashes are on by default, but there is no incremental backup and no option to carry the TLS/onion identity, so a restore changes the onion address and needs a new trust import. | Open: incremental backups and an identity-carrying option. |
| AUD-166 | P3 | Merkle proof cost | `crates/el-serve/src/lib.rs` | `get_merkle` reuses the 64-block txid ring, but a script-status cache (`subscribe` + `get_history` scan twice) is not implemented. | Open: measured 189-257 µs per history, not worth the invalidation risk yet. |

## Residual risks and intentional limits

| ID | Sev. | Area | Risk | Current mitigation |
|---|---|---|---|---|
| AUD-010 | P1 | Electrum exposure | No client authentication; a non-loopback listener accepts any client that can reach the port, and TLS only encrypts the transport. | Loopback default, startup warning, optional TLS with certificate reload, per-client token bucket (alpha.28; auto on non-loopback), `max_clients`; use firewall/IP allowlist/SSH/VPN/Tor. Wallet-compatible authentication is not planned (Electrum has no standard auth). |
| AUD-011 | P1 | Core RPC | HTTP only, no TLS. Credentials/cookie traverse the connection in clear text. | Loopback default; LAN/WAN requires SSH/VPN; documented. |
| AUD-012 | P2 | External rawtx | A provider can withhold data; with the per-txid default it learns which txids a load touches (hidden among decoy wallets), with `preset = "max"` it learns block hashes. | Per-txid with decoy wallets or whole blocks only (`max`), random source order, Tor by default, validation against the index and cache; `use_tor = false` is an explicit opt-out. |
| AUD-013 | P2 | Silent payments | BIP352 scan key/indexer not implemented; only the opt-in gate exists. | Methods return an explicit error; feature not advertised. |
| AUD-014 | P2 | Snapshot | Import trusts a local operator-provided archive; no encryption/signature. | Path/type validation, manifest, genesis/network/schema/tip/digest checks. |
| AUD-015 | P2 | Electrum concurrency | One OS thread per client; a client can still consume CPU with expensive queries within its budget. | `max_clients`, read/write timeouts, 1 MiB request cap, per-client token bucket, 1,000 outpoint subscriptions. |
| AUD-090 | P2 | Cross-container sync | The per-user sync lock is a local file lock; two containers (for example Umbrel apps) with different mount namespaces do not see it and can still run two intensive syncs. | Documented in TUNING.md; a real fix needs a shared lock medium or an orchestrator-level policy. |
| AUD-016 | P3 | Code layout | `bin/e-lite/src/main.rs` still holds CLI, indexer, disk guard, metrics wiring and helpers. | `tuning`, `config`, `snapshot`, `core_supervisor` already split; further extraction is refactor-only, not required for correctness. |
| AUD-017 | P3 | Docs drift | Historically `prune=550`, `lag 512`, snapshots and TLS claims appeared inconsistently. | Documentation rewritten in alpha.16; `AGENTS.md`/`docs/PROJECT_MEMORY.md` enforce consistency. |
| AUD-040 | P3 | Fee estimate | `blockchain.estimatefee` returns a relayfee/mempool fallback instead of `-1` when Core has no estimate; the protocol allows `-1`. | Intentional compatibility choice: some wallets (Sparrow) treat `-1` as a send failure. Documented in `docs/FEATURES.md`; revisit if wallets handle `-1` correctly. |
| AUD-041 | P3 | Version negotiation | The spec says to close when client and server ranges do not intersect. e-lite instead downgrades to the newest implemented version at or below the client maximum, so a client requesting `1.4.3`/`1.5` connects on `1.4.2` and `1.7` on `1.6`. | Deliberate leniency for wallet compatibility; the negotiated version is returned so a compliant client adapts. Only a client maximum below 1.4 is refused. Tests cover the matrix; documented in `AGENTS.md`/`FEATURES.md`. |

| AUD-091 | P1 | Throughput | `bin/e-lite/src/main.rs` `catch_up_limited` returned every `poll_seconds` (10 s), joined readers and discarded prepared blocks. At 3.3 blocks/s the in-flight window was a large share of each call. | Removed the deadline; the supervisor now runs at commit boundaries and the 512-block checkpoint remains. `/status.pipeline.discarded` is the regression signal; `tests/e2e/e2e_pruned_ibd.sh` asserts <5 % waste. |
| AUD-092 | P1 | Throughput | `bin/e-lite/src/main.rs` stored the current lag as `bulk_lag_baseline` on every resume, raising the next pause threshold by `max_bulk_lag_blocks/2` per cycle (14,884 → 24,858 → 34,858 in the 29.5.0 log). | A normal resume no longer rewrites the baseline; only a full-pause prewarm anchors it. Optional `E_LITE_LAG_POLICY=drain` uses the last commit cycle's rate; opt-in pending a mainnet A/B. |
| AUD-093 | P2 | Prune stall | `flush_all` forced all seven memtables to SST before every `pruneblockchain`, adding seconds to each prune. Durability only needs the WAL. | `SyncWAL(true)` replaces it. A `WriteBatch` is atomic across CFs and `txids.dat` is fsynced first; a failed barrier skips the prune. Covered by prune + SIGKILL + restart in `tests/e2e/e2e_pruned_ibd.sh`. |
| AUD-094 | P2 | Metrics/control | `property_int_value` reads the default CF for per-CF properties; `commit_share` divided by the configured interval; two `maybe_adapt` call sites could pass the 30 s gate concurrently; cache grew on free RAM alone. | Per-CF sums and DB-wide reads classified; `commit_cycle_ms` is the denominator; one caller plus `adapt_gate`; cache growth requires prefetch pressure; Bulk/Live hysteresis. `/status.rocksdb.properties_ok` marks unknown values. |
| AUD-095 | P3 | Tor noise | A managed Core kept `-listenonion=1 -torcontrol=127.0.0.1:9051` although the managed `torrc` has no `ControlPort`, logging connection errors forever. | Managed Core gets `-listenonion=0` unless the operator overrides it; `/status.tor` exposes the effective SOCKS port and `control_port: 0`; `doctor` explains/warns. Core's own onion is documented as not managed. |

| AUD-096 | P1 | CPU oversubscription | `tuning.rs`/`core_plan.rs` used `available_parallelism()` (logical SMT) for `-par`, readers and RocksDB jobs. A 4C/8T i7-6700T got `par=7`, `readers=8`, `jobs=8` plus e-lite's apply/commit threads. | `MachinePlan` counts physical `(physical id, core id)` pairs and applies cgroup CPU quota. 4C/8T now gets `par=3`, `rpcthreads=4`, `readers=4`, `jobs=2`. `/status.machine` and `doctor` print the plan. |
| AUD-097 | P1 | Commit overhead | Bulk commits were driven by a short timer/small batch (40-75 blocks, 2.4-8.2 s, 30-40 % of wall in 29.6.0). | Bulk bounds are 250k tx / 512 blocks / 20-60 s by disk, with a minimum of 8 blocks or 60 s; live is 1.5 s. Safe because the prune gate uses the committed tip. |
| AUD-098 | P2 | Prune stalls | `pruneblockchain` used the 30 s RPC read timeout; a slow prune returned `EAGAIN` (os error 11) and blocked the apply thread for 35-75 s. | Dedicated 120 s prune timeout + `el-prune` background thread. The WAL barrier still runs before the request; the supervisor collects the result at the next tick. |
| AUD-099 | P2 | Write stalls | Memtables were split evenly across seven CFs and `/status` only showed the sum, hiding which family caused compaction stalls. | Weighted memtables (`utxo` 6, `history`/`utxo_by_script` 4) under the shared WBM; `/status.rocksdb.cf` exposes per-family pending compaction/memtable/SST/table readers. |

| AUD-100 | P2 | RPC/commit/prune completion | 29.7.0 had a 120 s prune timeout but no retry on transient EAGAIN; bulk commits had tx/block/time bounds but no byte bound; the prune RPC could start without an explicit commit-boundary guarantee. | One 150 ms retry for socket timeout/EAGAIN/reset; raw-byte group bound `clamp(RAM/128, 64-256 MiB)`; `run_deferred_prune` starts only at the top of an apply iteration after the commit/WAL barrier. Unit tests + prune/recovery e2e. |
| AUD-101 | P1 | Bulk scheduler | `pruning.max_bulk_lag_blocks`/`core_resume_ahead_blocks` compared block counts across heights. 20k blocks are ~10 GiB at 100k and ~30 GiB at 800k, so the guard was not a disk bound and Core/disk contention returned as blocks grew. | Byte watermarks `max_bulk_lag_gib` (30) / `core_resume_ahead_gib` (7) over `lag × EWMA(raw block size)`; `/status.scheduler.backlog_bytes` exposes the estimate. Legacy blocks apply only when the GiB value is 0. Unit test `byte_backlog_is_height_independent`. |
| AUD-102 | P2 | Commit controller | The controller divided the commit EWMA by a cycle polluted by 1-block boundary commits (share 76-118 %), shrank the batch when the real limit was `Txs`, and shrank again on I/O-bound commits. | Controller reads only the last full group: reason, blocks, txs, bytes, cycle and `txids`/db split. `Txs`-limited and I/O-bound groups leave the batch alone; only a block-limited group with share >75 % shrinks. Boundary/shutdown partials are excluded from EWMA/cycle. Tests `boundary_commit_does_not_feed_the_controller`, `reader_changes_require_two_samples`. |
| AUD-103 | P2 | Reader controller | A high `read_ms` reduced readers from 4 to 2 even when `wait_ms` was high (pipeline starved) and a single sample could flip the pool. | Growth requires `wait_ms > 50` + prefetch pressure; reduction requires low `wait_ms`; two consecutive samples; floor `max(physical/2, 2)`. |
| AUD-104 | P3 | Disk detection | `same_device` compared only `device`, so bind mounts, `/data` layouts and unresolved `major:minor` reported `same=false` even on one disk. | `DiskInfo` keeps `major:minor` and mount source; `same_device` falls back through `dev`, parent device, mount source and mount point. Test `disk_identity_prefers_kernel_device_number`. |
| AUD-105 | P2 | Scheduler contention | e-lite and Core always downloaded/indexed simultaneously; no supported way to test strict alternation without I/O contention. | `scheduler = "overlap"` (default) or `"alternate"` (opt-in, requires `prune_mib = 1` and `pause_core_during_bulk = true`); serving continues while indexing is paused. `/status.scheduler` exposes mode/index_paused/watermarks. Default stays `overlap` until a measured Core-solo rate beats the ~6.7 blocks/s break-even. |
| AUD-106 | P1 | Memory budget | `budget_with` received `available_mib` but derived reserve, ceiling, memtables, runtime, cache start and page floor from installed RAM. On 8/16 GiB Umbrel hosts with other services this planned 7 GiB of e-lite memory even when only a few GiB were free, forcing the cache to oscillate. | `effective = min(total, MemAvailable - core_reserve)`; all components derive from it. Profile cache ceiling `total/4` for local SSD/unknown (2 GiB at 8, 4 at 16, 8 at 32), capped by the effective room; memtables `effective/16`; `-dbcache = min(total/8, available/2, 4096)`. Tests for the 8 GiB and 16 GiB cases. |
| AUD-107 | P1 | Cache oscillation | The shrink floor added `reserve + page_cache_floor`, double-counting reclaimable page cache; `shrink_for_core_resume` computed `total/4`, which was clamped to the current maximum and never shrank. The cache moved 2048↔3997 MiB every few minutes. | Shrink when `cache > dynamic_ceiling` or `MemAvailable < reserve`; grow only with `clamp(effective/8,256 MiB,2 GiB)` margin and slow prefetch; two samples plus 5-minute reversal dwell; fast path below 1 GiB; pre-resume shrink only if `available < reserve + cache/4`. Tests `cache_action_follows_available_and_stays_stable`. |
| AUD-108 | P2 | Commit controller | `batch_blocks` could grow to 4096 although the catch-up checkpoint is 512; groups were 47-104 blocks (Time/Txs/Bytes-bound), so `batch -> 1152` changed nothing. | Cap at 512. Grow the batch only for a `Blocks` reason; `Time` may grow the interval; `Txs`/`Bytes` leave both; log the reason. Unit test asserts `batch <= 512`. |
| AUD-109 | P2 | Byte backlog estimate | The 1/8 EWMA of raw block size moved from ~2.4 MB to ~0.67 MB between pause and resume, changing a 13.5k-block lag from 31 GiB to 6 GiB. | 1/1024 EWMA (~1000 blocks). A short burst of 4 MB blocks moves the watermark <1 % (`long_block_average_ignores_a_short_burst`). |
| AUD-110 | P1 | Core reserve | 29.8.1 subtracted the fitted `-dbcache` whenever Core was managed, even if it was already running; `MemAvailable` already excluded it. The docs claimed otherwise. | `run`/`index` probe the RPC port before the plan; already-running/external Core returns 0; `serve` reserves 0; operator `-dbcache` is parsed and used. `MachinePlan::detect_with_core_reserve`. |
| AUD-111 | P1 | Intensive-sync lock | `IbdLock` was acquired after the first `catch_up_limited` call; a 512-block call at 1-2 blocks/s left a ~9 minute window where a second instance could index. | Acquire the lock before the first call when Core is in IBD or lag > 5,000. Logs `intensive-sync slot acquired` at the start. |
| AUD-112 | P2 | Cache control signal | Cache growth used the 1/8 `prefetch_ms` EWMA, dominated by one 10-15 s block; it could grow on a tail outlier. | Keep 256 recent samples; `memory_step` uses p95. `/status.tuning` and the progress line expose p50/p95/max for prefetch and wait; test `percentiles_are_taken_from_recent_samples`. |
| AUD-113 | P2 | Byte phase flapping | The byte watermarks acted on one `lag_guard` sample, so a single large-block burst could pause/resume Core. | 10 s dwell for both byte thresholds; `reader_pressure` still pauses immediately. |
| AUD-114 | P2 | Diagnostics | The log still did not show disk fstype/source, ARC/SReclaimable/Shmem/RSS, phase rate, p95, commit group/reason or RocksDB compaction; an unknown disk could not be diagnosed without shell access. | Machine plan prints fstype/source/device; memory prints SReclaimable/Shmem/ARC/RSS; `/status.scheduler` has avg block bytes/phase/rate; progress has p50/p95/max, phase rate, commit group/reason; `rocksdb:` line every 10 min; unknown disk warns. |
| AUD-115 | P2 | Alternate scheduler | `scheduler = "alternate"` was advertised even when Core could not be paused (external/already running); `lag_guard` returned early and the mode did nothing. | `Indexer::new` disables it and logs a warning; `/status.scheduler.mode` reflects the effective mode. |
| AUD-116 | P2 | Hot memtables | Memtables were only resized by the critical one-way halving; no normal hot grow/shrink, and WBM-only resize could not change per-CF limits. | Opt-in `adaptive_memtables`: WBM + per-CF `write_buffer_size` from `effective/16`, 10 min persistence/cooldown, cache priority, explicit `write_buffer_mib` locks. `hot_memtable_resize_is_applied` opens a real RocksDB. |
| AUD-117 | P2 | Prefetch tail | A reader could spend 10-15 s on one block's prevouts; no way to cap it without changing the apply fallback. | Opt-in `prefetch_deadline_ms`: 512-key chunks, stop after deadline; apply already falls back. Config validation 0/50..60000. |
| AUD-118 | P3 | Core I/O contention | No supported way to deprioritise Core's block writes against e-lite's random UTXO reads. | Opt-in `core.io_priority` best-effort `ionice` for launched/adopted Core; unavailable/ignored is a warning. `core.cgroup_io_weight` additionally creates an `e-lite-core` cgroup v2 child and sets `io.weight`, best-effort. |
| AUD-119 | P2 | ZFS/overlay disk | ZFS/overlay mounts were classified only as `unknown`; the log did not distinguish them or expose ARC. | `DiskKind::Zfs`/`Overlay`, registry from fstype when there is no block device, startup warning with fstype/source, ARC in the memory line. |
| AUD-120 | P2 | Memtable accounting | `weighted_write_buffer`/`per_cf_write_buffer_bytes` capped every CF at 64 MiB, so a 2 GiB `memtables_mib` could not be reached (7 CFs × 64 MiB). The log promised memory the store did not use. | Removed the per-CF cap; the shared `WriteBufferManager` is the total cap and per-CF sizes are flush thresholds. The hot-resize test opens a real RocksDB. |
| AUD-121 | P1 | Cache arithmetic | `dynamic_cache_max` added planned memtables/runtime to `MemAvailable` and then subtracted them again; `MemAvailable` already excludes e-lite's resident memory. On a 32 GiB host with 2-5 GiB available, the hot cache collapsed from 1288 MiB to 8-30 MiB (`block_cache = 0`), leaving the data cache disabled while table readers grew to 4-6 GiB. | `cache_dyn = min(profile, MemAvailable - reserve_dyn)` with `reserve_dyn = min(startup_reserve, max(1024 MiB, MemAvailable/2))`. Dynamic reserve is tested with the exact log sample. `/status.memory` and the hot log expose planned memtables vs actual RocksDB memtable and table-reader memory; `[index] max_open_files` controls table readers. |
| AUD-122 | P2 | Prune deferral | `prune_defer_free_gib = 20` deferred *all* normal pruning while free space stayed above the margin. On a ZFS host with ~170 GiB free, a from-zero mainnet IBD left Core holding ~89k already-indexed blocks (~140 GiB) with `pruneheight` frozen at 886146 for days: the safe cut (index − 288) was ~90k blocks ahead and `min_gain` (5,000) was already satisfied — the deferral was the only blocker. Evidence: mainnet console log 2026-11-11 (`prune 886146` constant, milestone `core 145.4 GiB`, `free 171.4 GiB`). | Once Core is out of IBD **and** the index matches Core's tip, the free-space deferral is bypassed: the gate prunes to the safe maximum on the next tick, then `min_gain` batches the live trickle again. Durability unchanged: durable tip, WAL barrier, safe-height cap, retry-after-backoff. Tests: `caught_up_prune_bypasses_the_free_space_deferral`, `caught_up_live_mode_batches_after_the_transition_prune`; the IBD deferral test still passes. |
| AUD-123 | P1 | Catch-up tail | With fewer blocks left than the commit minimum, every 500 ms catch-up call rebuilt the same 2-block target list and the end-of-call commit could be skipped: Ok without progress, no log line, no `READY`, forever (mainnet: 85+ min at `lag 2` with "(100.0%)" on screen). | A clean end-of-call always commits the applied prefix (undo is written per block); the reader wait is bounded (10 s + `debug:`); readers return their permit at the list end; `debug: pipeline:` traces each call with a rate-limited no-progress note. `sync.ready` requires Core out of IBD; the height percentage caps at 99.9 % while blocks remain. |
| AUD-124 | P1 | Snapshots | `snapshot.rs::add_tree` walked the whole data dir and never excluded the nested Core datadir (`<data-dir>/bitcoin`), so an "index" `export-snapshot` silently packed the entire Core chain data (hundreds of GB on mainnet, including a `chainstate/` LevelDB copied without a consistency guarantee), while `disk_usage_bytes` (el-store) explicitly excludes that same directory. | Index snapshots always exclude `bitcoin/`. The full-backup path is explicit: `--with-core` refuses while a Core is running, probes it (start + RPC + stop) and packs the datadir with a manifest v2 section (tip, prune height, version, per-file size/SHA-256); import validates the pair (`index tip <= Core tip`, `prune_height <= index tip - 288`, sizes/hashes, `--verify=full` bounded `verifychain`). GNU sparse `blk*.dat` entries are accepted as regular files. Tests: `snapshot.rs` unit tests + `tests/e2e/e2e_snapshot.sh` (refusal while Core runs, index-only without `bitcoin/`, full export/import with `--hash-files`, second import rejected, `--verify=full` OK). |
| AUD-125 | P2 | Snapshot restore | The undo window is only valid if Core's prune cut stays at or below `index tip - 288`; a hand-mixed index/datadir pair could restore with a deeper cut, and a deep Core rollback would then hit `NEEDS_RECOVERY`. | Manifest v2 records Core tip and prune height; `validate_core_coherence` rejects `index tip > Core tip` and `prune_height + 288 > index tip` on export and import. Unit tests cover all three rules; `--verify=full` re-checks the restored tip and runs `verifychain(2, 288)`. |


## Reviewed and considered correct

- Commit ordering: `txids.dat` fsync happens before the RocksDB batch that
  advances `committed_tip`; startup truncates extra txids and rejects short
  files.
- Undo/reorg: rewind is bounded by `reorg_window`; deeper reorgs fail loudly.
- Prune safety: manual prune requests are capped by committed tip, readers and
  Core's `tip - 288`; auto-prune requires explicit opt-in and a 224-block guard.
  The pre-prune barrier is `SyncWAL(true)`; it must succeed or the prune is
  skipped. The prune + SIGKILL recovery test verifies the full history.
- Merkle/TXID/witness: rawtx blocks are validated against indexed headers,
  merkle roots and witness commitments before use.
- Prune-gap repair: only verified cache/P2P/Esplora blocks by hash; no snapshot
  requirement.
- Adaptive tuning: one change per 30 s under a single `adapt_gate`, memory/disk
  guards take precedence, explicit knobs are locked, and cache growth requires
  measured prefetch pressure. The machine plan sets physical-core and disk
  defaults; the cache shrinks in 12.5 % steps with an explicit page-cache
  floor.
- `unsafe`: only `statvfs` and `kill` wrappers in `el-chain`/`core_supervisor`,
  with checked return values.


## How to reproduce

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
tests/e2e/e2e_launch_core.sh
tests/e2e/e2e_pruned_ibd.sh
tests/e2e/e2e_regtest.sh
```

For LXC/OOM checks:

```bash
dmesg -T | grep -i -E 'oom|killed process'
cat /sys/fs/cgroup/memory.max /sys/fs/cgroup/memory.current
curl -s http://127.0.0.1:9184/status | python3 -m json.tool | grep -A20 '"tuning"'
```
