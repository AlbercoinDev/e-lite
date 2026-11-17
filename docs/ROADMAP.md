# Roadmap and proposed improvements

Future work only. Completed changes are in [`CHANGELOG.md`](../CHANGELOG.md),
current behaviour in [`FEATURES.md`](FEATURES.md), open defects and residual
risks in [`AUDIT.md`](AUDIT.md). Each item states priority, where it lives and
the signal that would move it to `FEATURES.md`. No dates are promised.

## Near-term fixes (verified in code)

What is left of the 29.15.0 audit (AUD-167) after the 2026-11-15 review, which
fixed items 4, 5, 12, 14 and 15 (AUD-169..173 in
[`audit/FIXED.md`](audit/FIXED.md)) and documented item 16. Numbers are kept so
older notes still match. Findings still open are in [`AUDIT.md`](AUDIT.md).

| # | Pri | Item | Where |
|---|---|---|---|
| 3 | P2 | An index-side error after a verified transaction (reorg moved its position, block not indexed) calls `penalize()` and cools up to three healthy providers for 300 s. Once the txid matches, everything else in `verify` is local: go straight to the block path. Needs a fake source that records `penalize` (the current one ignores it). | `crates/el-rawtx/src/txbatch.rs` (`verify`, `run_session`) |
| 7 | P2 | `scripthash`/`scriptpubkey` subscriptions are unbounded (only outpoints have a cap) and the notifier clones every subscription each pass. Cap them (a generous limit: a large wallet subscribes tens of thousands of scripts) and iterate under the lock. | `crates/el-serve/src/protocol.rs`, `crates/el-serve/src/notify.rs` |
| 11 | P2 | `privacy.toml` is one shared file next to the global `config.toml`. A per-network file (in the data dir, with migration) remains an option. | `bin/e-lite/src/privacy.rs`, `bin/e-lite/src/config.rs` |
| 17 | P3 | Slowloris: the idle deadline resets on every byte. (The other half of this item, "an unterminated line at EOF is executed", is not a defect: only a complete JSON document parses, and wallets or scripts that close after their last request rely on it.) | `crates/el-serve/src/server.rs` (`WireReader`) |
| 18 | P3 | Tests still missing because they need a real network: an e2e of the provider gate with `esplora=[]` (regtest never holds the gate; the rule is unit-tested in `provider_gate_*`) and a signet snapshot round trip. | `tests/e2e/e2e_networks.sh` |

## Security and network compatibility

| Priority | Item | Why / acceptance |
|---|---|---|
| P2 | Onion service packaging | publish Electrum over Tor without exposing a clearnet port; the managed mode already does it; remaining work is packaging/testing. |
| P2 | Derived `spends` index (outpoint performance) | 1.7 resolves spent outpoints on demand with no extra index writes. If profiling shows it is hot, add a derived `spends` CF with a schema/undo/snapshot plan; never on the hot apply path without a benchmark. |
| P2 | BIP352 silent payments | schema v2 for prevout pubkeys, in-memory scan keys, labels/notifications; never log or persist keys. |
| P3 | ARM64 builds and more distros | release artifacts + CI. |
| P3 | Wallet-compatible authentication | Electrum has no standard client authentication; token/basic/mTLS are not wallet-compatible. Access control stays firewall/IP allowlist/VPN/Tor. |

## Performance

| Priority | Item | Why / acceptance |
|---|---|---|
| P2 | HDD sequential mode/readahead | fewer readers in file order, `fadvise`/readahead; A/B on HDD. |
| P2 | Adaptive controller v2 | partial: dedicated 5 s/30 s thread, fitted memory plan, real `commit/blk`, starvation readers, p95 signals. Still missing: compaction-driven control and per-profile benchmark tables. |
| P2 | Compaction/job tuning | partial: per-disk `jobs` and per-family L0/statistics. Still missing: pause/batch when `pending_compaction` grows. |
| P3 | Commit-group batching | bulk groups use 20-60 s by disk class and 512 blocks; make the durability tradeoff explicit and tested per workload. |
| P3 | UTXO/prefetch pipeline experiments | measure before changing the hot path. |

## Storage savings

| Priority | Item | Why / acceptance |
|---|---|---|
| P1 | History compaction / deduplication (schema v3, AUD-130) | reduce the index from ~260 GiB to an estimated ~130-150 GiB: `txnum` u40 references, 16-byte scripthash and 8-byte txid prefixes verified against `txids.dat`, 1-byte history value, trimmed UTXO values, zstd bottommost. Design and per-table bytes in AUDIT AUD-130. **Decision: from-zero reindex, no in-place migration; deferred by the owner.** First step: measure per-CF SST size (`/status.rocksdb.cf`) and row counts on the owner's node and reindex testnet4 with both schemas; proceed only if it saves >= 35 %. |
| P2 | 40-bit tx refs / compact occurrence encoding | storage reduction with migration and BIP30 tests. |
| P2 | zstd/bottommost compression experiments | measure size vs CPU on HDD/SSD; no silent format change; applies to newly compacted data. |
| P3 | Incremental/encrypted snapshots | per-file sizes/SHA-256 and `--verify=full` are delivered; incremental backups and confidentiality of the chain-data archive are less urgent than the row above. |
| P3 | Undo retention policy | the bounded reorg window is conservative; tune only with recovery tests. |

## Privacy

| Priority | Item | Why / acceptance |
|---|---|---|
| P2 | Decoy scheduling improvements | avoid temporal correlation while keeping bandwidth bounded. |
| P2 | Tor stream isolation against a real Tor | per-request SOCKS credentials are unit-tested against a fake SOCKS server; still missing a real-Tor test that checks distinct circuits. |
| P3 | Redaction audit of `/status` and `/metrics` | logs are covered by `bin/e-lite/tests/log_redaction.rs`; extend the check to the HTTP output. |
| P3 | `privacy.toml` per network | optional once the shared-file docs are corrected; needs a migration decision. |

## Compatibility and operations

| Priority | Item | Why / acceptance |
|---|---|---|
| P2 | Container image (Docker/Podman) | reproducible runtime, mounts for Core/index; no secrets baked in. UmbrelOS notes are in `TUNING.md`. |
| P2 | Electrum/Sparrow matrix | scripted versions for 1.4; document known limitations. |
| P2 | Core version matrix | test the oldest supported and the latest official Core; fail clearly. |
| P3 | Cross-container sync coordination | the per-user IBD lock does not cross mount namespaces; needs a shared lock medium or orchestrator policy. |
| P3 | Plugin/extension surface | only after core stability; avoid freezing unstable APIs. |

## Review follow-ups

Left open by the 2026-11 review ([`reviews/2026-11-analisis.md`](reviews/2026-11-analisis.md)).

| Priority | Item | Why / acceptance |
|---|---|---|
| P2 | Table tests for `lag_guard` | `bin/e-lite/src/pipeline.rs`: only `byte_window` is unit-tested; the pause/resume/watermark decisions are covered by e2e only. Add a table of (Core state, lag, disk) → action before changing it. |
| P2 | Second review pass | `bin/e-lite/src/tuning/` and `crates/el-rawtx/src/p2p.rs` were reviewed by structure, targeted searches and tests, not line by line. |
| P3 | Defaults quoted in the docs | `scripts/check-docs.sh` checks links, versions and source references; extend it to the default values quoted in `docs/` (a `config.example.toml` key followed by its value). |
| P3 | Cost of `panic = "unwind"` | AUD-173 switched release builds to unwind; binary size and IBD speed before/after were not measured. |

## Cleanup scheduled

| When | Item |
|---|---|
| Two releases after 0.1.0-alpha.29.19.0 | Remove `DEPRECATED_FLAGS` (`bin/e-lite/src/cli.rs`): the retired flags then fail as unknown, and their table leaves `docs/CLI.md`. |

## Explicit non-goals for now

- Public unauthenticated Electrum without TLS/auth.
- Serving invented history or tx data when a source is unavailable.
- Claiming mainnet production readiness before the acceptance matrix and audit
  P0/P1 are closed.
