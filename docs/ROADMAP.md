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
- The provider gate needed **>60 s** to open: `probe_once` probed the
  providers **serially** (two HTTP requests each, 30 s timeout per request).
- `rpc error: method=blockchain.transaction.get code=-32000` during that
  window (throttled to one line per minute, so the real count is hidden).
- `wallet session:` printed every 60 s on idle connections: `server.ping`
  counted as activity.

**What the server cannot change:** a v3 onion round trip is 1-7 s. **What it
can change** is the number of round trips: Sparrow starts every connection
with one request per line and upgrades to JSON-RPC batches only for servers
it recognises by the `server.version` name (`electrumx`, `electrs/` >= 0.9.0,
`fulcrum` >= 1.6.0, `mempool-electrs`, `frigate`, `cormorant`). `e-lite X` is
not recognised and e-lite rejects batch arrays, so a 61-address scan is 61
serial round trips. See item A.

Plan reviewed against the code on 2026-11-13. Order of work:

| # | Priority | Change | Where | Status / acceptance |
|---|---|---|---|---|
| 2 | P1 | `transaction.get` failures counted by cause (`not_found`, `no_source`, `external_failed`, `verify_failed`, `internal`) in `/status.rawtx.tx_get_failures`, `debug:` line every 5 s per cause, no hashes (AUD-132). Fail-closed behaviour unchanged (AUD-045). | `el-serve`, `el-rawtx` (`RawtxError::NoSource`) | **Done in alpha.29.12.2.** |
| B | P1 | Separate timeout for Esplora requests through Tor (`tor_request_timeout_seconds`, 20 s; direct stays 8 s) and a 40 s budget per `fetch_block`. The 8 s total covered a new circuit plus a whole block, and a timeout cooled the provider for 30-300 s. | `crates/el-rawtx/src/esplora.rs` | **Done in alpha.29.12.2; measured 2026-11-13: not enough** (20 s total still cut live downloads, timeouts still cooled providers). Replaced in alpha.29.13.0 by connect/stall/attempt timeouts, failure-typed backoff and one 60 s budget per block (AUD-155/156). |
| 1 | P1 | Probe public providers **in parallel**, one request each (genesis raw block by its known hash; `/block-height/0` only when unknown), own Tor circuit per provider, Tor attempts get at least `tor_request_timeout_seconds`; the gate opens 2 s after the first healthy provider. | `bin/e-lite/src/public_servers.rs` (`probe_round`) | **Done in alpha.29.12.2.** Test `gate_opens_with_the_first_healthy_provider`; the owner's mainnet log decides the real opening time. |
| A | P1 | **JSON-RPC batch arrays** (answered in order; items in parallel up to 8; max 1000) plus opt-in `[server] advertise_electrumx_compat` so Sparrow recognises e-lite (banner `e-lite X (electrumx-compatible)`). **Done in alpha.29.12.2.** Owner decision 2026-11-13: ElectrumX substring, banner keeps "e-lite" first. Next: measure a Sparrow scan over the onion with the option on; if it is fast and no other wallet misbehaves, make it the default and send Sparrow an upstream patch that recognises `e-lite` with the measured numbers. | `el-serve`, `docs/ELECTRUM_PROTOCOL.md` | A batched scan over the onion takes a few round trips. |
| 3 | P2 | Session summary only on wallet requests (>= 3 `get_history`/`transaction.get`), with AUD-149. | `el-serve` | **Done in alpha.29.12.2.** |
| 5 | P2 | `NOTIFY_WRITE_TIMEOUT` 2 s -> 10 s **rejected**; instead `notify` queues behind the connection lock and the lock holder delivers on release, so the shared notifier never waits for one client's 30 s response write. | `el-serve` (`notify`, `flush_pending`) | **Done in alpha.29.12.2**, test `notify_never_waits_for_a_busy_connection`. |
| 4 | P1 | Prefetch with decoys, **on by default**, below wallet requests in priority. Mainnet/testnet4 logs of 2026-11-13 (29.12.3) showed `tx.get` at 16-28 s average and up to 85 s even with batches: every pruned block waited for a whole download over Tor. | `el-rawtx`, `el-serve` `Prefetcher` | **Done in alpha.29.13.0 (AUD-159)**, with AUD-153..158. Acceptance: Sparrow over Tor on a pruned mainnet node, `wallet session` `tx.get` average < 5 s, cache hits > 0, no `all p2p peers are cooling down`, no `inflight block download timed out`. |
| 6 | P2 | Document the Tor reality in `PRIVACY.md`/`TUNING.md` with the measured result of A and B. | `docs/` | With the same change. |

**Note on item A (Sparrow, read 2026-11-13 from its `master` sources):**
Sparrow never probes a capability; it maps the `server.version` banner to
fixed flags (`ServerCapability`). An unrecognised banner gets batching
**off**, but still `unsubscribe` and `server.features` on, which e-lite
already serves. A recognised name (e.g. `electrumx`, `fulcrum 1.6`) turns
batching on and keeps the same expectations; `cormorant` additionally
turns on block stats, `mempool-electrs` caps fee targets at 25. Batches are
paged at 100 requests (`PagedBatchRequestBuilder.DEFAULT_PAGE_SIZE`, halved
after timeouts), responses are matched **by id**, and one item error makes
`execute()` discard the page (`executeTolerant()` keeps the rest). Not
verified: the wire framing in the JSON-RPC library and the
`isResponseTooLarge` retry. Decision: (a) now, with the honest banner
`e-lite X (electrumx-compatible)` (Sparrow matches "electrumx" by
substring), to measure; (b) the upstream patch afterwards, with the
measurement.

Out of scope (with a reason, do not reintroduce blindly): a second Tor
instance for the outbound/Core `-onion` side (revisit only with measured
contention), and any change that hints at clearnet fetches.

## Planned work (2026-11-14) — 29.13.1, proposed

### External path: agile and precise (AUD-160)

Measured on mainnet with alpha.29.13.0 (owner log, LAN, pruned Core): a
wallet load served 218 of 239 transactions from Core/cache; the 21 that
needed a **pruned block** all failed outside, at **20.4 s average / 51.9 s
max** per `transaction.get`, 8 `external_failed`, with every source failing
at once:

```text
p2p: tor 65.191.83.40:8333: socks connect: Resource temporarily unavailable (os error 11)
p2p: 34.93.152.123:8333 block: Resource temporarily unavailable (os error 11); p2p deadline reached
esplora: tor https://mempool.space/api: timeout; deadline reached
```

Why it happens (verified in the code and in `man tor`):

| Fact | Evidence |
|---|---|
| Tor allows **60 s** to build a circuit (`CircuitBuildTimeout`, adaptive) and **2 min** per SOCKS handshake (`SocksTimeout`); **32** pending circuits max (`MaxClientCircuitsPending`) | `man tor`, defaults |
| e-lite gives up earlier: P2P over Tor **30 s**, Esplora connect **20 s** | `p2p_tor_timeout_seconds`, `tor_connect_timeout_seconds` |
| A SOCKS CONNECT that never gets an answer is blamed on the **peer** (`Blame::Transient`, 30 s→300 s cooldown) and the 15 s proxy pause never trips | `p2p.rs:550`, `p2p.rs:763-770` |
| P2P has one deadline for circuit + handshake + whole block; Esplora already has connect/stall/absolute (AUD-155) | `p2p_tor_timeout_seconds` |
| No global cap on new Tor connections: 4 downloads can open P2P v2+v1 each, plus prefetch and decoys | `lib.rs`, `p2p.rs` |

So the path is not slow, it is **mis-detected**: each block pays the full
60 s budget, healthy peers are cooled, and a dead proxy is indistinguishable
from a slow circuit.

Design (two time planes; a breaker with hysteresis; one probe):

| Layer | Budget | Behaviour |
|---|---|---|
| Wallet | **10 s** (`wallet_external_wait_seconds`) | a pruned-block `tx.get` answers in 1-5 s when the path is healthy; otherwise returns a typed error in 10 s and the wallet retries |
| Probe | **75 s** (`CircuitBuildTimeout` 60 + margin), background | launched once after a circuit stall, off the wallet's path; success restores `Healthy`, failure extends the pause with 15→30→60 s backoff |

| State | Enters on | Behaviour |
|---|---|---|
| `Healthy` | one success | normal attempts |
| `Degraded` | **2 CONNECT stalls in 90 s** against distinct peers/providers (or 1 stall + Esplora down) | wallet fails in ms; P2P-over-Tor paused; one probe per 30-60 s |
| `ProxyDown` | SOCKS TCP/greeting/auth failure (exists) | same, 15 s pause |

Blame table (precision): a SOCKS **reply code** (refused, unreachable, exit
policy) is that peer/provider's failure (short backoff); a **CONNECT without
an answer** is a *path* signal counted globally (peer gets 30 s only); a proxy
handshake failure is `ProxyDown`; 429/503 keep `Retry-After`; a wrong answer
keeps the full 300 s. Never cool a peer that was not contacted, and never
declare the proxy dead from one slow circuit.

Concurrency: a global semaphore of **4** new SOCKS connections shared by P2P,
Esplora, decoys and prefetch, well below Tor's 32 pending. `max_parallel_fetches`
keeps bounding downloads.

Files: `crates/el-rawtx/src/health.rs` (state machine, counters, probe),
`p2p.rs` (connect/stall/absolute split, blame, consult the state),
`esplora.rs` (consult the state; no attempts while degraded), `lib.rs`
(wallet plane, probe, semaphore, prefetch/decoys paused while degraded),
`config.rs` + `config.example.toml` (`wallet_external_wait_seconds` 10,
`external_pause_seconds` 30, `external_stalls_to_degrade` 2,
`max_external_connects` 4), `el-serve` (`/status.rawtx.external` and one INFO
line per state change; per-block failures move to `debug:` deduplicated).

Tests (fake SOCKS, which already exists for the greeting): greeting ok and no
CONNECT answer → no peer cooled, second stall → `Degraded`, next `tx.get` in
ms; reply code 5 → only that peer; greeting rejected → `ProxyDown`; probe ok →
`Healthy`; prefetch/decoys do not queue while degraded; 20 concurrent requests
never open more than 4 connections; a healthy path keeps 29.13.0's timings.

Acceptance: healthy path unchanged; with the path down no `tx.get` exceeds
~10 s, `/status` shows `Degraded` plus the probe, recovery in ≤60 s without a
restart, one INFO line per state change and no `rawtx at height … failed`
flood.

Out of scope: reusing circuits per time window (one per provider per 30-60 s)
would remove the storm but breaks per-request isolation (AUD-135); keep it as a
documented opt-in with its privacy trade-off. Clearnet on Tor failure stays
rejected (AUD-128); a second Tor instance stays out until contention is
measured.

Environment check before/while implementing (to separate network from code):

```bash
tail -50 <data-dir>/tor/tor.log
curl -s --max-time 20 --socks5-hostname 127.0.0.1:9050 \
  https://mempool.space/api/blocks/tip/height; echo " exit=$?"
```

A `curl` that also times out means Tor cannot leave the host; this design then
turns it into fast errors and retries, which is all a server can do.

Interim mitigation (config, no code):

```toml
[rawtx]
prefetch = false
decoys = 0
max_parallel_fetches = 2
```

### Schema v2 / history compaction (AUD-130) — from-zero reindex

**Owner decision (2026-11-13): no in-place migration.** Schema v2 will be
delivered as a from-zero reindex with a from-zero Core; the migrator and its
double-disk requirement are cancelled.

Scope: 1-byte `history` value, 8-12 byte txid prefix verified against
`txids.dat`, 16-byte scripthash prefix; estimated 30-40 % smaller (not
measured).

Design notes from the 2026-11-13 review:

- A txid prefix is **not unique**: with ~1.2e9 transactions an 8-byte prefix
  already has a few-percent chance of an accidental collision, and a
  colliding pair costs ~2^32 work to craft. The lookup must accept several
  candidates per prefix and verify each against `txids.dat`; "fail
  conservatively" is not enough.
- The 16-byte scripthash prefix is justified by 128-bit second-preimage
  resistance, not by BIP158 (which uses 64-bit SipHash values).
- Measure first: per-family SST sizes on a mainnet index
  (`/status.rocksdb.cf`) and the bottommost zstd experiment (Storage
  savings, P2), which needs no schema change. Before starting, also cover:
  BIP30 duplicate-txid tests, `canonical_digest` equivalence (old vs new on
  the same chain), the snapshot `schema` field and import refusal, and a
  measured size + read-latency benchmark (including a wallet xpub scan).

### Deferred with rationale

- **Online export / decoy weighting / incremental backups**: left for a later
  batch (owner decision).
- **`/status` and `/metrics` redaction check**: the log macro scanner only
  covers log lines; extend it to status/metrics output now that the
  wallet-session and `tx_get_failures` counters exist.

### Documentation and diagnostics checklist (closed in alpha.29.12.2)

Every item below was checked against the code and the doc was fixed in
alpha.29.12.2 (AUD-148/AUD-149/AUD-150); kept as the record of what was
verified:

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
