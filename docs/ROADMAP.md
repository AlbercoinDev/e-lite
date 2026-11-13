# Roadmap and proposed improvements

This is not a promise of dates. Each item states priority, risk and the
acceptance signal that would move it to `FEATURES.md`.

## Planned work (2026-11-13)

### Wallet over Tor: measured problem, corrections (next release)

Measured on mainnet with alpha.29.12.1 (owner logs):

- Server-side latency for wallet requests is **0-5 ms** (`debug: wallet
  session: ... history 39 (0 ms avg, 5 ms max)`).
- The same wallet over LAN: 277 `tx.get` in 67 s. Over the onion: 61
  `get_history` in 425 s (~7 s each), serial.
- The provider gate needed **>60 s** to open: `probe_once` probes the
  providers **serially** (two HTTP requests each, 30 s timeout per request).
- `rpc error: method=blockchain.transaction.get code=-32000` during that
  window (throttled to one line per minute, so the real count is hidden).
- `wallet session:` printed every 60 s on idle connections: `server.ping`
  counted as activity.

**Physics that cannot be fixed server-side:** a v3 onion round trip is
1-7 s, Electrum is one round trip per request, and wallets issue 1-3 serial
requests per scanned address. A 61-address scan over Tor is minutes; the
fast path is TLS over LAN.

The actionable corrections, in order:

| # | Priority | Change | Where | Acceptance |
|---|---|---|---|---|
| 1 | P1 | Probe public providers **in parallel** (bounded threads, per-provider timeout, first genesis-validated answer wins; statuses still collected). | `bin/e-lite/src/public_servers.rs` (`probe_once`) | Gate opens within one probe timeout instead of 3x; unit test with local slow/fast servers. |
| 2 | P1 | Tag `transaction.get` failures by cause (`external unavailable` / `not found` / `pruned gap`), `debug:` line with a 5 s throttle (INFO stays at 60 s), counter in `/status`. No hashes in logs (AUD-132). Behaviour stays fail-closed (AUD-045). | `crates/el-serve/src/lib.rs`, `el-rawtx` | A pruned-block `tx.get` without a provider logs the cause and increments the counter. |
| 3 | P2 | Session summary only on interesting activity: count only `get_history`/`transaction.get` (>=3 to print); `server.ping` no longer triggers it. | `el-serve` `ClientStats` | Idle connection with pings prints no `wallet session:`. |
| 4 | P2 | Prefetch with **decoys**, still opt-in (`[rawtx] prefetch = true`): one decoy per prefetched block through the same resolver (`raw_tx_prefetch` is currently decoy-less). This warms the blocks of the served history while the wallet keeps scanning, so the "Loading transactions" phase hits the cache. Privacy trade-off documented; default stays off (AUD-134). | `crates/el-rawtx/src/lib.rs:552`, `el-serve` `Prefetcher` | With prefetch on, a second `tx.get` of the same block does not fetch; decoy counter rises; with prefetch off no extra traffic. |
| 5 | P2 | `NOTIFY_WRITE_TIMEOUT` 2 s -> 10 s so a slow Tor client is not disconnected between notifications (each reconnect costs a new onion circuit). Trade-off: the notifier is shared, a stuck client can delay others up to 10 s. | `el-serve` | No behaviour change on LAN; test with a client that does not read. |
| 6 | P2 | Document the Tor reality and the opt-in prefetch in `PRIVACY.md`, `TUNING.md`; log/CHANGELOG/HANDOFF entries. | `docs/` | Docs updated with the same change. |

Out of scope (with a reason, do not reintroduce blindly): pipelining per
connection (no effect with serial wallets — this log shows one request every
~7 s and zero `tx.get` during the finding phase), request batching (the
wallet decides), a second Tor instance for the outbound/Core `-onion` side
(revisit only with measured contention), and any change that hints at
clearnet fetches.

### Schema v2 / history compaction (AUD-130) — from-zero reindex

**Owner decision (2026-11-13): no in-place migration.** Schema v2 will be
delivered as a from-zero reindex with a from-zero Core; the migrator and its
double-disk requirement are cancelled.

Scope: 1-byte `history` value, 8-12 byte txid prefix verified against
`txids.dat`, 16-byte scripthash prefix (BIP158 standard); estimated 30-40 %
smaller. Checklist before starting: collision rules (prefix ambiguity must
fail conservatively), BIP30 duplicate-txid tests, `canonical_digest`
equivalence (old vs new on the same chain), snapshot `schema` field and
import refusal, and a measured size + read-latency benchmark (including a
wallet xpub scan).

### Deferred with rationale

- **Online export / decoy weighting / incremental backups**: left for a later
  batch (owner decision).
- **`/status` and `/metrics` redaction check**: the log macro scanner only
  covers log lines; extend it to status output when the wallet-session
  counters land.

### Documentation and diagnostics checklist (open, AUD-148/AUD-149)

Verify each claim against the code and fix the doc (no release needed):

- `AGENTS.md:167` — 4C/8T plan gives `jobs=2`: stale. `MachinePlan::detect`
  now uses `physical.clamp(2, jobs_cap)`: SSD/NVMe cap 8, ZFS/virtual 4, HDD
  2 -> a 4C host gets 4.
- `AGENTS.md:57-59` — prune bullet: add the caught-up bypass (29.9.0) and
  `min_gain_live_blocks = 144` (29.9.2); the 20 GiB margin only applies while
  Core downloads or the index drains.
- `AGENTS.md` — missing bullets: `[logging] level` (alpha default `debug`,
  `debug:`/`trace:` lines incl. `wallet session`), `zfs_arc_dynamic` /
  `zfs_arc_max_mib` / `zfs_arc_min_mib` + `e-lite arc-setup` + the 10-minute
  permission-probe retry, `recent`/`eta` in the height line.
- `docs/TUNING.md:52`, `:58-64`, `:483` — the machine-plan example and table
  repeat the old `jobs` formula; update to the per-disk rule.
- `docs/CLI.md` — document `--apply` (arc-setup) and
  `--hash-files`/`--no-hash-files` (snapshot export).
- `docs/FEATURES.md` — add: plain listener loopback default (AUD-147),
  prune-at-tip + `min_gain_live`, log levels, `recent`/`eta`, `wallet
  session`, per-disk jobs plan.
- `docs/HANDOFF.md:51` — "on 0.0.0.0 for Electrum": TLS is `0.0.0.0`, plain
  is loopback by default (`plain_lan`/`--plain-lan`).

Diagnostics cosmetics (AUD-149):

- `wallet session`: print µs (or a 1 ms minimum) instead of `0 ms avg`.
- `first tx.get N ms after get_history`: measure from the connection's first
  history, not the last one (idle use reports minutes).

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
| P2 | Adaptive controller v2 | **Partly delivered in alpha.29.5.0**: dedicated 5 s/30 s thread, fitted memory plan, real `commit/blk`, `batch_locked` honoured. 29.8.0 adds full-group commit reasons and starvation-based readers; 29.8.1 fits the budget to `MemAvailable` (8/16 GiB) and stabilises the hot cache; 29.8.2 adds p95 signals, phase/ARC diagnostics and a critical memtable shrink; 29.8.3 adds opt-in hot memtables, prefetch deadline and ionice. Still missing: compaction-driven control and per-profile benchmark tables. |
| P2 | Parallel compaction/job tuning | Partial: the plan now gives SSD/NVMe `jobs = physical cores (2..8)`, HDD 2 (alpha.29.9.1), plus per-family L0/statistics diagnostics (29.8.6) and the dynamic ZFS ARC. Still missing: compaction-driven control (pause/batch when `pending_compaction` grows) and per-profile benchmark tables. |
| P3 | Commit-group batching | interval/batch already extend to 30 s/4096 in bulk; still make durability tradeoff explicit/tested per workload. |

## Storage savings

| Priority | Item | Why / acceptance |
|---|---|---|
| P1 | History compaction / deduplication | reduce `history` size on mainnet; must preserve canonical digest semantics or version the schema. **Decision 2026-11-13: schema v2 as a from-zero reindex (no in-place migration); see "Planned work (2026-11-13)".** |
| P2 | 40-bit tx refs / compact occurrence encoding | storage reduction with migration and BIP30 tests. |
| P2 | zstd/bottommost compression experiments | measure size vs CPU on HDD/SSD; no silent format change. |
| P2 | Incremental/encrypted snapshots | Per-file sizes/SHA-256 and `--verify=full` verification are delivered (alpha.29.10.0). Still missing: incremental backups and optional confidentiality. |
| P3 | Undo retention policy | bounded reorg window is already conservative; tune only with recovery tests. |

## Privacy

| Priority | Item | Why / acceptance |
|---|---|---|
| P2 | Periodic provider health/cooldown | startup readiness, latency and Tor→clearnet fallback are implemented (alpha.29.1). Add periodic re-probing, cooldown and per-source policy; still whole-block only. |
| P2 | Tor stream isolation verification | Per-request SOCKS credentials for Esplora and P2P are delivered and unit-tested against a fake SOCKS server (alpha.29.11.0); still missing: a test against a real Tor that checks distinct circuits, and provider rotation instead of last-good-first. |
| P2 | Decoy scheduling improvements | avoid temporal correlation while keeping bandwidth bounded. |
| P3 | Log/metric redaction audit | Delivered (alpha.29.11.0): leaks fixed (AUD-132) and `tests/log_redaction.rs` rejects log macros that print script hashes or txids. Still missing: the same check for `/status` and `/metrics` output. |

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
