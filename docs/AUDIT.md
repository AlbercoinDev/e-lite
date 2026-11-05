# Code audit

**Base:** `0.1.0-alpha.21` → published `0.1.0-alpha.22` · **Scope:** `bin/e-lite`, `crates/*` (25 Rust files,
~17.2k lines) · **Status:** findings fixed in `0.1.0-alpha.22` where noted.

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

## Residual risks and intentional limits

| ID | Sev. | Area | Risk | Current mitigation |
|---|---|---|---|---|
| AUD-010 | P1 | Electrum exposure | No client authentication or per-request rate limit. A non-loopback listener accepts any client that can reach the port; TLS only encrypts the transport. | Loopback default, startup warning, optional TLS with certificate reload; use firewall/IP allowlist/SSH/VPN/Tor. Rate limiting and wallet-compatible authentication remain roadmap. |
| AUD-011 | P1 | Core RPC | HTTP only, no TLS. Credentials/cookie traverse the connection in clear text. | Loopback default; LAN/WAN requires SSH/VPN; documented. |
| AUD-012 | P2 | External rawtx | Provider can withhold blocks; provider learns the set of block hashes fetched. | Whole blocks only, decoys, random source order, Tor by default, validation and cache. |
| AUD-013 | P2 | Silent payments | BIP352 scan key/indexer not implemented; only the opt-in gate exists. | Methods return an explicit error; feature not advertised. |
| AUD-014 | P2 | Snapshot | Import trusts a local operator-provided archive; no encryption/signature. | Path/type validation, manifest, genesis/network/schema/tip/digest checks. |
| AUD-015 | P2 | Electrum concurrency | One OS thread per client; no per-client token bucket. | `max_clients`, read/write timeouts, 1 MiB request cap. |
| AUD-016 | P3 | Code layout | `bin/e-lite/src/main.rs` still holds CLI, indexer, disk guard, metrics wiring and helpers. | `tuning`, `config`, `snapshot`, `core_supervisor` already split; further extraction is refactor-only, not required for correctness. |
| AUD-017 | P3 | Docs drift | Historically `prune=550`, `lag 512`, snapshots and TLS claims appeared inconsistently. | Documentation rewritten in alpha.16; `AGENTS.md`/`docs/PROJECT_MEMORY.md` enforce consistency. |

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
