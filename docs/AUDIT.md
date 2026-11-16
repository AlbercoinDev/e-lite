# Code audit

**Base:** `0.1.0-alpha.29.4.5` → current release · **Scope:** `bin/e-lite`,
`crates/*`, snapshots, the Electrum server and the external path.
**Status:** open findings and residual risks as of the 2026-11-15 review of
`0.1.0-alpha.29.17.0`, released as `0.1.0-alpha.29.18.0`.

This audit is not a claim of formal verification. It records static checks,
manual review areas, concrete findings and residual risks. Fixed findings
(AUD-001..173, 180, 181) are in [`audit/FIXED.md`](audit/FIXED.md). The review itself, in
Spanish, is [`ANALISIS.md`](ANALISIS.md).

## Baseline

```text
cargo fmt --check                                      OK
cargo clippy --workspace --all-targets -- -D warnings  OK
cargo test --workspace                                 373 tests OK (6 ignored)
tests/run-e2e.sh --release launch_core regtest          OK
scripts/check-docs.sh                                  OK
tests/e2e/e2e_regtest.sh                               OK
tests/e2e/e2e_defaults.sh                              OK
tests/e2e/e2e_launch_core.sh                           OK
tests/e2e/e2e_wait_core.sh                             OK
tests/e2e/e2e_external_pruned.sh                       OK
tests/e2e/e2e_resilience.sh                            OK
tests/e2e/e2e_snapshot.sh                              OK
tests/e2e/e2e_multi_instance.sh                        OK
tests/e2e/e2e_pruned_ibd.sh                            OK
tests/e2e/e2e_recovery.sh                              OK
```

Run on the review branch on 2026-11-16, with the automatic restart on;
no process was left behind. Not run: `shellcheck` (not installed
on the review machine), `e2e_networks.sh` (real signet/testnet4 sync; it
refuses to start without the free disk space it requires), the real-Tor test
and the installer.

Additional checks performed:

- grep of `unsafe`, `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!`,
  filesystem writes and process spawning;
- review of the Electrum request path, client limits, snapshot import, Core RPC,
  rawtx external sources, pruning state machine and storage commit path;
- review of adaptive tuning and cgroup/LXC memory handling (fixed in alpha.15);
- the 29.15.0 wallet path (AUD-167) and the external-path race (AUD-160);
- 2026-11-15: full read of the catch-up pipeline, the lag guard, the Electrum
  connection loop, the rawtx resolver entry points and snapshot export/import;
  count of `unwrap`/`expect`/`panic!` outside tests in the code that parses
  untrusted input (`el-serve`: 0; `el-rawtx`: 4, all guarded: two P2P
  `version` slices behind a length check, one TLS constant, one map entry
  inserted on the line above).

## Open findings

| ID | Sev. | Area | Location | Finding | Proposed fix / status |
|---|---|---|---|---|---|
| AUD-130 | P1 | Disk footprint | `crates/el-model/src/codec.rs` (`history_key` 40 B + 18 B value, `occurrence_key` 40 B, `script_utxo_key` 68 B), `crates/el-store/src/lib.rs` (LZ4 on every level), `docs/TUNING.md:132` | The mainnet index is documented at ~260 GiB (total with pruned Core ~300 GB), which dominates the minimal-disk goal. The structure is right for a pruned node (the index must answer without blocks, unlike electrs, which keeps 8-byte prefixes and re-reads each block from an unpruned bitcoind: ~802 GB of blocks + 56 GB of index per its `doc/usage.md`; Fulcrum: ~133 GB + full node with `txindex`), but it is oversized: full 32-byte hashes everywhere, the txid stored twice (`tx_occurrences` and `txids.dat`), and an 18-byte history value of which responses use nothing (`received_sat` and `flags` are never read; `spent_sat` only as `!= 0` in `el-serve` `outpoint_status`). | Analysis 2026-11-14 (code reading; sizes per row before compression). Proposed schema v3: `txnum` (u40) as the transaction reference everywhere, height/position from an in-memory `txnum_base` vector; `history` = scripthash[0..16] + txnum, 1-byte flags (58 -> 22 B/row); `tx_occurrences` = txid[0..8] + txnum, every candidate checked against `txids.dat` (40 -> 13 B, also handles BIP30); `utxo` keeps the full 36-byte outpoint key (spends must be exact; a prefix could collide) with a trimmed value (81 -> ~66 B); `utxo_by_script` = scripthash[0..16] + txnum + varint vout (81 -> ~31 B); zstd on the bottommost level. Estimate: index ~260 GiB -> ~130-150 GiB, total ~150-160 GB. Prerequisite: measure per-CF SST size (`/status.rocksdb.cf`) and row counts on the owner's node and reindex testnet4 with both schemas; go only if it saves >= 35 %. | open - deferred by the owner (2026-11-14): worth doing, not urgent; do it during the alpha (a mandatory reindex costs more with more users). From-zero reindex, no in-place migration (decision 2026-11-13). |
| AUD-140 | P2 | Snapshot export | `bin/e-lite/src/snapshot.rs`, `bin/e-lite/src/diagnostics.rs` | Export still requires the DB lock (cannot run while `run` holds it); an online RocksDB checkpoint is not implemented. | Open: online export via a control endpoint. Digest/dedup/gzip work landed in 29.12.0. |
| AUD-143 | P3 | Snapshot details | `bin/e-lite/src/snapshot.rs` | Core file hashes are on by default, but there is no incremental backup and no option to carry the TLS/onion identity, so a restore changes the onion address and needs a new trust import. | Open: incremental backups and an identity-carrying option. |
| AUD-166 | P3 | Merkle proof cost | `crates/el-serve/src/protocol.rs` (`transaction_merkle`, `block_txids_cached`) | `get_merkle` reuses the 64-block txid ring, but a script-status cache (`subscribe` + `get_history` scan twice) is not implemented. | Open: measured 189-257 µs per history, not worth the invalidation risk yet. |
| AUD-174 | P2 | Server threads | `crates/el-serve/src/server.rs` (`client_loop`, `process_batch`, `PIPELINE_DEPTH`, `BATCH_IO_WORKERS`) | One OS thread per connection, plus up to 4 background lines per connection, each of which may be a batch that spawns up to 32 scoped threads: ~130 threads for one busy wallet, created and destroyed per batch line. `max_clients` therefore bounds connections, not threads; the real ceiling under load is memory and the scheduler. Extends AUD-015. | Open: one bounded worker pool shared by all connections (requests queued per connection to keep fairness), sized from `MachinePlan`. Needs a wallet-load benchmark before and after; do not change the per-connection limits without it. |
| AUD-175 | P3 | Error handling | `bin/e-lite/src/main.rs` (`classify_error`), `bin/e-lite/src/pipeline.rs` (`index_loop`) | Whether an indexing error is fatal or retried forever is decided by searching the message for 13 substrings ("schema", "txids.dat", "needs_recovery", ...), and "Core is loading" by three more. Rewording a message, or an unrelated error that happens to contain "schema", silently changes the class: a fatal integrity error becomes an endless retry, or a transient RPC error stops the node. | Open: a typed error (`IndexFailure { Permanent, Transient, CoreLoading }`) built where the error is created (`el-index`, `el-store`, `el-rpc` already have their own enums); `classify_error` then matches a variant. A test per variant. |
| AUD-176 | P3 | Snapshot cost | `crates/el-store/src/lib.rs` (`canonical_digest`), `bin/e-lite/src/snapshot.rs` (`export`, `import_into`) | The digest that protects a snapshot iterates and hashes every row of every column family, single-threaded, once at export and again at import: on mainnet that is the whole index (~260 GiB logical) read and decompressed twice, on top of the tar pass. The cost grows with the chain. | Open: record a SHA-256 per stored file (the index section is already stored uncompressed, so it is one sequential pass that the tar walk can share) and keep the canonical digest for `--verify=full`. No format break: a new optional manifest table. |
| AUD-177 | P2 | Wallet latency during IBD | `bin/e-lite/src/pipeline.rs` (`commit_group`), `crates/el-serve/src/protocol.rs` (readers of `Shared::lock`) | The commit holds the global write lock for the whole `txids.dat` append + fsync and the RocksDB write. Wallet requests take the read side, so every request waits for the commit: milliseconds in live mode, seconds for a 512-block bulk group and tens of seconds in the spikes the log already warns about ("commit spike"). | Open: serve reads from a RocksDB snapshot taken at the committed tip (the batch is atomic and `txids.dat` is only read below the committed `txnum`), keeping the exclusive lock for rewinds only. Needs a reorg test under concurrent readers. |
| AUD-178 | P3 | Protocol negotiation | `crates/el-serve/src/protocol.rs` (`negotiate_protocol`), `docs/ROADMAP.md` | ROADMAP asked to honour `client_min` (a client asking `[1.4.3, 1.4.3]` gets 1.4.2), while AUD-041 records the same downgrade as deliberate leniency and the tests assert it. The two documents contradicted each other. | Not changed. Recommended rule: keep the leniency for a single version string (what wallets send) and honour `client_min` only for an explicit `[min, max]` range with `min < max`; decide before touching the tests. |
| AUD-179 | P3 | Pipeline waste | `bin/e-lite/src/pipeline.rs` (`catch_up_limited`, `spawn_readers`), `bin/e-lite/src/reader_pool.rs` | The reader pool lives for one call: every 512 blocks (and every 500 ms at the tip) the threads are created and joined and the prepared window (4-32 blocks already fetched, decoded and prefetched) is thrown away. With 8 readers that is up to 16 of every 528 prepared blocks. | Open: keep one `ReaderPool` across calls and feed it heights; the pool is already isolated for this. Measure with `tests/bench/bench_ibd.sh` and `/status.pipeline.discarded`. |

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
| AUD-017 | P3 | Docs drift | Historically `prune=550`, `lag 512`, snapshots and TLS claims appeared inconsistently. | Documentation rewritten in alpha.16; `AGENTS.md`/`docs/PROJECT_MEMORY.md` enforce consistency. |
| AUD-040 | P3 | Fee estimate | `blockchain.estimatefee` returns a relayfee/mempool fallback instead of `-1` when Core has no estimate; the protocol allows `-1`. | Intentional compatibility choice: some wallets (Sparrow) treat `-1` as a send failure. Documented in `docs/FEATURES.md`; revisit if wallets handle `-1` correctly. |
| AUD-041 | P3 | Version negotiation | The spec says to close when client and server ranges do not intersect. e-lite instead downgrades to the newest implemented version at or below the client maximum, so a client requesting `1.4.3`/`1.5` connects on `1.4.2` and `1.7` on `1.6`. | Deliberate leniency for wallet compatibility; the negotiated version is returned so a compliant client adapts. Only a client maximum below 1.4 is refused. Tests cover the matrix; documented in `AGENTS.md`/`FEATURES.md`. |

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
- `unsafe`: libc calls only, with checked return values: `statvfs`
  (`el-chain`, `tuning.rs`, `diagnostics.rs`), `getrlimit` (`el-store`),
  `kill` (`core_supervisor.rs`, `tor.rs`), `flock`/`getuid` (`ibd_lock.rs`),
  `pipe`/`dup`/`dup2`/`close` for the log tee (`logging.rs`), `localtime_r`
  (`main.rs`) and `geteuid`. None in the index, the Electrum server or the
  external-fetch path.
- Untrusted input: `el-serve` has no `unwrap`/`expect`/`panic!` outside tests;
  request lines are capped (1 MiB), batches at 1000 items, pongs at 4 MiB per
  connection, outpoint candidates at 64.

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
curl -s http://127.0.0.1:55084/status | python3 -m json.tool | grep -A20 '"tuning"'
```
