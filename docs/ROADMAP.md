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

## External path: agile and resilient (AUD-160) — implemented, unreleased

The first proposal (two time planes, a `Degraded` breaker, a semaphore of 4
connections) was replaced after measuring: the path was not mis-detected but
**serial and blocking**. What was built and why is in `docs/AUDIT.md`
(AUD-160) and `docs/PRIVACY.md`; the short version:

- Each block download **races** its endpoints (providers over Tor, P2P peers):
  up to 3 attempts, hedged on failure, silence, a crawling rate or 15 s; the
  first verified block wins.
- Phase limits per attempt (connect, stall) instead of one long deadline;
  endpoints that never answer back off up to 10 min; a Tor 429 is per circuit.
- The 60 s network budget starts at the download slot; slots 4 -> 8.
- The Electrum connection no longer blocks behind a slow `transaction.get`.
- The prefetch starts every history block as soon as the history is served.

Measured over Tor (32 distinct blocks at once): 0.11-0.14 -> 0.49-0.66
blocks/s, failures 66-78 % -> 0-9 %; clearnet 5.1 blocks/s. The remaining time
over Tor is the network: one circuit carries 60-450 KB/s and a mainnet block
is ~1.5 MB.

Acceptance still pending on the owner's node: a cold Sparrow load of the same
wallet with `[logging] level = debug`, comparing `wallet session` lines and
`/status.rawtx.race` with the 29.13.0 log.

### Next candidates (not implemented)

- **Core `getblockfrompeer` as a race source.** Core already holds 8-10 open
  peer connections (onion ones included), so a request skips the circuit and
  the handshake. Core 23+: `getblockfrompeer <hash> <peer_id>` schedules a
  `getdata(MSG_WITNESS_BLOCK)` on that connection and returns at once; the
  block lands in Core's current `blk` file (so it survives manual pruning for
  a while) and is read back with `getblock`. Constraints from Core's source:
  it is asynchronous (poll `getblock`), a peer that does not deliver is
  **disconnected** by Core, peers ignore blocks older than a month that are
  not in their active chain (not our case) and pruned peers do not serve old
  blocks, so only `NODE_NETWORK` peers qualify. Privacy: a clearnet peer sees
  the node's IP asking for an old block, and every request on one long-lived
  connection is linkable. Proposal: opt-in `[rawtx] core_fetch =
  "off" | "onion" | "any"`, default `off`; `onion` uses only Core's onion
  outbound peers (no IP), `any` is for `allow_clearnet` setups. Needs RPC
  plumbing (`getpeerinfo` filter, `getblockfrompeer`, polling) and an e2e test
  on regtest with two nodes.
- **Circuit reuse per provider for a short window.** Would save the circuit
  build per block but lets a provider link the blocks of one load by exit
  (AUD-135). Only as a documented opt-in, if measurements show circuit
  building, not transfer, dominates.
- **Managed `torrc` tuning.** Nothing needed for the race today; revisit only
  with measurements (`CircuitBuildTimeout` is learned; `MaxClientCircuitsPending`
  32 is above our 24-attempt cap).

Rejected after checking: HTTP Range to split a block over several circuits
(Esplora servers answer 200 with the full body), compression (no gzip on
`/block/:hash/raw`), answering a wallet early with an error (Sparrow stores an
unfetchable placeholder, AUD-045; a timeout is retried), clearnet on Tor
failure (AUD-128).

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
