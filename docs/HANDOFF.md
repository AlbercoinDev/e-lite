# Session handoff

**Updated:** 2026-11-13
**Latest published release:** `0.1.0-alpha.29.12.3`
**Release:** https://github.com/AlbercoinDev/e-lite/releases/tag/v0.1.0-alpha.29.12.3
**Main commit at handoff:** tag `v0.1.0-alpha.29.12.3`

alpha.29.11.0 was verified after publishing: `ci` and `release` workflows
green, and both one-line installers (`--no-core` and `--with-core --yes`)
installed it in a clean `debian:12` container (Core v32.1.0). Not measured on
mainnet yet: AUD-129 (cold wallet scan), AUD-131 (IBD write volume), AUD-138
(mempool cost), AUD-139 (cache hit rate).

This file is the short “where we are” for a new session. The invariants live in
[`AGENTS.md`](../AGENTS.md) and [`PROJECT_MEMORY.md`](PROJECT_MEMORY.md); the
current behaviour is defined by the code, `config.example.toml` and
`CHANGELOG.md`. If this handoff conflicts with a newer commit, the newer commit
wins; update this file.

## Read order for a new session

1. `AGENTS.md`
2. `docs/README.md`
3. `docs/PROJECT_MEMORY.md`
4. `docs/HANDOFF.md` (this file)
5. `docs/FEATURES.md` and `docs/ROADMAP.md`
6. `docs/AUDIT.md`
7. The specific docs for the task (`CLI`, `INSTALL`, `ARCHITECTURE`, `OPERATIONS`,
   `PRIVACY`, `TUNING`, `ELECTRUM_PROTOCOL`, `GETTING-STARTED`).

Suggested prompt:

```text
Trabaja en este repo. Lee AGENTS.md, docs/README.md,
docs/PROJECT_MEMORY.md, docs/HANDOFF.md, docs/AUDIT.md,
docs/FEATURES.md y docs/ROADMAP.md. Continúa desde el estado actual;
no repitas contexto ni rehagas decisiones ya documentadas.
```

## Current state

- From-zero pruned IBD uses `prune_mib = 1` (manual pruning). Core does not prune
  by itself; the prune gate keeps the cut below `index - 288`.
- `prune_mib >= 550` requires `[pruning] allow_auto_prune = true`.
- Snapshots are optional and are not needed for the from-zero flow.
  `export-snapshot` is per-network; `--with-core` bundles the network's Core
  datadir with an index archive (manifest format 3, coherence-checked,
  `--verify=full` opens the restored Core). `AGENTS.md` has the details.
- Electrum binds per-network defaults: plain/TLS/metrics are
  `55011/55022/55084` (mainnet), `55111/55122/55184` (testnet4),
  `55211/55222/55284` (signet) and `55311/55322/55384` (regtest). TLS binds
  `0.0.0.0`; plain binds loopback by default (`plain_lan`/`--plain-lan` to
  expose it, AUD-147); metrics stay on loopback. TLS is on by default with a generated self-signed certificate
  (`<data-dir>/tls/`) and does not depend on Tor; `tls = false` / `--no-tls`
  disables it. `[tor] mode = "managed"` (default) publishes the same plain/TLS
  ports as a v3 onion. Data defaults to `~/e-lite/<network>` and Core to
  `<data-dir>/bitcoin`, so one instance per chain can run at once. Wildcard
  listeners (`0.0.0.0`/`[::]`) are mapped to loopback onion targets. The
  installer-provisioned Tor bundle is launched with `LD_LIBRARY_PATH` pointing
  at its own libraries. There is no client authentication; treat the onion
  address as a secret. Per-client rate limiting is a throttle, not auth: 200
  req/s with burst 5000 for every client (LAN and public), over-budget requests
  wait for the next token instead of being rejected, and `[server] rate_limit =
  0` disables it. `run` probes the
  network providers and gates the wallet listeners until one answers a real
  genesis block request (`/block-height/0` plus `/block/<genesis>/raw` with a
  header-hash check); Tor failure falls back to clearnet
  (`tor_fallback_clearnet`).
- Electrum 1.4–1.7 is implemented and negotiated per connection: numeric
  `server.version`, verbose `transaction.get` with block context, notification
  suppression, real `scripthash.unsubscribe`, canonical mempool status order,
  1.6 headers arrays, `mempool.get_info`, `estimatefee(mode)`,
  `broadcast_package`, 1.7 `scriptpubkey.*`, `outpoint.*`,
  `testmempoolaccept`, `mempool.recent` and the new `server.ping`. A request for
  an unimplemented/skipped version falls back to the newest implemented version
  at or below it; 1.6+ requires `server.version` as the first message. Outpoints
  are resolved on demand from the existing history/UTXO data; there is no
  persistent spend index or schema change.
- Core RPC is HTTP; LAN/WAN requires SSH/VPN.
- Rawtx resolution is local Core -> validated cache -> P2P/Esplora (whole blocks,
  Tor-first via the managed Tor, decoys, verified).
- Adaptive profiles and `SIGUSR1` hot reload are implemented:
  `auto|hdd|ssd|nvme|manual`, readers capped at 4/8/16, cgroup memory aware,
  explicit readers/commit overrides locked. Every knob is also available via CLI
  and `--set` (see `docs/CLI.md`).
- Memory: budget fitted to `MemAvailable` (`effective = min(total, available -
  core_reserve)`), reserve `clamp(effective/8, 2-4 GiB)`, ceiling 75 % of
  effective, memtables `effective/16` (weighted per CF), local-SSD profile
  cache ceiling `total/4` (2 GiB at 8, 4 at 16, 8 at 32) capped by the effective
  room; the hot cache follows available RAM with hysteresis and `cache_mib` is
  a maximum. Readers/jobs come from physical cores and the disk class.
- `run` waits for Core RPC before starting P2P/mempool/listeners; `serve` is the
  offline mode. Shutdown joins background threads and stops managed Tor before
  stopping Core.
- `docs/FEATURES.md` is the current-feature source of truth. `docs/TOR.md` covers
  the onion setup, reset and security.
- `docs/ROADMAP.md` contains future features/improvements. Do not present them as
  available.

## Last change (alpha.29.12.1)

ARC without root, honest rate/ETA, wallet-session diagnostics.

- **`e-lite arc-setup`** (+ `--apply` as root) and an automatic
  `e-lite-arc-cap.service` from `install.sh`: `zfs_arc_max` becomes
  group-writable so the dynamic ARC manager works without running e-lite as
  root. Negative permission probes are retried every 10 min (signet had the
  parameter read-only and the manager stayed warn-only with a 205 MiB cache).
- **Height line**: `recent X b/s` (60 s sliding window over full commit
  groups) + `eta` = `lag / recent`; `phase X b/s` stays as the phase
  average. The signet dip showed the phase average (117) hiding a real
  37 b/s interval.
- **`debug:` wallet-session lines** (counts/ms only): connection open/close,
  first `tx.get` after `get_history` (reconstruction start), and a 60 s
  summary with history/tx.get counts and avg/max latency. This is the
  measurement step for the xpub-scan work.
- **Owner decision (this session): schema v2 (AUD-130) will be developed as a
  from-zero reindex with a from-zero Core — no in-place migration.**
- Tests: fmt, clippy, workspace (282).

## Last change (alpha.29.12.0)

Owner decisions of 2026-11-12, implemented (see `CHANGELOG.md` 29.12.0):
AUD-128 (no clearnet fallback), AUD-134 (prefetch opt-in), AUD-135 (random
provider order), AUD-136 (`[core] network`, `private_broadcast`), AUD-147
(plain listener on loopback), AUD-140/141/143 (snapshot format 3). **Defaults
changed**: read the upgrade notes before upgrading wallets/hosts.

Signet validation of the whole 29.9.x-29.12.0 chain (owner log,
2026-11-12): full sync 0 -> 330574 in **48m44s (113 blocks/s)** with
`jobs=4` from the plan, FULLY SYNCED once, then the caught-up prune ran
(`deferred prune at 330286` -> `pruned Core blocks below 329970`, ~329k block
locators dropped) and the process kept serving. The mid-sync dip to 37 b/s
tracked block size (avgblk 138 -> 407 KiB) against a constant ~20 MB/s
effective write ceiling on the ZFS pool; it recovered when blocks shrank.
`arc: dynamic management unavailable (cannot write zfs_arc_max: Read-only
file system)` - the signet run had no sysfs permission, so the manager stayed
warn-only as designed (ARC 17.7 GiB, e-lite cache squeezed to 205 MiB). For
the dynamic ARC to work, e-lite needs write access to
`/sys/module/zfs/parameters/zfs_arc_max` (root, group-writable parameter or a
non-read-only mount).

## Last change (alpha.29.12.3)

Requested by the owner (2026-11-13): a way to empty the memoised cache so a
wallet measurement is honest, and the measurement data in the log instead of
extra commands. Only the rawtx cache is touched: the index, Core, TLS and the
onion keys share the data directory and are never deleted.

- **`e-lite cache status|clear [--yes] [--dry-run] [--json]`**. `status`
  reports files/bytes for whole blocks and verified transactions, the limits
  from `cache_split` and the free space. `clear` removes only `*.blk` at the
  cache root and `tx/<xx>/*.tx`, drops the emptied shards, and counts anything
  else as skipped. It refuses the data directory, an ancestor/descendant, a
  filesystem root, a RocksDB `CURRENT` and `index/`, `bitcoin/`, `tls/` or
  `tor/`. Works while serving (a vanished file is a miss); restarting releases
  the in-memory block cache. No Core needed (`network = auto` resolves from the
  existing data directories).
- **Self-describing `debug:` logs**: startup banner + `electrumx_compat` state,
  cache inventory at open (no directory walk), provider limits; per connection
  the client name/version and protocol and a one-time `first batch, N items`;
  session/window summary and close line with batches, cache hits/puts/fetches
  and `tx.get` failure deltas; one `rawtx summary` at shutdown. Two relaxed
  counters per line, nothing per item; `info` costs nothing.
- `/status.server` (banner, compat, lines/batches) and
  `/status.rawtx.cache_*` for panels.
- AUD-151 (`dbcache 0 MiB` while the operator value was in force) and AUD-152
  (fixed 25 % ARC advice against the dynamic policy) fixed.
- Gates: fmt, clippy -D warnings, `cargo test --workspace` (294),
  `tests/e2e_regtest.sh` (now includes cache status/clear and asserts the index
  and Core survive it), `tests/e2e_defaults.sh`. Nothing measured on mainnet
  yet.

## Previous change (alpha.29.12.2)

Owner decision of 2026-11-13 on the wallet-over-Tor plan: JSON-RPC batch
arrays are the main lever, and Sparrow only sends them to servers it
recognises by name, so `[server] advertise_electrumx_compat` (opt-in) makes
the banner `e-lite X (electrumx-compatible)` without claiming another
server. Measure a Sparrow scan over the onion with it on before making it
the default.

- **JSON-RPC batches** (max 1000 items, up to 8 in parallel, ordered when the
  batch carries `server.version` or an unsubscribe). This turns one onion
  round trip per address into one per page.
- `[rawtx] tor_request_timeout_seconds = 20`: Esplora requests through Tor no
  longer share the 8 s direct timeout; one `fetch_block` spends at most 40 s.
  **Not measured**: the owner's mainnet-over-Tor run decides.
- Provider gate: parallel probe, one request per provider (genesis raw
  block by hash), own Tor circuit, opens 2 s after the first healthy one.
- `/status.rawtx.tx_get_failures` + `debug: tx.get failed: <cause>`;
  `RawtxError::NoSource` is new.
- The notifier queues behind the connection lock instead of waiting for it
  (the planned 10 s timeout was rejected).
- AUD-148 (docs vs code), AUD-149 (wallet-session diagnostics) and AUD-150
  (help/docs defaults) fixed.
- Gates run for this revision: `cargo fmt --check`, `clippy -D warnings`,
  `cargo test --workspace` (288) and `tests/e2e_regtest.sh`. Nothing is
  measured on mainnet yet.

Open: the **2026-11-13 measured plan** is in `docs/ROADMAP.md` ("Planned
work (2026-11-13)") and its table now carries the status of each item: done
(batches, parallel probe, Tor timeout, failure causes, session noise,
notifier) versus pending (prefetch with decoys, demoted to P3 until item A
is measured; documentation of the measured result). The schema v2 decision
(AUD-130: **from-zero reindex, no in-place migration**) is unchanged.
Online export, decoy weighting and incremental backups stay deferred.

AUD-148, AUD-149 and AUD-150 are fixed in this revision; the roadmap's
"Documentation and diagnostics checklist" is closed and kept only as a
record of what was checked.

Measured on mainnet (2026-11-13, alpha.29.12.1): the server answers wallet
requests in 0-5 ms; the same wallet scans in seconds over LAN and took
minutes over the onion because a v3 onion round trip is 1-7 s and the wallet
issued one serial request per address (61 `get_history` in 425 s). The
server cannot shorten a round trip; it can reduce their number, which is
what JSON-RPC batches do (opt-in banner until measured).

Candidate raised by the signet log: `phase indexing X b/s` was the cumulative
phase average (117 b/s while the real interval was 37). Fixed in 29.12.1:
the `height` line now prints `recent X b/s` from a 60 s window plus `eta`.

## Last change (alpha.29.11.0)

A static review (2026-11-12) recorded AUD-126..AUD-147 in `docs/AUDIT.md`
under "Open findings". Fixed in alpha.29.11.0 (see `CHANGELOG.md`):

- **AUD-126**: `--with-core` packs only `blocks/`, `chainstate/`, `indexes/`
  of the network (allowlist); wallets, keys and `bitcoin.conf` never enter
  the archive.
- **AUD-127**: the provider gate is skipped for an unpruned Core; for a
  pruned Core it stays fail-closed (AUD-045), with opt-in
  `[public_servers] open_without_provider`.
- **AUD-129**: 32-byte prefix extractor + prefix bloom on `history`,
  `utxo_by_script`, `tx_occurrences`; no migration. Mainnet cold-scan
  latency still to measure.

- Second batch: AUD-131 (undo written only inside the window), AUD-132
  (log redaction), AUD-133 (generic user agents), AUD-135 (per-request Tor
  circuits), AUD-137 (P2P peer refresh), AUD-140/141/142 (single digest
  pass, atomic import, `--expect-digest`), AUD-144, AUD-145.

Still open and needing an owner decision: AUD-128 (clearnet fallback
default), AUD-130 (schema v2 for disk), AUD-134 (prefetch without decoys is
an AGENTS invariant), AUD-135 provider rotation (last-good-first is an
invariant), AUD-136 (route Core through Tor), AUD-143 (hash Core files by
default), AUD-147 (bind 0.0.0.0). - Third batch: AUD-138 (mempool by txid diff), AUD-139 (transaction
  cache), AUD-146 (notifier wake-up, notification timeout, `listunspent`
  order, readers at the tip), AUD-141 free-space checks, AUD-132 log lint.
  Also needing a decision: AUD-140's gzip default and online export (needs a
  control endpoint). Nothing in this batch is measured on mainnet yet.

## Last change (alpha.29.10.0)

Full per-network snapshots: index + Bitcoin Core in one archive.

- **AUD-124 (P1, fixed)**: `snapshot.rs::add_tree` never excluded the nested
  Core datadir, so an index `export-snapshot` silently packed all of Core
  (hundreds of GB, inconsistent `chainstate/`). Index snapshots now always
  exclude `bitcoin/`; the full-backup path is explicit.
- **`export-snapshot --with-core`**: refuses while a Core is running, probes
  it (start + RPC + stop), packs `<data-dir>/bitcoin` under `bitcoin/`
  (wallets/mempool/peers/cookie/logs excluded) and records tip, prune height
  and subversion in manifest **format 2** (format 1 still imports). Pair
  rules enforced: `index tip <= Core tip`, `prune_height <= index tip - 288`.
  gzip level `--compress-level` (default 3, balanced); `--hash-files` adds
  per-file SHA-256.
- **`import-snapshot`**: empty target, all format-1 checks plus the Core
  section: file sizes/hashes (`--verify`), coherence, and `--verify=full`
  (isolated Core `-connect=0 -listen=0`, tip checks, `verifychain(2, 288)`).
- Per network: run per `--network mainnet|testnet4|signet`; regtest covered
  by `tests/e2e_snapshot.sh` (9 new unit tests + the e2e).
- **AUD-125 (P2, mitigated)**: hand-mixed index/datadir pairs could restore
  with a prune cut deeper than the undo window; the manifest now rejects it.

### Not decided in 29.10.0

- Whether gzip level 3 is the right default for Core-heavy archives (the
  `blk*.dat` gains ~10-20 %); revisit with a mainnet-size measurement — the
  level is already a flag.
- `--verify` remains opt-in; sizes are checked always. Decide whether the
  hash check should be default once real mainnet archives exist.

## Last change (alpha.29.9.2)

Live-mode prune batching on the day scale.

- **Finding**: `min_gain_blocks = 5,000` is right for IBD/drain (the gain
  grows thousands of blocks per hour) but wrong at the tip: the safe cut
  advances ~144 blocks/day at mainnet rates, so live mode would hold ~7.5
  GiB of dead data for ~35 days.
- **Fix**: `[pruning] min_gain_live_blocks = 144` (default, 0 = keep the
  IBD gain) — once caught up, the gate prunes when the safe cut advances
  144 blocks (~1 day, ~220 MiB). Validation caps it at `min_gain_blocks`.
  IBD/drain behaviour unchanged; emergency pruning unchanged; durability
  invariants unchanged.
- Whole chain at the tip (29.9.0 + 29.9.1): prune to the safe maximum once,
  keep ~one day of tail, prune daily, ARC shrinks to its minimum, the
  memory plan recovers.
- Tests: `caught_up_live_gain_uses_the_live_scale`; workspace 235.

### Not decided in 29.9.2

- Whether the drain phases (Core out of IBD, lag > 0) should also use the
  live-scale gain: today they keep `min_gain_blocks`, which releases most
  of the backlog on its own windows. Revisit with a log.

## Last change (alpha.29.9.1)

Automatic jobs at startup, honest live re-syncs and the dynamic ZFS ARC.

- **Bug**: the 29.8.8 jobs rule only reached the profile-reload path;
  `MachinePlan::detect` kept `physical/2`, so an automatic 4-core host still
  planned `jobs = 2` (29.8.9 mainnet log). Both paths now share the rule,
  capped by disk kind (SSD/NVMe 8, ZFS/virtual 4, HDD 2).
- **READY/FULLY SYNCED**: first transition per session logs INFO; later
  live re-syncs (one block ahead, then caught up) log at `debug:`.
- **Dynamic ZFS ARC** (`[index] zfs_arc_dynamic = true` default): cap grows
  to `zfs_arc_max_mib` (0 = half of RAM) while Core is in IBD or the index
  drains, drops to `zfs_arc_min_mib` (1024) once fully synced. Writes
  `zfs_arc_max` (bytes, 5 s tick, 5 min dwell) only when e-lite can write
  it; without permission it stays warn-only. `arc: cap X -> Y MiB (phase)`
  on every change; the hourly uncapped warning yields to the manager.
  29.9.0's prune-to-max-at-caught-up pairs with it: at the tip the ARC
  shrinks, the plan recovers and the prune releases ~140 GiB.
- Tests: fmt, clippy, workspace (234).

### Not decided in 29.9.1

- The ARC hit-rate line mixes compaction misses with prevout lookups
  (compaction reads dominated the 29.8.9 log's 4%); separating them is a
  diagnostics nicety, not a correctness issue.

## Last change (alpha.29.9.0)

Prune to the safe maximum once the index is caught up.

- **Finding (AUD-122)**: `prune_defer_free_gib = 20` deferred all normal
  pruning while free space stayed above the margin — on the mainnet ZFS host
  (~170 GiB free) a from-zero IBD left Core holding ~89k already-indexed
  blocks (~140 GiB) with `pruneheight` frozen at 886146 for days.
- **Fix**: once Core is out of IBD and the index matches Core's tip, the
  free-space deferral is bypassed: one prune to the safe maximum
  (`index - 288`), then `min_gain_blocks` batches the live trickle. During
  Core's IBD nothing changes. Durability invariants untouched (durable tip,
  WAL barrier, safe-height cap, retry-after-backoff).
- Tests: caught-up bypass + live batching in `el-chain`; the IBD deferral
  test still passes. Workspace 234.

### Not decided in 29.9.0

- Whether the caught-up bypass should also apply while a Core **out of IBD**
  still has a large index backlog (lag > 0): today it does not — the drain
  phases defer as before, and the backlog's own `min_gain` windows release
  most of it. Revisit with a log.

## Last change (alpha.29.8.9)

The index now reaches the tip: the last-blocks stall and honest READY.

- **Bug**: with fewer blocks left than the commit minimum, each 500 ms
  catch-up call rebuilt the same 2-block target list and the end-of-call
  commit could be skipped — Ok without progress, no log line, forever. Seen
  on mainnet as 85+ min at `lag 2` with no `READY` and "(100.0%)" on screen.
- **Fixes**: a clean end-of-call always commits the applied prefix (under
  disk pressure it commits first and logs a `debug:` line); the reader wait
  is bounded (10 s + `debug:`); readers return their permit at the end of
  the target list; `debug: pipeline:` traces every call (rate-limited
  no-progress note at the tail).
- **READY correctness**: `sync.ready` now requires Core out of IBD (the
  spurious "READY ... Core still in IBD" lines are gone; the state machine
  already excluded it). The height line caps at 99.9 % while blocks remain;
  `FULLY SYNCED` is the real 100 %. READY/FULLY SYNCED lines include the
  session time.
- Full-sync timing from the console log: 0 → 975409 took 4 d 9 h 12 min
  wall (~17 sessions, Core IBD in parallel); the last drain was 23m18s at
  ~3.4 blocks/s before the tail stall.
- Tests: fmt, clippy, workspace (232).

### Not decided in 29.8.9

- The exact reader-level root cause of the tail silence: the bounded wait
  plus the `pipeline:` debug lines will show it in the next occurrence; the
  symptom (no commits at the tail) is now structurally impossible.

## Last change (alpha.29.8.8)

Log levels, a bolder compaction plan and the ARC warning.

- **Log levels** `info < debug < trace`: `[logging] level` / `--log-level`,
  hot-reloadable, alpha default `debug` (stable will flip to `info`). INFO
  lines unchanged; new detail prints as `[e-lite] debug:` / `trace:`:
  per-family and per-level RocksDB counters, live statistics (cache hit
  ratio, WAL-sync latency histogram, compact/flush/WAL bytes) every 10 min,
  PSI + ARC hit rate and the cache-controller decision next to every memory
  sample, byte-window arithmetic at phase changes, per-commit-group lines at
  `trace`. `/status.rocksdb.cf` gains `level0_files`.
- **Plan: `background_jobs = physical cores, 2..8` on SSD/NVMe** (HDD keeps 2,
  virtual keeps `physical/2`): the old `physical/2` plan gave a 4-core host
  one compaction thread and the debt grew ~70 GiB/h at drain; four jobs kept
  it flat. Explicit config still wins. Validated by the next mainnet log.
- **ZFS ARC warning**: startup + hourly when ARC > 25% of RAM, with the
  suggested cap `min(25% RAM, 8 GiB)`. ARC is invisible to `MemAvailable`
  and starved the cache plan at ~20.5 GiB.
- Tests: fmt, clippy, workspace (232, incl. ARC/PSI parser tests).

### Not decided in 29.8.8

- Crediting ARC headroom into the cache ledger: defer until an ARC-capped
  log shows the ledger working with a true `MemAvailable`.
- `prefetch_deadline_ms` stays opt-in; the `rocksdb-stats` line now gives the
  data to decide.
- Default log level flips to `info` for stable releases; keep `debug` through
  alpha.

## Last change (alpha.29.8.7)

Two measurement fixes found by the first 29.8.6 mainnet log.

- **Bug (29.8.6)**: `block_cache` was summed over the seven families, but
  with a shared cache every family reports the whole-cache usage
  (`18854 MiB` reported next to a `2705 MiB` capacity = 7x). Read once
  through the first data family now; expected ≈ the memory plan's capacity.
- **Bug (pre-existing)**: ZFS `arc` parsed the kstat type column, so it
  always printed 0 MiB; it now reads the value column (unit-tested). On ZFS
  this matters for read-spike analysis (the same log showed 602-4850 ms
  block reads).
- Evidence for the compaction question: `pending_compaction` stayed flat
  (~170-175 GiB) through 26 min of the indexing phase at `jobs=2`; the
  decisive test is the draining phase (the 29.8.5 debt grew at drain rate).
- No controller, default or budget change. Tests: fmt, clippy, workspace
  (231, incl. the new ARC kstat test).

### Not decided in 29.8.7

- `background_jobs` default stays the plan value until a draining-phase A/B.
- Whether ARC was actually capped/empty on the ZFS box is only observable
  after this fix; re-read the `arc` value in the next log.

## Last change (alpha.29.8.6)

Compaction diagnostics: real block cache, L0 files and commit-spike context.

- **Bug**: `rocksdb.block-cache-usage` was read through the DB handle, which
  only reports the empty default family, so `rocksdb:`/`/status.rocksdb`
  always showed `block_cache 0 MiB`. It is now summed per CF.
- **Telemetry**: `CompactionStats.level0_files` (summed
  `num-files-at-level0`) in the 10-minute `rocksdb:` line and
  `/status.rocksdb`. Growing `l0` with `running 1` is the flush/compaction
  stall signal; `pending_compaction` is an estimate that inflates during
  ingest — use its trend.
- **Commit spikes**: a full group ≥ 10 s costing twice the bulk time limit or
  5x the previous full group's per-block cost logs `[e-lite] commit spike:`
  with running compactions, L0 files, memtable, pending compaction and Core
  backlog, sampled at the spike. Normal bulk groups stay silent.
- Motivation: mainnet end-of-IBD log showed `pending_compaction` 49 → 185 GiB
  in one hour with `running 1`, commit/blk 57 → 210 ms and one unexplained
  45.5 s commit.
- `[index] background_jobs` (0 = plan: physical/2, 2..4) is the lever to A/B;
  no default changed. Tests: fmt, clippy, workspace.

### Not decided in 29.8.6

- `background_jobs` default stays the plan value until the A/B shows the
  compaction debt stops growing with 4.
- Cache churn (shrink on short `available` dips, 256 MiB grow steps) is
  untouched; revisit only with evidence.

## Last change (alpha.29.8.5)

Dynamic cache ceiling without double-counting resident memory.

- **Bug**: 29.8.4 added the planned memtable/runtime budgets to `MemAvailable`
  and then subtracted them again, so the hot cache collapsed to 8-30 MiB when
  `available` was 2-5 GiB (`block_cache = 0`, prefetch p95 10-15 s).
- **Fix**: `cache_dyn = min(profile, MemAvailable - reserve_dyn)`, with
  `reserve_dyn = min(startup reserve, max(1024 MiB, MemAvailable/2))`.
  With `available = 2870 MiB` and `reserve = 3066 MiB`, the ceiling is 1435 MiB.
- **Telemetry**: `/status.memory` and the hot log show dynamic reserve,
  effective basis, planned memtables, **actual** RocksDB memtable and
  table-reader bytes.
- **A/B knob**: `[index] max_open_files` to reduce table readers on 8-16 GiB.
- Tests: fmt, clippy, workspace (230), dynamic-reserve unit test on the log
  sample.

### Not decided in 29.8.5

- `max_open_files` default stays 8192; 2048/4096 are A/B knobs.
- `adaptive_memtables`/`prefetch_deadline_ms`/`io_priority` remain opt-in.

## Last change (alpha.29.8.4)

cgroup v2 I/O priority, prune defaults and the A/B harness.

- **`core.cgroup_io_weight`**: best-effort `e-lite-core` child cgroup with
  `io.weight` for launched/adopted Core; warning if delegation/`io` is absent,
  `ionice` remains the fallback.
- **Prune defaults**: 20 GiB free-space deferral and 5,000-safe-block gain;
  emergency low-disk pruning bypasses both. The pruned-IBD e2e pins
  `prune_defer_free_gib = 0` because it must observe the gate.
- **`tests/ab_umbrel.sh` + `docs/AB_TESTS.md`**: labelled sampling CSV, phase
  rates and the readers/cache/bulk/alternate matrix. Smoke-tested locally;
  the mainnet/Umbrel run is the user's measurement.
- Tests: fmt, clippy, workspace (229), e2e defaults/regtest/pruned/recovery/
  multi/launch.

### Not decided in 29.8.4

- readers/cache/`max_bulk`/`alternate` defaults require the Umbrel A/B; the
  harness and thresholds are ready.
- `adaptive_memtables`, `prefetch_deadline_ms`, `io_priority` and
  `cgroup_io_weight` remain opt-in.

## Last change (alpha.29.8.3)

Opt-in hot memtables, prefetch deadline, ionice and explicit ZFS/overlay.

- **`adaptive_memtables`**: live `effective/16` target, 512 MiB..2 GiB, WBM +
  per-CF `write_buffer_size`; 10 min persistence and 10 min between changes;
  cache changes have priority; explicit `write_buffer_mib` disables it.
  Critical-memory halving remains.
- **`prefetch_deadline_ms`**: 512-key chunks, stop at deadline, apply falls
  back; opt-in.
- **`core.io_priority`**: best-effort `ionice` for managed/adopted Core;
  warning if unavailable/ignored; off by default.
- **Disk**: `zfs`/`overlay` classes, warning with fstype/source, ARC in memory.
- Tests: fmt, clippy, workspace (228), real RocksDB hot resize test.

### Not defaulted in 29.8.3

- The new memtable/prefetch/ionice knobs are opt-in.
- Prune deferral, readers/cache and `alternate` A/B need mainnet measurements.

## Last change (alpha.29.8.2)

Coherence pass over the 29.8.1 memory policy.

- **Core reserve is real**: `run`/`index` probe the RPC port first; an
  already-running or external Core is not subtracted again. `serve` reserves
  nothing. Operator `-dbcache=...` is parsed and used.
- **Cache growth uses p95**, not the tail-dominated EWMA. `/status.tuning`
  exposes p50/p95/max prefetch/wait.
- **Byte pause/resume dwell**: 10 s at the watermark; reader pressure still
  pauses immediately.
- **Diagnostics**: machine plan prints fstype/source/device; memory prints
  SReclaimable/Shmem/ARC/RSS; `/status.scheduler` has avg block bytes, phase and
  phase rate; progress prints phase rate, p50/p95/max, commit group/reason;
  `rocksdb:` summary every 10 min. Unknown disk class warns with fstype/source.
- **IbdLock** is acquired before the first 512-block call when Core is in IBD or
  lag > 5,000.
- **Alternate** with a Core e-lite cannot pause warns and keeps overlap.
- **Critical memtables**: below 256 MiB available the shared
  `WriteBufferManager` is halved (minimum 256 MiB) as a last-resort safety
  valve; routine hot resizing remains future work.
- Tests: fmt, clippy, workspace (224), p50/p95 and 4 GiB budget tests.

### Not yet done in 29.8.2

- Routine hot memtable resizing remains future work; memtables are sized at
  startup, with the critical one-way shrink above.
- `ionice`/cgroup I/O priority and prefetch deadlines are not implemented.
- No mainnet/Umbrel measurement of the new diagnostics yet.

## Last change (alpha.29.8.1)

Memory budget fitted to available RAM, not installed RAM.

- **Effective budget**: `effective = min(total, MemAvailable - core_reserve)`.
  Reserve, ceiling, runtime, memtables, cache start/floor and page floor derive
  from it. `core_reserve` is the fitted `-dbcache` when e-lite will start Core;
  external or already-running Core is already in `MemAvailable`.
- **8/16 GiB typical**: local SATA/unknown/virtual cache ceiling is `RAM/4`
  (2 GiB at 8 GiB, 4 at 16, 8 at 32); HDD/network `RAM/8`. Memtables are
  `effective/16` (512 MiB at 8 GiB, 640 MiB at 16, ~880 MiB at the 32 GiB log
  with 18 GiB available). `-dbcache` is `min(total/8, available/2, 4096)`.
- **Hot cache**: dynamic ceiling from `MemAvailable + current`; shrink when
  `cache > ceiling` or `available < reserve`; grow with a proportional margin;
  two samples + 5-minute reversal dwell, fast path below 1 GiB. The page-cache
  floor is no longer added to the shrink floor.
- **Memtables**: fixed at startup from `effective`; no hot WBM resize in this
  release because it can force flushes/commits stalls.
- **Block-size average**: 1/1024 EWMA instead of 1/8, so a burst cannot move
  the byte backlog 3-4x between pause and resume.
- **Commit batch**: capped at 512 (the checkpoint); grows only for a
  `Blocks`-limited group. `Time` may grow the interval; `Txs`/`Bytes` do not
  touch batch or interval.
- **Logs**: memory plan prints total, available, effective, core reserve,
  reserve, page floor, ceiling and cache start..max; `doctor` and
  `/status.machine` expose the same fields.
- Tests: fmt, clippy, workspace; new 8/16 GiB budget tests, dynamic cache
  action, long block average, batch cap. e2e defaults, regtest, recovery,
  pruned-IBD, multi-instance and launch-Core pass.

### Not yet verified in 29.8.1

- No mainnet run of the new available-RAM budget yet; the 8/16 GiB numbers are
  unit-tested, not measured on Umbrel.
- Hot memtable resizing remains future work.
- `scheduler = "alternate"` is still opt-in; keep `overlap` until a Core-solo
  measurement beats the break-even.

## Last change (alpha.29.8.0)

Byte backlog watermarks, opt-in alternate scheduler, commit/reader fixes.

- **Bytes, not blocks**: pause Core at `max_bulk_lag_gib` (30), resume at
  `core_resume_ahead_gib` (7). The backlog is
  `lag_blocks × avg_block_bytes` (EWMA of `PreparedBlock::raw_size`).
  `/status.scheduler` shows mode, backlog and both watermarks. The legacy
  `*_blocks` fields apply only when the GiB value is 0.
- **Scheduler**: `overlap` remains the default. `alternate` pauses e-lite
  indexing while Core downloads and pauses Core while e-lite drains to the low
  watermark; serving continues. It requires `prune_mib = 1` and
  `pause_core_during_bulk = true`. Opt-in because the measured break-even is
  Core alone > ~6.7 blocks/s and phase switches cost peer reconnects.
- **Commit controller**: full groups only. Boundary/shutdown partials do not
  update the EWMA/cycle; `Txs`-limited groups no longer shrink the batch;
  I/O-bound groups (`db_us > 2 × txids_us`) are left alone. New
  `/status.tuning.commit_last_*` fields expose reason, blocks, txs, bytes,
  cycle and the txids/db split.
- **Readers**: growth needs `wait_ms > 50` (starvation) and two consecutive
  samples; a high `read_ms` alone no longer removes readers when `wait_ms` is
  high. Floor is half the physical cores (min 2).
- **Disk identity**: `DiskInfo` keeps `major:minor`/mount source; `same_device`
  works through bind mounts and `/data` layouts.
- Tests: fmt, clippy, workspace; new unit tests for byte watermarks at two
  heights, boundary commit exclusion, tx-limited reason, reader streak and
  disk identity. Mainnet A/B pending.

### Not yet verified in 29.8.0

- No mainnet measurement of `scheduler = "alternate"`; keep `overlap` until the
  5-minute `SIGSTOP` Core-solo test is available.
- Real power loss and true ENOSPC remain untested.
- `/status.rocksdb.cf` and `commit_last_*` during the competition phase are not
  captured yet, so compaction/memtables are not tuned from that data.

## Last change (alpha.29.7.0)

Machine-aware plan and commit/prune policy.

- **CPU** uses physical cores and cgroup quota, not logical SMT: a 4C/8T
  i7-6700T gets `readers=4`, `jobs=2`, Core `par=3`, `rpcthreads=4` instead of
  8/8/7/8.
- **Disk** is classified (NVMe/SATA/HDD/virtual/network) from mountinfo+sysfs;
  data/Core sharing a device is detected. The class sets reader concurrency,
  cache cap, jobs and bulk commit interval.
- **Memory** adds a page-cache floor (`clamp(RAM/16, 1-4 GiB)`) and a larger
  reserve (`clamp(RAM/8, 2-4 GiB)`); cache shrinks in 12.5 % steps.
- **Commits** in bulk are bounded by tx (250k), blocks (512) and time
  (20-60 s by disk), with a minimum of 8 blocks/60 s. Live is 1.5 s. Safety is
  unchanged because the prune gate uses the committed tip.
- **Memtables** are weighted per CF; `/status.rocksdb.cf` exposes per-family
  pending compaction/memtable/SST/table readers.
- **Prune** runs in `el-prune` after the WAL barrier, with a 120 s timeout; the
  30 s EAGAIN stalls are gone. `/status.machine` shows the detected plan.

### Not yet verified in 29.7.0

- Regtest e2e passed (regtest, recovery, multi_instance, pruned_ibd). No
  mainnet A/B of the machine plan or the 20-60 s commit intervals.
- The disk classifier is host-tested on ext4/NVMe and tmpfs; LVM/overlay uses
  the virtual profile.
- Real power loss and true ENOSPC remain untested; the recovery e2e simulates
  SIGKILL, reorg and a file-size limit.

## Last change (alpha.29.6.0)

IBD throughput and safety pass driven by the 29.5.0 mainnet log.

- **Pipeline continuity**: the 10 s supervisor deadline is gone; supervisor
  actions run at commit boundaries and the 512-block checkpoint remains.
  `/status.pipeline` exposes prepared/applied/discarded.
- **Lag**: no baseline rewrite on a normal resume (the old code raised the next
  pause threshold by 10,000 blocks per cycle). `E_LITE_LAG_POLICY=drain` is an
  opt-in time watermark; the default stays the fixed window until a mainnet A/B.
- **Prune**: `SyncWAL(true)` replaces `flush_all` before `pruneblockchain`.
  `tests/e2e_pruned_ibd.sh` covers prune + SIGKILL + restart.
- **Metrics**: per-CF properties summed, DB-wide properties read once,
  `properties_ok` distinguishes unknown from zero; `commit_cycle_ms` is the
  real denominator for `commit_share`.
- **Controller**: one caller and one gate mutex; cache growth needs measured
  prefetch pressure; Bulk/Live hysteresis.
- **Prevouts**: `batched_multi_get_cf` over deduplicated sorted keys.
- **Core/Tor**: managed Core gets `-listenonion=0`; `/status.tor` exposes the
  effective SOCKS port; `doctor` explains the Core onion state.

### Not yet verified in 29.6.0

- Regtest e2e passed (`regtest`, `recovery`, `multi_instance`, `launch_core`,
  `pruned_ibd` with prune + SIGKILL). No mainnet A/B was run here.
- The `drain` lag policy is opt-in and unmeasured on mainnet.
- Real power loss and true ENOSPC remain untested; the recovery e2e simulates a
  file-size limit and SIGKILL.
- The UmbrelOS package is not built here.

## Last change (alpha.29.5.0)

Whole-IBD pass: memory, Core, commits and measurement are now one plan.

- **Minimum RAM enforced before side effects**: mainnet 7500 MiB, other
  networks 3800 MiB, measured after cgroup limits. `serve` is exempt.
- **Fitted budget**: `planned = runtime + memtables + cache <= ceiling`; explicit
  fractions are never raised; a shared `WriteBufferManager` caps memtables.
- **cgroup v1/v2 hierarchy** resolved from `/proc/self/cgroup` +
  `/proc/self/mountinfo`, each limit paired with its own usage.
- **Descriptors**: `RLIM_INFINITY` fixed, socket reserve, visible in
  `doctor`/`/status.file_descriptors`.
- **Core plan**: `-dbcache/-par/-rpcthreads` and an explicit `-prune`; operator
  overrides honoured, managed arguments rejected; child registered before the
  pid file; 120 s shutdown grace.
- **Undo for every block**, pruned from the real commit tip, including undo
  inserted by the same batch. Early commits can no longer lose the window.
- **`el-tune` thread**: 5 s sampling, 30 s minimum interval, correct bulk/live
  classification in `index`, `batch_locked` honoured, real `commit/blk`.
- **Per-user sync slot**: a second intensive sync waits and starts on its own.
- **Benchmark fixed**: monotonic clocks, `CLK_TCK`, simultaneous RSS peak,
  matching units, failures propagated; disk accounting excludes nested Core.

### Not yet verified

- The 4 GB testnet4/signet profile is a supported *candidate*: it has unit and
  logic coverage but no real hard-chain IBD measurement in this release.
- No real mainnet A/B of the new defaults (adaptive cache, Core plan, undo cost).
- Real power loss and true ENOSPC remain untested; the recovery e2e simulates a
  file-size limit and SIGKILL.
- The UmbrelOS package is not built here; container notes in `TUNING.md` are
  guidance, not a verified Compose file.

## Last change (alpha.29.4.5)

- Cap RocksDB `max_open_files` to `ulimit -n - 64` (minimum 256) so the prevout
  read improvement cannot cause `EMFILE` on hosts with a low file limit.

## Last change (alpha.29.4.4)

- **Mainnet baseline with defaults**: 4.2–4.8 blocks/s at ~640k; per-block
  cost dominated by prevout prefetch (100–1500 ms), not block reads. Commit is
  ~7–8 ms/block amortised. The block cache oscillated 6.6 GiB → 256 MiB.
- **Stable cache defaults**: max cache RAM/4, adaptive floor
  `clamp(RAM/8, 512 MiB, 4096 MiB)`, cache decisions based on `MemAvailable`.
- **Prevout path**: index/bloom filters stay out of the data block cache and
  `max_open_files = 8192`, so a shrinking cache does not evict filters.
- **Instrumentation**: cumulative stage microseconds, commit split (txids
  fsync vs RocksDB write), compaction/memtable/SST counters in `/status`;
  `tests/bench_ibd.sh` joint Core+e-lite benchmark.

## Last change (alpha.29.4.3)

- **TLS trust fixed for Java/Sparrow**: the generated TLS identity is now a
  local CA (`ca.crt`, `CA:TRUE`) plus a server leaf signed by it. The chain file
  contains leaf + CA. Trust `ca.crt` in the wallet; the old single self-signed
  cert was rejected as a Java `TrustAnchor`.
- Existing installs migrate on the next start (leaf key reused, new CA, chain
  rewritten). Remove the old trusted certificate from the wallet and import the
  new `ca.crt`.
- Tests: CA/leaf structure, migration, `openssl verify`, `CA:TRUE`, and
  wallet-style hostname/LAN-IP validation.

## Last change (alpha.29.4.2)

- **Undo at commit boundaries**: the window follows the end of the current
  commit group and time-based commits wait for undo coverage; a partial group
  without undo is not committed.
- **Per-network SOCKS**: managed Tor defaults to 9050 (mainnet) and
  55150/55250/55350 (testnet4/signet/regtest), avoiding startup races between
  instances.
- **New tests**: `tests/e2e_multi_instance.sh` (two instances, independent
  listeners, distinct certs, combined RSS, stop/restart isolation) and full
  TLS trust/hostname validation in `tests/e2e_defaults.sh`.

## Last change (alpha.29.4.1)

- **Undo at the committed tip**: bulk catch-up keeps undo for the last
  `reorg_window + 64` blocks of each batch, not only near Core's tip; a deep
  Core rollback now reports `NEEDS_RECOVERY` explicitly.
- **TLS hardening**: LAN IP SANs, DER SHA-256 fingerprint, atomic `0600` writes,
  key reuse, corrupt-pair regeneration, `tls_auto_cert = false` accepts an
  existing pair, and operator PEM pairs are validated at startup.
- **Validation**: a half-configured `tls_cert`/`tls_key` fails instead of being
  completed silently; legacy Core datadirs require the chain's blocks directory.
- New `tests/e2e_recovery.sh` (SIGKILL, reorg at the tip, write failure);
  regtest, pruned-IBD, defaults, resilience and wait-Core e2e pass.

## Last change (alpha.29.4.0)

- **Per-network ports**: 55011/55022/55084 (mainnet), 55111/55122/55184
  (testnet4), 55211/55222/55284 (signet), 55311/55322/55384 (regtest).
- **TLS on by default** with an auto self-signed certificate that does not
  require Tor; `--no-tls` disables it. Operator PEMs are preserved.
- **Per-network data/Core layout**: `~/e-lite/<network>` and
  `<data-dir>/bitcoin`; a single legacy Core datadir is detected with a
  migration warning, two candidates are rejected as ambiguous.
- **`run` serves offline**: an existing index opens Electrum in
  `CORE_UNAVAILABLE` while the index loop retries Core; first runs still wait.
- New `tests/e2e_defaults.sh`; regtest, pruned-IBD, resilience and wait-Core
  e2e pass. `e2e_networks.sh` needs 15 GiB free and real signet/testnet4
  downloads; it was not completed on the development VM (3.9 GiB free).

## Last change (alpha.29.3.7)

- **Pruned-file locator cleanup**: sidecar locators for missing `blk*.dat` files
  are dropped at open and the sidecar is compacted before the scanner starts;
  after a prune the in-memory map is filtered and file descriptors are closed.
- **ReaderTracker**: rawtx guards are taken only for blocks Core can still serve
  (height >= pruneheight); external-only resolutions and prefetch no longer
  block pruning.
- **Disk-first gain**: `min_gain_blocks` defaults to 1,000 (≈1.5 GiB retained
  tail on mainnet, rounded to whole block files) instead of 10,000.
- Regtest and pruned-IBD e2e pass; no schema/wire change.

## Last change (alpha.29.3.6)

- **Durable prune**: RocksDB/WAL is flushed before the prune cut is authorised;
  failed prune RPCs retry at the same safe height after 30 s.
- **Confirmed pauses**: `setnetworkactive` must succeed before internal
  pause/resume state changes; disk pauses cannot be overridden by lag resumes.
- **Exact readiness**: `READY`/`FULLY SYNCED` require the exact tip; prune and
  resume paths recompute state.
- **Indexing before providers**: `run` starts the indexer immediately and gates
  only the wallet listeners; provider outages no longer stop block progress.
- **Bounded pipeline**: explicit prepared-block window; long catch-ups yield to
  the supervisor after the poll interval.
- **Space-first default**: `prune_defer_free_gib = 0`; 1k gain and 30 s
  interval still bound churn. Bulk reader cache is dropped after prune and disk
  checks include a custom `blocks_dir`.
- Regtest and pruned-IBD e2e pass.

## Last change (alpha.29.3.5)

- **Outpoint notifications** are resolved by a bounded worker pool (2 workers,
  queue 256, pending retry) instead of the notifier thread, so a Core/provider
  lookup cannot delay other wallets. Reorg forcing stays.
- Removed the temporary live provider test; deterministic cooldown tests remain.
- Added `docs/ELECTRUM_PROTOCOL.md` with the complete 1.4–1.7 method/version
  matrix, notification rules, limits, deviations and performance notes.
- No schema or wire-format change.

## Last change (alpha.29.3.4)

- **P2P BIP144 fix**: `getdata` requests `MSG_WITNESS_BLOCK` (0x40000002), peers
  must advertise `NODE_WITNESS`, `notfound` is immediate, and the witness
  commitment is validated inside the P2P source before a peer is marked good.
  This removes the repeated `missing witness reserved value` errors.
- **BIP141**: highest-index coinbase commitment and exactly one 32-byte
  reserved witness item.
- **Electrum 1.7**: scripthash and scriptpubkey subscriptions are separate;
  reorg notifications are forced for scriptpubkey and outpoint subscriptions.
- **Protocol hardening**: checked numeric parameters, `testmempoolaccept`
  `reason`, prefetch dedup by block hash with retry, and no prefetch side effect
  from status computation.
- **Cache/perf**: reads and fsync outside the metadata lock; capacity enforced
  at open. External fetch deadline 30 s; inflight wait 45 s.
- Tests updated; no schema change.

## Last change (alpha.29.3.3)

- **Rate limit uniform and non-blocking**: 200 req/s / burst 5000 for every
  client (no loopback/LAN/public distinction). Over-budget requests wait for
  the next token instead of returning `-32000`; a Sparrow gap scan no longer
  aborts at ~55 addresses. `rate_limit = 0` still disables.
- **Disk-minimal defaults for a max-pruned Core**: rawtx cache 256 MiB (was
  2048), optional `rawtx.cache_dir` for tmpfs (`/dev/shm/e-lite-rawtx`), log
  rotation 16 MiB (was 64). Snapshots remain manual/off by default.
- **Disk breakdown** in `/status.disk`: index, txids, cache, log and snapshot
  bytes, plus the effective cache directory and total.
- No schema or protocol change.

## Last change (alpha.29.3.2)

- **Never fail without a network attempt**: if every provider is cooling down,
  e-lite forces one attempt (last good first). A provider that already answered
  is cooled for at most 30 s after a failure, so one transient error cannot
  disable the only working source for the full 300 s.
- **Bounded SOCKS5 handshake**: P2P Tor uses a minimal SOCKS5 CONNECT with the
  P2P timeout on connect/auth/request/reply; the `socks` crate had no timeouts
  and could block indefinitely on a stalled proxy.
- **Inflight panic guard**: a panicking external fetch completes the shared
  entry with an error instead of leaving followers waiting.
- **Diagnostics**: `get_history` and rawtx failures log the scripthash/txid,
  height and block hash.
- Tests: forced attempt while cooling, failed-provider skip with a healthy
  alternative, SOCKS timeout and password handshake. No schema/protocol change.

## Last change (alpha.29.3.1)

- **Dead providers no longer stall every block**: direct Esplora failures are
  cooled down like Tor failures (`provider_cooldown_seconds`, 300 s), the last
  good provider is tried first, and `request_timeout_seconds` defaults to 8 s.
  This fixes the 17–34 s gaps caused by `mempool.space/signet/api` timing out
  on every block.
- **In-flight dedup and concurrency**: concurrent wallet/prefetch requests for
  one block share a download; `max_parallel_fetches` (4) caps external work.
- **P2P fail-fast**: `p2p_timeout_seconds` (8), current Core connections first,
  last-good peer first, failed-peer cooldown, total 2× timeout per block,
  `connect_timeout`, and a Tor circuit breaker before clearnet fallback.
- **Decoys/prefetch**: `decoys = 1` (max 2 in flight), prefetch has one worker,
  enqueues at most 16 heights and never spawns decoys.
- `/status`/`/metrics` expose P2P failures/skips, decoys in flight and prefetch
  queued/done. No schema or protocol change.

## Last change (alpha.29.3)

- **P2P witness fix**: the `version` message now advertises `NODE_WITNESS`
  (bit 3) in v1 and v2. Without it peers strip witnesses from served blocks;
  the block headers/merkle still look valid but BIP141 validation failed with
  `missing witness reserved value`, leaving wallets on `Retrieving
  transaction`. This was the real cause of the Sparrow stall, not wallet
  corruption.
- **P2P clearnet fallback** when Tor connect fails (`tor_fallback_clearnet`),
  one-time warning; `.onion` peers still require Tor.
- **Fast rawtx**: `rawtx.request_timeout_seconds` (15 s, was hard-coded 60) and
  `rawtx.provider_cooldown_seconds` (120 s) skip a provider's Tor path after a
  failure instead of waiting per transaction.
- **History prefetch**: serving `blockchain.scripthash.get_history` enqueues the
  confirmed heights in a bounded, deduplicated background queue (2 workers), so
  the following `transaction.get` usually hits the block cache. Optimisation
  only, disabled when no external sources exist.
- **Real provider probe**: the readiness gate now also fetches the genesis raw
  block and verifies its header hash; a provider that cannot serve real blocks
  does not open the wallet gate. Tor warning distinguishes total Tor failure
  from per-provider clearnet fallback.
- Tests: `NODE_WITNESS` payload bit, raw-block probe match/mismatch, Tor
  fallback state; workspace tests + regtest e2e green. No schema/protocol
  change.

## Last change (alpha.29.2)

- Readiness probes now validate `/block-height/0` against the **indexed
  network genesis** (`run`/`serve`) or the known chain genesis (`doctor`).
  A provider serving another network is marked unhealthy with a `genesis
  mismatch` error and cannot open the wallet gate.
- Tests cover the matching and mismatching genesis; no protocol/schema change.

## Last change (alpha.29.1)

- **Provider readiness gate**: `run` probes `public-servers.toml` for the active
  network and does not open Electrum until one answers `/block-height/0`
  (`require_ready`, 60 s budget). `serve` probes and warns but stays offline.
  This prevents Sparrow's `UNFETCHABLE_BLOCK_TRANSACTION` NPE caused by a
  history entry whose raw tx cannot be served.
- **Tor → clearnet fallback**: `tor_fallback_clearnet = true` by default; logs
  the IP exposure and shows it in `/status`. `.onion` still requires Tor;
  `false` restores fail-closed.
- `/status`, `/metrics` and `doctor` expose provider health/latency/via.
- Provider endpoints with two clients in `el-rawtx`; no schema/protocol changes.

## Last change (alpha.29)

- **Core RPC exposure guard**: warning on non-loopback `rpc_url`;
  `[core] require_loopback_rpc = true` makes it fatal; `doctor` prints the
  transport and `/status` exposes `core_rpc_loopback`.
- No native Core RPC TLS; the documented path is an SSH/VPN tunnel.
- Roadmap cleanup: rate limiting, Core RPC guard and UTXO prefetch are done;
  systemd stays optional.

## Last change (alpha.28)

- **Per-client rate limiting**: one token bucket per connection; 50 req/s with
  burst 300 on non-loopback listeners, disabled on loopback, `0` disables.
  Exhausted requests get `-32000 "rate limit exceeded; slow down"` and the
  connection stays open.
- `/status` exposes `rate_limit_rps`, `rate_burst`, `rate_limited`; `/metrics`
  adds `e_lite_rate_limited_total`.
- No client authentication: this is defense-in-depth; firewall/allowlist/VPN/Tor
  remain the access control. Systemd is explicitly optional in the roadmap.

## Last change (alpha.27.2)

- Verified the official mempool.space onion over Tor 0.4.9.15: block 9 raw bytes
  match the requested hash and are identical to clearnet. It is now enabled in
  the shipped `public-servers.toml`.
- The Blockstream API onion candidate did not resolve over Tor and remains
  disabled.
- Existing user files are never rewritten; only first-run/new installs get the
  enabled onion.

## Last change (alpha.27.1)

- Scriptpubkey notifications now use `blockchain.scriptpubkey.subscribe` (the
  params still carry the scripthash); scripthash subscribers are unchanged.
- The 1.7 e2e subscribes before mining and asserts both notification methods,
  and selects a UTXO belonging to the mining address so the `spk_hint` path is
  real.

## Last change (alpha.27)

- **Electrum 1.7** (AUD-043): `protocol_max = "1.7"`, `server.features` omits
  `hash_function` for 1.7, and `blockchain.block.headers` keeps the 1.6 array.
- `blockchain.scriptpubkey.*` (dict results, `10001`, reorg notification),
  `blockchain.outpoint.*` (on-demand history scan, `spk_hint` verification,
  1,000 subscriptions/client), `server.ping(pong_len, data)`,
  `mempool.recent`, `blockchain.transaction.testmempoolaccept`.
- No schema/CF/undo/digest change: the apply path and snapshots are untouched.
  The e2e asserts `rawtx.fetches == 0`, i.e. local data only.
- `listunspent` is sorted in blockchain order for both namespaces.

## Last change (alpha.26.1)

- **Public providers are file-based**: `public-servers.toml` next to
  `config.toml` is the only list; there is no compiled-in fallback. A first run
  copies the shipped template, existing files are never overwritten, and a
  missing file means local data only.
- Defaults: Blockstream + mempool.space for mainnet, signet and testnet4;
  mempool onion present but commented pending a real-Tor verification.
- Clearnet URLs must be HTTPS; `.onion` is the only plain-HTTP exception and
  still requires Tor.
- `/status` exposes the effective list; `doctor` prints the resolved file,
  source and endpoints. `--public-servers` and `[public_servers]` override the
  location/generation.
- Packages ship the template; `install.sh` copies it only when absent.

## Last change (alpha.26)

- **Electrum 1.6 compatibility** (AUD-042): `protocol_max = "1.6"`.
  `blockchain.block.headers` returns `headers: []` for 1.6 clients while 1.4
  clients keep `hex`; `server.version` must be the first message for 1.6 (a late
  negotiation is capped at 1.4.2).
- **`mempool.get_info`** exposes `mempoolminfee`, `minrelaytxfee` and
  `incrementalrelayfee`.
- **`blockchain.estimatefee(number, mode)`** passes `unset|economical|
  conservative` to Core and caches per `(target, mode)`.
- **`blockchain.transaction.broadcast_package`** wraps `submitpackage` with the
  stable `{success, errors}` result; `verbose=true` returns Core's object.
- **Tests**: real parent+child package in `tests/e2e_electrum16.py`, 1.6/1.4
  headers on the same instance, first-message cap, fee modes and mempool info.
- 1.7 remains unimplemented and is not advertised.

## Last change (alpha.25.4)

- **Lenient version negotiation** (AUD-041): the server now selects the newest
  implemented version at or below the client's maximum instead of disconnecting
  when the ranges do not intersect. `1.4.3`, `1.5`, `1.6` and `1.7` requests all
  negotiate `1.4.2` today; only a maximum below 1.4 is refused.
- `SERVER_VERSIONS` is the single source of truth; add a version there only
  after all of its methods are implemented and tested.
- Tests: selection matrix (including gaps and future versions), protocol
  fallback and the existing socket-close path for `< 1.4`.

## Last change (alpha.25.3)

- **Electrum 1.4 conformance** (AUD-039): numeric version negotiation
  (`1.4`–`1.4.2`), first `server.version` only, close on incompatible ranges,
  and `server.features` advertises the new range.
- **Verbose transactions**: `blockchain.transaction.get(txid, true)` on a
  confirmed transaction now includes `hex`, `blockhash`, `confirmations`,
  `time`, `blocktime` and `in_active_chain`; mempool responses carry
  `confirmations: 0`.
- **JSON-RPC notifications** are not answered; `scripthash.unsubscribe` returns
  the real boolean; `server.add_peer` returns `false`.
- **Mempool status order** is canonical `(-height, txid)` and deterministic.
- No schema, config or index changes. 1.6/1.7 remain unimplemented and are not
  advertised.

Evidence: `docs/AUDIT.md` (AUD-039), `CHANGELOG.md`, `cargo test --workspace`,
`tests/e2e_regtest.sh` (plain + TLS + Sparrow sequence with confirmed verbose
transaction).

## Last audit/fixes (alpha.25.2)

- **Onion ports mirror the listeners**: `[tor] onion_port`/`onion_tls_port`
  default to 0 = same as `server.listen`/`server.tls_listen`, so
  `--listen 0.0.0.0:51001 --tls-listen 0.0.0.0:51002` publishes `:51001` and
  `:51002` on the onion instead of 50001/50002. Explicit values still win; the
  startup log, torrc, state file, `/status` and `onion show` use the effective
  ports.
- Evidence: unit tests for listener mirroring/override; a manual `run` with
  51001/51002 showed the matching onion ports.

## Last audit/fixes (alpha.25.1)

- **Bundled Tor libraries**: the expert bundle's `tor` has no RPATH; on Debian
  13.7 without `libevent-2.1.so.7` it exited 127. e-lite now prepends the Tor
  binary directory to `LD_LIBRARY_PATH`, preserving any inherited value.
- **Diagnostics**: unexpected Tor exits print the last `tor.log` lines;
  `doctor` runs `tor --version` with the same environment.
- Evidence: unit test for the library path, the real-Tor integration test, and a
  Debian 12 minimal container reproducing the missing-libevent case (before:
  exit 127; after: `Tor version 0.4.9.15`).

## Last audit/fixes (alpha.25)

- **Onion with wildcard listeners**: `--listen 0.0.0.0:50001` /
  `--tls-listen 0.0.0.0:50002` now publish the onion with loopback targets
  (`127.0.0.1`) instead of refusing. `[tor] target_plain`/`target_tls` override
  the target; the effective ports/targets are saved in `<data-dir>/tor/state`
  so `e-lite onion show` is accurate even when the daemon used CLI flags.
- Evidence: unit tests for wildcard/IPv6 mapping and torrc targets; a manual
  `run` with `0.0.0.0` created the onion and `onion show` reported the custom
  ports; fmt/clippy/workspace tests.

## Last audit/fixes (alpha.24)

- **Adaptive memory budget**: 75 % ceiling, `max(1024, RAM/8)` reserve,
  memtables `RAM/8`, page-cache floor, 256 MiB growth / 25 % shrink with
  two-sample hysteresis; `cache_mib` is a maximum and critical pressure can
  lower it; pre-resume shrink when Core is about to download again.
- **Managed Tor + onion**: e-lite runs its own Tor by default, publishes plain
  50001 and TLS 50002 onion ports, auto-generates a self-signed TLS certificate
  with the onion SAN, and exposes `e-lite onion show|reset`.
- **Installer Tor bundle**: downloads the official Tor expert bundle (pinned
  version + SHA-256) into `<prefix>/lib/e-lite/tor`; `--without-tor` skips it and
  `tor` in PATH is the fallback. Indexing continues with a warning if no binary
  exists.
- **Snapshots** exclude `tor/` and `tls/` (onion and private keys are not index
  data); a restored snapshot starts with a new onion address.
- Evidence: `cargo test --workspace` (memory policy, torrc, onion validation,
  self-signed SAN/permissions, fake-Tor lifecycle, reset), the ignored
  `managed_tor_real_binary_creates_onion` with the real bundle, `e2e_regtest`,
  `e2e_pruned_ibd`, `e2e_launch_core`, installer with/without Tor, and a manual
  `run` showing `Tor onion: ...:50001 (plain), :50002 (tls)` + `Tor ready`.
- **Not claimed**: no real mainnet memory benchmark for alpha.24 yet; the
  operator still validates the cache growth and commit behavior on the VM.

## Last audit/fixes (alpha.23)

- **Prevout prefetch in reader threads**: alpha.22 mainnet logs showed `apply`
  growing from ~300 ms to >1.5 s per block while `read`/`decode`/`wait` stayed
  near zero. The single apply thread was doing every UTXO point lookup
  serially. Readers now resolve a speculative prevout superset while the
  previous block is applied; apply still checks the group overlay first and
  falls back to a point lookup, so speculation cannot change the result.
  `prefetch` is a new progress-line figure (not exported to `/status`).
- **RocksDB write amplification**: `atomic_flush` across the 7 CFs (one flush
  point, fewer/larger L0 files), 2 MiB compaction readahead and a 512 MiB L1.
- Evidence: oracle-parity tests now run through
  `prepare → prefetch → apply_prepared_with_prefetch` (including spends of
  outputs created earlier in the same uncommitted group), `cargo test
  --workspace`, `e2e_regtest`, `e2e_pruned_ibd`, `e2e_launch_core`.
- **Not claimed**: no measured mainnet speedup yet. The next real run should
  report `prefetch` vs `apply`; if `prefetch` is high and `apply` low, the disk
  is the limit, not the pipeline.

## Last audit/fixes (alpha.22)

- **Cache metric removed**; `--cache-mib` and the block cache remain.
- **`commit_sync = auto`**: `txids` in bulk/IBD, `always` live. `txids.dat`
  is always fsynced; startup truncates extras.
- **Core bulk pause + pre-warm**: pause during bulk, resume 5,000 blocks early
  so peers reconnect, then relative 20k guard.
- **Prune not deferred by free space by default**; normal cadence every 1k safe blocks;
  emergency low-disk prune remains.
- **READY / FULLY SYNCED** in log, `/status`, `/metrics`, `e-lite status`.
- **Wallet banner**: built-in INDEXING/READY templates plus optional
  `[server] banner_file` with `[indexing]`/`[ready]` and placeholders.
- **Apply micro-opts**: output script hashes precomputed in readers, txid set,
  reused buffers.
- `scripts/benchmark-index.sh` for real A/B runs.

Evidence: `CHANGELOG.md`, `docs/CLI.md`, `docs/TUNING.md`,
`cargo test --workspace`, `tests/e2e_regtest.sh` (banner/READY + log),
`tests/e2e_pruned_ibd.sh`.

## Last audit/fixes (alpha.21)

- **Deterministic store tests**: global failpoint tests are serialized with a
  mutex, fixing the alpha.20 CI flake. No runtime change.

## Last audit/fixes (alpha.20)

- **Bulk lag guard fix (AUD-026)**: the relative baseline was following the lag
  down while Core was paused, so the resume threshold dropped every block and
  Core resumed immediately. The baseline is now frozen while paused; the guard
  holds until the lag reaches half the growth window.
- **Explicit readers survive hot reload**: the `2 × CPUs` cap only applies to
  automatic reader counts.
- **`--log-file` without a parent directory** works.
- **`PreparedBlock` fields are private**; only Merkle/hash-validated prepared
  blocks can be applied.

## Last audit/fixes (alpha.19)

- **`e-lite.log`**: stdout/stderr are teed to `<data-dir>/e-lite.log` (append,
  rotation to `.1`, `--log-file`/`--log-max-mib`/`--no-log-file`). Snapshot
  export skips the log.
- **Prune cadence**: `min_gain_blocks` default 10,000 (emergency low-disk prune
  still bypasses it).
- **Parallel decode/txid/Merkle** in reader threads; apply only does UTXO work
  and batch writes. Block hash is computed once.
- **Commit**: skip `prune_undo` when the group has no undo; bulk interval up to
  30 s and batch up to 4096; controller shrinks the batch when commit dominates
  the interval.
- **Metrics**: `decode_ms`, `wait_ms`; the cache-hit metric added in alpha.19
  was removed in alpha.22 because RocksDB did not report it reliably.
- **Bulk guard is relative**: `--max-bulk-lag-blocks` bounds additional growth
  from the baseline, so a 300k backlog no longer freezes Core for the whole
  catch-up.
- **Readers capped by `2 × CPUs`** unless explicitly overridden.

Evidence: `docs/AUDIT.md` (AUD-022..025), `CHANGELOG.md`, `docs/CLI.md`,
`docs/TUNING.md`, `cargo test --workspace`, `tests/e2e_regtest.sh` (log file +
startup/shutdown), `tests/e2e_pruned_ibd.sh` (prune gate with explicit 288 gain).

## Last audit/fixes (alpha.18)

- **`/proc/meminfo` parser fixed** (the colon was left attached). This was the
  real cause of `memory pressure (0 MiB)` and the 1 GiB/0 MiB fallback on every
  machine; cache/readers were being throttled unnecessarily.
- **Global RocksDB memtable budget** instead of 64 MiB per CF × 3 memtables.
  Jobs capped by CPU count.
- **Ordered startup/shutdown** (AUD-021): no pre-Core P2P/mempool errors, no
  post-shutdown `Connection refused` or tuning lines.
- **Progress**: height/Core percentages over `headers`, verification progress,
  read/apply/commit/cache metrics, start/stop local times, milestones every
  10,000 blocks or 6 h with e-lite/Core disk usage.
- **Opt-in bulk lag guard** (`--max-bulk-lag-blocks`, default 0) for managed
  pruned Core during IBD.
- **TLS listener** (AUD-018): rustls, PEM cert/key, mtime reload, 10 s handshake
  timeout, no client authentication.
- **CLI/`--set`**: every TOML field can be overridden without a config file.

Evidence: `docs/AUDIT.md` (AUD-018..021), `CHANGELOG.md`,
`cargo test --workspace` and `tests/e2e_regtest.sh` (plain + TLS + clean
startup/shutdown + log assertions).

## Open risks / next priorities

P1 (before any public exposure or mainnet production claim):

1. Authentication for non-loopback Electrum. TLS encrypts the transport and
   rate limiting is only a throttle, not access control; document
   firewall/allowlist/VPN/Tor.
2. Core RPC TLS or a hard SSH/VPN requirement with tests.
3. Verify/close the audit residual risks in `docs/AUDIT.md` (snapshot archive
   limits, per-client threads, provider trust).

Performance/storage (P2, benchmark first):

- UTXO prefetch pipeline.
- HDD sequential/readahead mode.
- History compaction/refs/compression experiments.
- Adaptive controller v2 (queue/compaction signals).

Compatibility (P2/P3):

- Electrum 1.7 (outpoint spend index), BIP352 silent payments (schema v2
  needed).
- Systemd unit/container image.
- ARM64/more distros.

## Commands

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
tests/e2e_launch_core.sh
tests/e2e_pruned_ibd.sh
tests/e2e_regtest.sh
tests/e2e_defaults.sh
tests/e2e_recovery.sh
tests/e2e_multi_instance.sh
```

Hot reload:

```bash
kill -USR1 "$(pgrep -f 'e-lite run')"
```

## Do not do without explicit approval

- Change the database schema without `schema_version` + migration/test.
- Re-enable automatic pruning by default or weaken the 288/224 safety margins.
- Expose metrics publicly.
- Expose Electrum without TLS or without a firewall/VPN/Tor access policy; TLS
  alone does not authenticate clients.
- Claim client authentication, that rate limiting is access control, Electrum
  1.7 or silent payments are implemented.
- Delete an existing index/datadir to “fix” an error before checking
  `NEEDS_RECOVERY`/snapshot options.

## Known working test baseline at handoff

`cargo fmt --check`, clippy, workspace tests and the local regtest e2e
(`launch_core`, `pruned_ibd`, `regtest` with plain + TLS Electrum, clean
startup/shutdown/memory assertions, `e-lite.log` presence, 1.6 and 1.7
negotiation on the same instance, scriptpubkey/outpoint checks, outpoint
notification, real testmempoolaccept, the rate-limit burst test, the strict
Core RPC check and the Sparrow sequence with confirmed verbose transaction)
passed on the alpha.29.5.0 code. Full signet/testnet4 acceptance
is still operator-run; results belong in `docs/GETTING-STARTED.md`/release notes
when available.
