# Roadmap and proposed improvements

This is not a promise of dates. Each item states priority, risk and the
acceptance signal that would move it to `FEATURES.md`.

## Security and network compatibility

| Priority | Item | Why / acceptance |
|---|---|---|
| P1 | Authentication and rate limiting for non-loopback Electrum | Electrum has no standard client authentication; a token/basic policy or mTLS is not wallet-compatible. At minimum add per-client request budgets and document firewall/IP allowlist/VPN/Tor. TLS on a separate listener is already implemented. |
| P1 | Core RPC over TLS or a documented authenticated tunnel | avoids cleartext credentials on LAN/WAN. Could be native TLS or a mandatory SSH/VPN recipe with tests. |
| P2 | Onion service packaging | publish Electrum over Tor without exposing a clearnet port. |
| P2 | Electrum 1.7 | feature negotiation and new methods; only when tested against Sparrow/Electrum. |
| P2 | BIP352 silent payments | schema v2 for prevout pubkeys, in-memory scan keys, labels/notifications; never log/persist keys. |
| P3 | ARM64 builds and more distros | release artifacts + CI. |

## Performance

| Priority | Item | Why / acceptance |
|---|---|---|
| P1 | Parallel decode/txid/Merkle in reader threads | overlap SHA256 with I/O/apply; measure tx/s on large blocks. |
| P1 | UTXO prefetch pipeline | prefetch next block’s prevouts into a bounded cache; overlay wins. |
| P2 | HDD sequential mode/readahead | fewer readers in file order, `fadvise`/readahead; A/B on HDD. |
| P2 | Adaptive controller v2 | queue-wait metrics, compaction/RocksDB signals, safer AIMD, benchmark profiles. |
| P2 | Parallel compaction/job tuning | expose/test mutable RocksDB options per profile. |
| P3 | Commit-group batching | amortise fsync while bounding crash window; make interval configurable/tested. |

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
| P1 | Provider pooling/health and per-source policy | avoid single-provider dependency; still whole-block only. |
| P2 | Tor stream isolation verification | per-request circuits, no DNS leaks; integration test where possible. |
| P2 | Decoy scheduling improvements | avoid temporal correlation while keeping bandwidth bounded. |
| P2 | Log/metric redaction audit | ensure scripts/txids/labels never appear; add tests/lint. |

## Compatibility and operations

| Priority | Item | Why / acceptance |
|---|---|---|
| P1 | Systemd unit and service hardening | `Restart=always`, resource limits, no privileges; documented. |
| P2 | Container image (Docker/Podman) | reproducible runtime, mounts for Core/index; no secrets baked in. |
| P2 | Electrum/Sparrow matrix | scripted versions for 1.4; document known limitations. |
| P2 | Core version matrix | test oldest supported and latest official; fail clearly. |
| P3 | Plugin/extension surface | only after core stability; avoid freezing unstable APIs. |

## Explicit non-goals for now

- Public unauthenticated Electrum without TLS/auth.
- Serving invented history or tx data when a source is unavailable.
- Claiming mainnet production readiness before the acceptance matrix and audit
  P0/P1 are closed.
- Silent payments before a schema/versioned migration and key-handling review.
