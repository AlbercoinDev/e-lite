# Roadmap and proposed improvements

This is not a promise of dates. Each item states priority, risk and the
acceptance signal that would move it to `FEATURES.md`.

## Security and network compatibility

| Priority | Item | Why / acceptance |
|---|---|---|
| P3 | Wallet-compatible authentication | Electrum has no standard client authentication; a token/basic policy or mTLS is not wallet-compatible. Per-client rate limiting is implemented (alpha.28); access control still belongs to firewall/IP allowlist/VPN/Tor. |
| P2 | Onion service packaging | publish Electrum over Tor without exposing a clearnet port. |
| P2 | Derived `spends` index (outpoint performance) | 1.7 resolves spent outpoints on demand from script history with no extra index writes. If profiling shows it is hot, add a derived `spends` CF (outpoint → spender) with a schema/undo plan and snapshot handling; never on the hot apply path without a benchmark. |
| P2 | BIP352 silent payments | schema v2 for prevout pubkeys, in-memory scan keys, labels/notifications; never log/persist keys. |
| P3 | ARM64 builds and more distros | release artifacts + CI. |

## Performance

| Priority | Item | Why / acceptance |
|---|---|---|
| P2 | HDD sequential mode/readahead | fewer readers in file order, `fadvise`/readahead; A/B on HDD. |
| P2 | Core/e-lite scheduler A/B | `scheduler = "alternate"` is implemented and opt-in. Measure Core solo with a 5-minute `SIGSTOP`; make it the default only if Core solo exceeds the ~6.7 blocks/s break-even with 40–60 GiB phases. |
| P2 | Adaptive controller v2 | **Partly delivered in alpha.29.5.0**: dedicated 5 s/30 s thread, fitted memory plan, real `commit/blk`, `batch_locked` honoured. 29.8.0 adds full-group commit reasons and starvation-based readers; 29.8.1 fits the budget to `MemAvailable` (8/16 GiB) and stabilises the hot cache. Still missing: hot memtable resizing, compaction-driven control and per-profile benchmark tables. |
| P2 | Parallel compaction/job tuning | expose/test mutable RocksDB options per profile. |
| P3 | Commit-group batching | interval/batch already extend to 30 s/4096 in bulk; still make durability tradeoff explicit/tested per workload. |

## Storage savings

| Priority | Item | Why / acceptance |
|---|---|---|
| P1 | History compaction / deduplication | reduce `history` size on mainnet; must preserve canonical digest semantics or version the schema. |
| P2 | 40-bit tx refs / compact occurrence encoding | storage reduction with migration and BIP30 tests. |
| P2 | zstd/bottommost compression experiments | measure size vs CPU on HDD/SSD; no silent format change. |
| P2 | Incremental/encrypted snapshots | faster backups and optional confidentiality; checksum/signature verification. |
| P3 | Undo retention policy | bounded reorg window is already conservative; tune only with recovery tests. |

## Privacy

| Priority | Item | Why / acceptance |
|---|---|---|
| P2 | Periodic provider health/cooldown | startup readiness, latency and Tor→clearnet fallback are implemented (alpha.29.1). Add periodic re-probing, cooldown and per-source policy; still whole-block only. |
| P2 | Tor stream isolation verification | per-request circuits, no DNS leaks; integration test where possible. |
| P2 | Decoy scheduling improvements | avoid temporal correlation while keeping bandwidth bounded. |
| P2 | Log/metric redaction audit | ensure scripts/txids/labels never appear; add tests/lint. |

## Compatibility and operations

| Priority | Item | Why / acceptance |
|---|---|---|
| P3 (optional) | Systemd unit and service hardening | **Optional, not a release blocker.** A user can request it; `Restart=always`, resource limits and no privileges, documented. |
| P2 | Container image (Docker/Podman) | reproducible runtime, mounts for Core/index; no secrets baked in. UmbrelOS packaging notes are in `TUNING.md` (container RAM/nofile, external Core, volumes, SIGTERM, cross-container sync lock). |
| P3 | Cross-container sync coordination | the per-user IBD lock does not cross mount namespaces; needs a shared lock medium or orchestrator policy. |
| P2 | Electrum/Sparrow matrix | scripted versions for 1.4; document known limitations. |
| P2 | Core version matrix | test oldest supported and latest official; fail clearly. |
| P3 | Plugin/extension surface | only after core stability; avoid freezing unstable APIs. |

## Explicit non-goals for now

- Public unauthenticated Electrum without TLS/auth.
- Serving invented history or tx data when a source is unavailable.
- Claiming mainnet production readiness before the acceptance matrix and audit
  P0/P1 are closed.
- Silent payments before a schema/versioned migration and key-handling review.
