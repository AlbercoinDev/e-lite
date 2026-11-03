# Code audit

**Base:** `0.1.0-alpha.15` → published `0.1.0-alpha.16` · **Scope:** `bin/e-lite`, `crates/*` (25 Rust files,
~15.9k lines) · **Status:** findings fixed in `0.1.0-alpha.16` where noted.

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
| AUD-006 | P2 | Public listener | `bin/e-lite/src/main.rs` | `listen` could be set to `0.0.0.0`/LAN without TLS or auth; only documentation warned. | Startup warning for Electrum and metrics listeners that are not loopback. |
| AUD-007 | P1 | LXC memory | `bin/e-lite/src/tuning.rs` | alpha.14 raised readers to 28 on low read latency and used host `MemTotal`, causing OOM kills in LXC. | Reader caps 4/8/16, cgroup v2/v1 memory accounting, memory-pressure backoff and explicit overrides. |

## Residual risks and intentional limits

| ID | Sev. | Area | Risk | Current mitigation |
|---|---|---|---|---|
| AUD-010 | P1 | Electrum exposure | No TLS, authentication or per-request rate limit. A non-loopback listener is plain TCP. | Loopback default, startup warning; use SSH/VPN/reverse proxy; TLS is roadmap. |
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
