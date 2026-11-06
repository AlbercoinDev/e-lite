# Code audit

**Base:** `0.1.0-alpha.26.1` → published `0.1.0-alpha.27` · **Scope:** `bin/e-lite`, `crates/*` (26 Rust files,
~19k lines) · **Status:** findings fixed in `0.1.0-alpha.27` where noted.

This audit is not a claim of formal verification. It records static checks,
manual review areas, concrete findings and residual risks.

## Baseline

```text
cargo fmt --check                                      OK
cargo clippy --workspace --all-targets -- -D warnings  OK
cargo test --workspace                                 OK
tests/e2e_launch_core.sh                               OK
tests/e2e_pruned_ibd.sh                                OK
tests/e2e_regtest.sh                                   OK
E_LITE_REAL_TOR=... managed_tor_real_binary_creates_onion (ignored by default) OK
install.sh --with-tor / --without-tor                  OK
```

Additional checks performed:

- grep of `unsafe`, `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!`,
  filesystem writes and process spawning;
- review of the Electrum request path, client limits, snapshot import, Core RPC,
  rawtx external sources, pruning state machine and storage commit path;
- review of adaptive tuning and cgroup/LXC memory handling (fixed in alpha.15).

## Fixed findings

| ID | Sev. | Area | Location | Finding | Fix / evidence |
|---|---|---|---|---|---|
| AUD-001 | P1 | Electrum network | `crates/el-serve/src/lib.rs` | `read_until` could grow a per-connection buffer without bound until a newline; unauthenticated memory DoS. | `read_line_limited` with `MAX_REQUEST_BYTES` (1 MiB); unit test `request_line_is_bounded`. |
| AUD-002 | P1 | Electrum network | `start_server` | `max_clients` was checked before spawn but incremented inside the child; a burst could exceed the limit. | Atomic reservation before `try_clone`/spawn and decrement on every failure path. |
| AUD-003 | P1 | Snapshot import | `bin/e-lite/src/snapshot.rs` | A tar containing symlinks/hardlinks/device nodes could escape the target or create special files. | Reject every entry that is not a regular file or directory; regression test `import_rejects_symlink_entries`. |
| AUD-004 | P2 | Snapshot import | same | No archive size/entry-count limit; a malicious snapshot can exhaust disk. | Residual risk, documented. Import is operator-supplied, but a `snapshot_max_bytes` limit is a future hardening item. |
| AUD-005 | P2 | Rawtx providers | `crates/el-rawtx/src/esplora.rs` | Non-HTTP endpoints were accepted by config and failed later. | Skip endpoints not starting with `http://`/`https://`; test `refuses_non_http_endpoints`. |
| AUD-006 | P2 | Public listener | `bin/e-lite/src/main.rs` | `listen` could be set to `0.0.0.0`/LAN without TLS or auth; only documentation warned. | Startup warning for Electrum and metrics listeners that are not loopback; optional TLS since alpha.17 (AUD-018). |
| AUD-007 | P1 | LXC memory | `bin/e-lite/src/tuning.rs` | alpha.14 raised readers to 28 on low read latency and used host `MemTotal`, causing OOM kills in LXC. | Reader caps 4/8/16, cgroup v2/v1 memory accounting, memory-pressure backoff and explicit overrides. |
| AUD-018 | P1 | Electrum transport | `crates/el-serve/src/lib.rs` | Electrum could only be plain TCP; LAN/WAN traffic had no encryption in e-lite. | Optional TLS listener (rustls, TLS 1.2+) with PEM cert/key, 10 s handshake timeout, same 1 MiB line cap and mtime-based certificate reload. Tests `tls_listener_completes_handshake_and_serves`, `tls_watcher_reloads_changed_pem_and_keeps_old_on_error`, `e2e_regtest.sh`. |
| AUD-019 | P1 | Memory accounting | `bin/e-lite/src/tuning.rs` | `/proc/meminfo` was parsed with `strip_prefix` without consuming the colon, so `MemTotal`/`MemAvailable` always returned `None`. The daemon fell back to 1 GiB and 0 MiB available, shrank cache/readers and logged false `memory pressure (0 MiB)` on every machine. | Parse with `split_once(':')`, add tests, cgroup-aware available memory and reclaimable `inactive_file`; `doctor`/`/status` expose the values. |
| AUD-020 | P1 | RocksDB memory | `crates/el-store/src/lib.rs` | `write_buffer_bytes` was applied per column family: 7 CFs × 3 memtables × 64 MiB ≈ 1.3 GiB, plus the block cache. On low-RAM hosts this caused reclaim stalls and slow indexing. | Global memtable budget (`RAM/16`, 64 MiB..1 GiB) split across CFs, capped per CF; background jobs capped by CPU count; tests for the budget. |
| AUD-021 | P2 | Lifecycle | `bin/e-lite/src/main.rs`, `crates/el-serve/src/lib.rs` | P2P discovery/mempool started before Core; shutdown stopped Core while detached mempool/notification threads were still running, logging `Connection refused` and post-shutdown tuning lines. | `run` waits for Core RPC before starting dependants; `serve` stays offline; shutdown flag + joined threads before Core stop; e2e asserts clean startup/shutdown logs. |
| AUD-022 | P2 | Operational logging | `bin/e-lite/src/main.rs` | Console progress and errors were lost when running in the background; there was no application log (only Core's `bitcoind.log`). | Tee stdout/stderr to `<data-dir>/e-lite.log`, rotation, `--log-file`/`--log-max-mib`/`--no-log-file`; e2e checks the file. |
| AUD-023 | P2 | Prune churn | `bin/e-lite/src/config.rs`, `el-chain` supervisor | Manual pruning evaluated every 288 safe blocks, causing frequent prune I/O during a long catch-up. | Default `min_gain_blocks = 10000`; emergency low-disk pruning still bypasses it. |
| AUD-024 | P2 | Indexing throughput | `bin/e-lite/src/main.rs`, `crates/el-index` | Deserialize/hash and txid/Merkle work ran in the single apply thread, leaving reader threads idle and the second CPU core unused. | Reader threads prepare blocks (`prepare_block_bytes`), apply uses `apply_prepared`; block hash computed once; `decode_ms`/`wait_ms` exposed. |
| AUD-025 | P2 | Bulk lag guard | `bin/e-lite/src/main.rs` | Absolute `max_bulk_lag_blocks` paused Core immediately on a 300k backlog and kept it paused until lag ≤10k, freezing Core's own sync for hours. | Window is relative to the lag baseline and follows it; resumes at half the growth window. |
| AUD-026 | P1 | Bulk lag guard | `bin/e-lite/src/main.rs` | While paused, the relative baseline also followed the decreasing lag, so every indexed block lowered the resume threshold and Core resumed on the next check. The guard effectively never held. | `bulk_lag_window` takes `paused`; the baseline is frozen while paused and only follows the lag down while downloading. Unit test `bulk_lag_window_bounds_additional_growth`. |
| AUD-027 | P3 | Test reliability | `crates/el-store/src/lib.rs` | Global failpoint state was shared by tests running in parallel; `reopen_keeps_tip...` could fail when another test injected `BEFORE_COMMIT`. | Test mutex serializes failpoint/commit tests; repeated local runs and CI pass. |
| AUD-028 | P3 | Metrics | `bin/e-lite/src/tuning.rs`, `crates/el-store` | The cache-hit metric was always unknown, so the progress line showed a misleading `0%`/`?` and the controller could not use it. | Metric and cache-growth branch removed; `--cache-mib` and the block cache remain. |
| AUD-029 | P2 | Bulk scheduling | `bin/e-lite/src/main.rs` | Core could download during e-lite's bulk catch-up, competing for disk; pausing until the tip would reconnect peers late. | Default full pause plus `core_resume_ahead_blocks = 5000` pre-warm; relative 20k guard afterwards. |
| AUD-030 | P2 | Commit durability | `crates/el-store`, `bin/e-lite` | Two fsyncs per commit made bulk indexing slower than necessary. | `commit_sync=auto`: `txids` in bulk (txids.dat still fsynced, startup truncates extras), `always` live. |
| AUD-031 | P3 | Readiness/banner | `crates/el-serve` | Wallets had no clear READY signal and a static banner. | Dynamic `READY`/`FULLY SYNCED` log/status/metrics and INDEXING/READY banner templates, optional file with placeholders. |
| AUD-032 | P2 | Indexing throughput | `crates/el-index`, `bin/e-lite`, `crates/el-store` | Mainnet alpha.22 logs showed `apply` growing from ~300 ms to >1.5 s per block with `read`/`decode`/`wait` near zero: every prevout multi-get ran serially in the single apply thread, and the apply thread was also the only consumer of the reader pipeline. | Reader threads now prefetch each block's prevouts (`prefetch_prevouts`) while apply works on the previous block; apply uses the overlay first and falls back to a point lookup, so speculative misses are harmless. RocksDB `atomic_flush` and a larger L1/compaction readahead reduce multi-CF write amplification. Oracle-parity tests cover the prefetch path; `e2e_regtest`, `e2e_pruned_ibd`, `e2e_launch_core` pass. |
| AUD-033 | P2 | Memory budget | `bin/e-lite/src/tuning.rs`, `bin/e-lite/src/main.rs` | The block cache was fixed at a profile value (`RAM/4`) and `cache_mib` was locked; the machine's real `MemAvailable` and page cache were only consulted below 512 MiB. On an 8 GiB host with Core, the cache could neither use spare RAM nor adapt to pressure. | New hot policy: 75 % ceiling, `max(1024, RAM/8)` reserve, memtables `RAM/8`, page-cache floor, 256 MiB steps/25 % shrink with hysteresis; `cache_mib` is a maximum; critical pressure overrides it; pre-resume shrink. Unit tests for the budget/actions and a status/log budget line. |
| AUD-034 | P1 | Inbound onion | `bin/e-lite/src/tor.rs`, `bin/e-lite/src/main.rs` | The new managed Tor publishes Electrum on an onion; like the plain listener it has no client authentication (rate limiting exists per client). A leaked address gives full query access. | Loopback-only targets, v3 onion keys 0700, explicit warning in logs/docs, `mode=off`/`--no-tor`, address treated as a secret. Residual risk documented in `AUD-010`. |
| AUD-035 | P2 | Installer supply chain | `install.sh`, `bin/e-lite/src/tor.rs` | The installer downloads a Tor expert bundle and e-lite later executes it; a compromised download would run as the user. | Pinned bundle version and SHA-256 in the installer; download from the Tor Project over TLS; fallback to `tor` in `PATH`; `doctor` shows the resolved binary. |
| AUD-036 | P1 | Onion with wildcard listener | `bin/e-lite/src/tor.rs` | Binding Electrum to `0.0.0.0` (a common LAN setup) made the onion refuse to publish (`non-loopback target`), so users saw no onion address. | Wildcard listeners are mapped to loopback targets (`0.0.0.0`→`127.0.0.1`, `[::]`→`[::1]`); explicit `target_plain`/`target_tls` overrides; effective ports/targets persisted in `tor/state` for accurate `onion show`. |
| AUD-037 | P1 | Managed Tor startup | `bin/e-lite/src/tor.rs` | The expert bundle's `tor` has no RPATH; on Debian 13.7 without `libevent-2.1.so.7` it exited 127 and e-lite retried forever, so no onion was created. | Launch Tor with `LD_LIBRARY_PATH` prepended with its own directory (inherited value preserved); print the last `tor.log` lines on unexpected exit; `doctor` runs `tor --version` with the same environment. Reproduced in a Debian 12 container (127 before, 0.4.9.15 after). |
| AUD-038 | P1 | Onion ports | `bin/e-lite/src/tor.rs`, `bin/e-lite/src/config.rs` | `onion_port`/`onion_tls_port` defaulted to 50001/50002, so a user with `--listen 51001 --tls-listen 51002` saw the onion on 50001/50002 and connected to the wrong port. | 0 now means "same as the Electrum listener"; effective ports are used in torrc, the startup log, `tor/state`, `/status` and `onion show`; explicit overrides still work. |
| AUD-039 | P2 | Electrum 1.4 conformance | `crates/el-serve/src/lib.rs`, `crates/el-serve/src/version.rs` | `server.version` compared versions as strings, returned the server maximum instead of the negotiated version, accepted repeated calls, never closed on incompatible ranges, and answered JSON-RPC notifications. `transaction.get(verbose)` lacked `blockhash`/`confirmations`/`time`, and `scripthash.unsubscribe` always returned `true`. | Numeric `ProtocolVersion` negotiation (1.4–1.4.2, first call only, close on no overlap), notification suppression, verbose tx decoration from the index, real unsubscribe result, `server.add_peer`, canonical mempool order. Tests in `version.rs`, `protocol.rs` and `e2e_sparrow.py`. |
| AUD-042 | P2 | Electrum 1.6 | `crates/el-serve/src/lib.rs`, `crates/el-rpc/src/lib.rs` | 1.6 clients expect `blockchain.block.headers` to return a `headers` array, plus `mempool.get_info`, `estimatefee(mode)` and `broadcast_package`. Serving the 1.4 shape or advertising 1.6 without those methods would break clients. | Version-aware headers shape; `server.version`-first cap for 1.6 (late negotiation stays 1.4.2); Core `getmempoolinfo` mapping; fee mode with per-mode cache; `submitpackage` wrapper with stable `{success, errors}` result. Tests: 1.6 negotiation/headers, protocol unit tests, real parent+child package in `tests/e2e_electrum16.py`. |
| AUD-043 | P2 | Electrum 1.7 | `crates/el-serve/src/lib.rs`, `crates/el-mempool/src/lib.rs` | 1.7 removes `hash_function`, changes `server.ping`, and adds `scriptpubkey.*`, `outpoint.*`, `mempool.recent` and `testmempoolaccept`. Outpoint status needs a spender lookup that must not slow the index apply path. | Additive methods exposed without version gating; features/ping version-aware. Outpoints resolved on demand: UTXO fast path, mempool reverse map, history scan limited to `spent_sat > 0`, `spk_hint` verified, raw tx only for candidates. Scriptpubkey reuses scripthash logic with `10001` and reorg notifications. No schema/CF/undo/digest changes; `listunspent` sorted. Tests in `protocol.rs` and `tests/e2e_electrum17.py`; e2e asserts `rawtx.fetches == 0`. |

## Residual risks and intentional limits

| ID | Sev. | Area | Risk | Current mitigation |
|---|---|---|---|---|
| AUD-010 | P1 | Electrum exposure | No client authentication; a non-loopback listener accepts any client that can reach the port, and TLS only encrypts the transport. | Loopback default, startup warning, optional TLS with certificate reload, per-client token bucket (alpha.28; auto on non-loopback), `max_clients`; use firewall/IP allowlist/SSH/VPN/Tor. Wallet-compatible authentication is not planned (Electrum has no standard auth). |
| AUD-011 | P1 | Core RPC | HTTP only, no TLS. Credentials/cookie traverse the connection in clear text. | Loopback default; LAN/WAN requires SSH/VPN; documented. |
| AUD-012 | P2 | External rawtx | Provider can withhold blocks; provider learns the set of block hashes fetched. | Whole blocks only, decoys, random source order, Tor by default, validation and cache. |
| AUD-013 | P2 | Silent payments | BIP352 scan key/indexer not implemented; only the opt-in gate exists. | Methods return an explicit error; feature not advertised. |
| AUD-014 | P2 | Snapshot | Import trusts a local operator-provided archive; no encryption/signature. | Path/type validation, manifest, genesis/network/schema/tip/digest checks. |
| AUD-015 | P2 | Electrum concurrency | One OS thread per client; a client can still consume CPU with expensive queries within its budget. | `max_clients`, read/write timeouts, 1 MiB request cap, per-client token bucket, 1,000 outpoint subscriptions. |
| AUD-016 | P3 | Code layout | `bin/e-lite/src/main.rs` still holds CLI, indexer, disk guard, metrics wiring and helpers. | `tuning`, `config`, `snapshot`, `core_supervisor` already split; further extraction is refactor-only, not required for correctness. |
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
- Merkle/TXID/witness: rawtx blocks are validated against indexed headers,
  merkle roots and witness commitments before use.
- Prune-gap repair: only verified cache/P2P/Esplora blocks by hash; no snapshot
  requirement.
- Adaptive tuning: one change per 30 s, memory/disk guards take precedence,
  explicit knobs are locked.
- `unsafe`: only `statvfs` and `kill` wrappers in `el-chain`/`core_supervisor`,
  with checked return values.

## How to reproduce

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
tests/e2e_launch_core.sh
tests/e2e_pruned_ibd.sh
tests/e2e_regtest.sh
```

For LXC/OOM checks:

```bash
dmesg -T | grep -i -E 'oom|killed process'
cat /sys/fs/cgroup/memory.max /sys/fs/cgroup/memory.current
curl -s http://127.0.0.1:9184/status | python3 -m json.tool | grep -A20 '"tuning"'
```
