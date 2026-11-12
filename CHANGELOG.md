# Changelog

## Unreleased

### IBD write volume, log redaction, Tor isolation, P2P peers, safer restores

No schema, protocol or snapshot-format change; the database written by the
new undo path is byte-identical to the old one (same canonical digest).

- **AUD-131 (IBD)**: undo is built for every applied block but kept in
  memory, trimmed to the reorg window, and only the records inside the
  window of the committed tip are written. Before, every block's undo went
  into the `WriteBatch` and most of it was deleted again in the same or the
  next commit — an estimated third of the RocksDB write volume during IBD.
  Early commits (time/tx/byte limit, boundary, shutdown) stay fully
  rewindable. Not yet measured on mainnet: compare `tests/bench_ibd.sh`
  and the `commit-group` / WAL counters.
- **AUD-132 (privacy, logs)**: `e-lite.log` no longer records the script
  hash of a failed `get_history`, nor the txid/block hash of a failed or
  rejected raw transaction (height and error class only; 64-hex values in
  error details become `<hash>`). reqwest errors are logged without their
  URL, which contained the block hash.
- **AUD-133 (privacy, fingerprint)**: HTTP requests send a generic
  `User-Agent` (`Mozilla/5.0`) instead of `e-lite/<version>`, and the P2P
  `version` message a common Core subversion instead of `/e-lite:0.1/`.
- **AUD-135 (privacy, Tor)**: every Esplora request over Tor uses a fresh
  random SOCKS username, so Tor puts each block on its own circuit.
- **AUD-137 (P2P)**: the P2P source exists even when Core had no peers at
  start, and `el-p2p-peers` refreshes the discovered list from Core every
  10 minutes (60 s while empty). Pinned `[rawtx] p2p_peers` are untouched. An
  empty P2P source does not count as a repair/rawtx source.
- **AUD-140/141/142 (snapshots)**: `export-snapshot --output` computes the
  canonical digest once instead of twice and prints a reminder to keep it.
  `import-snapshot` extracts into `.<name>.import-partial` next to the target
  and renames it only after every check, so a failed or interrupted import
  leaves nothing half-restored (a leftover is removed by the next import).
  New `--expect-digest <hex>` refuses an archive whose recomputed digest is
  not the one recorded at export.
- **AUD-144**: an Esplora body without `Content-Length` is read through an
  8 MiB + 1 bound instead of being buffered whole first.
- Tests: `deferred_undo_matches_write_then_prune_and_rewinds`,
  `undo_window_is_measured_from_the_real_commit_tip`,
  `errors_and_logs_do_not_carry_the_block_hash`,
  `requests_do_not_identify_e_lite`, `each_tor_request_gets_its_own_circuit`,
  `chunked_body_above_the_limit_is_rejected`,
  `p2p_source_counts_only_once_it_has_peers`,
  `p2p_peers_refresh_soon_when_empty_and_honour_pinned_peers`,
  `import_is_atomic_and_checks_the_expected_digest`; `version_advertises_witness`
  now also checks the user agent.

### Snapshot secrets, provider gate and prefix bloom (AUD-126, AUD-127, AUD-129)

No schema, protocol or snapshot-format change. An existing index opens
without migration.

- **AUD-126 (P1, security)**: `export-snapshot --with-core` now packs Core
  through an **allowlist**: only `blocks/`, `chainstate/` and `indexes/` of
  the exported network. Before, it walked the whole datadir minus a few
  names, so a legacy root `wallet.dat` (or a wallet file with any other
  name), `onion_v3_private_key`, `i2p_private_key`, `bitcoin.conf` (may hold
  `rpcpassword`), `settings.json` and other networks' subdirectories could
  enter the archive. The export prints what it left out ("not copied"). A
  restore no longer brings `bitcoin.conf`; e-lite passes the managed Core's
  flags on the command line, so a managed node needs nothing from it.
- **AUD-127 (P1)**: the `run` provider gate is skipped when Core is **not
  pruned** — every raw transaction is local, so Electrum opens at once instead
  of waiting for public servers. For a pruned Core it stays fail-closed by
  default (AUD-045: Sparrow persists a failed `transaction.get`); the new
  `[public_servers] open_without_provider = true` opens the listeners with
  local data after `ready_timeout_seconds`. When listeners open without a
  ready provider (`run` opt-in or `serve`), an `el-providers` thread re-probes
  every 60 s with the same wallet-free requests and logs the recovery. With
  `require_ready = false` the start-up probe no longer waits
  `ready_timeout_seconds` before opening.
- **AUD-129 (P1, wallet speed)**: `history`, `utxo_by_script` and
  `tx_occurrences` get a 32-byte fixed-prefix extractor; prefix scans use
  `prefix_same_as_start` and digests/full scans use `total_order_seek`. The
  bloom filter now rejects an unused script/txid per SST without reading data
  blocks (gap-limit scans are mostly unused addresses), and those three
  families store prefix-only filters (smaller table readers). SSTs written by
  older releases have no extractor and are read as before until compaction
  rewrites them. Not yet measured on mainnet: compare a cold wallet scan and
  `/status.rocksdb.table_readers_bytes` before/after.
- Tests (first batch): `core_allowlist_only_descends_into_chain_data`,
  `mainnet_core_walk_leaves_wallets_keys_and_config_out`,
  `provider_gate_skips_unpruned_core_and_expires`,
  `prefix_scans_stop_at_the_script_boundary` (includes the RocksDB
  seek-filter counter), `index_written_without_prefix_extractor_still_reads`.

## 0.1.0-alpha.29.10.0 — 2026-11-11

### Full per-network snapshots: index + Bitcoin Core in one archive

No schema, protocol or consensus change to the index itself; the snapshot
manifest moves to **format 2** (format 1 archives still import).

**`export-snapshot --with-core`** bundles the network's Core datadir with the
Electrum index so a backup restores both halves of a pruned from-zero node:

- The datadir is only consistent while Core is stopped: the export **refuses**
  while any Core answers, then starts a probe instance (`-connect=0
  `-listen=0``, small cache), records tip height/hash, prune height and
  subversion via RPC, stops it and only then packs `bitcoin/`. Non-loopback
  RPC URLs are refused (a remote datadir cannot be copied from here).
- Wallets, `mempool.dat`, `peers.dat`, `banlist.dat`, `.cookie`,
  `anchors.dat`, logs and lock files are never copied.
- The manifest records the Core section plus a per-file table (sizes always,
  SHA-256 with `--hash-files`), and enforces the pair rules at export:
  `index tip <= Core tip` and, when pruned, `prune_height <= index tip - 288`
  (the undo window). Compression is gzip **level 3** by default
  (`--compress-level 0..9`; 0 stores). The archive writes to `<output>.tmp`
  and is renamed atomically.
- Index-only exports (format 1 behaviour) no longer include the nested
  `<data-dir>/bitcoin` at all — previously the "index" tar silently packed
  the whole Core datadir (AUD-124).

**`import-snapshot --input <file>`** restores into an empty target and, for
format 2, verifies the Core section: manifest file sizes (always) and
SHA-256 hashes (`--verify`), `index tip <= Core tip`, the 288-block prune
margin and the manifest's Core tip. **`--verify=full`** opens the restored
datadir with an isolated Core (`-connect=0 -listen=0`), checks that the Core
tip matches the manifest and the index-tip block exists, runs a bounded
`verifychain(checklevel=2, nblocks=288)` and stops the probe.

Cross-network use: run the command per network (`--network mainnet`,
`testnet4`, `signet`); each archive is self-contained for that chain.

Tests: 9 new snapshot unit tests (manifest v2 round-trip, coherence rules,
index/Core walk exclusions, SHA-256, gzip level, sparse acceptance, wrong
network/unknown format rejection) and `tests/e2e_snapshot.sh` (export refusal
while Core runs, index-only without `bitcoin/`, full export with hashes,
import + digest equality, second import rejected, `--verify=full` OK).

## 0.1.0-alpha.29.9.2 — 2026-11-11

### Live-mode prune batching on the day scale

No schema, protocol or consensus change. Follow-up to the 29.9.0 caught-up
prune: `min_gain_blocks = 5,000` is the right batching while Core downloads
(the gain grows thousands of blocks per hour), but once fully synced the
safe cut advances **~144 blocks/day** at mainnet rates — the IBD-scale gain
would hold ~7.5 GiB of dead data for ~35 days before the next prune.

**`[pruning] min_gain_live_blocks = 144`** (new, default): once the index is
caught up with a Core out of IBD, the prune gate uses this gain instead of
`min_gain_blocks` — one prune per day (~220 MiB retained), churn negligible.
`0` uses `min_gain_blocks` in live mode too. Validation caps it at
`min_gain_blocks`. During IBD/drain nothing changes; the emergency path
(free space below `prune_defer_free_gib`) still bypasses gain and interval.

Full chain at the tip: prune to the safe maximum once (29.9.0), then keep
~one day of tail (~220 MiB), prune daily, prune instantly if the disk
presses. Durability unchanged: durable tip behind the WAL barrier, cut
capped by `index - 288` and Core's `tip - 288`, retry after backoff.

Tests: `caught_up_live_gain_uses_the_live_scale` (live gain 2 < 3: no prune;
gain 3: prune, with a 10,000-block IBD gain present); the IBD batching test
is untouched. Workspace 235.

## 0.1.0-alpha.29.9.1 — 2026-11-11

### Automatic jobs at startup, honest live re-syncs and the dynamic ZFS ARC

No schema, protocol or consensus change.

**The startup plan now gives fast disks enough compaction threads.** The
29.8.8 rule (`background_jobs = physical cores`, 2..8 on SSD/NVMe) was only
applied on the profile-reload path; `MachinePlan::detect` kept the old
`physical/2` and an automatic 4-core host still planned `jobs = 2` (visible
in the 29.8.9 mainnet log). Both paths now use the same rule, capped by disk
kind: SSD/NVMe 8, ZFS/virtual/unknown 4, HDD/network 2. On the 4-core ZFS
host the plan is now `jobs = 4` with no explicit config.

**READY / FULLY SYNCED print once per session.** In live mode Core gets
ahead by a block and the index re-syncs every few minutes, so the two INFO
lines repeated forever. The first transition of each kind keeps the INFO
line; later re-syncs log at `debug:` (`scheduler: fully synced again ...`).

**Dynamic ZFS ARC cap (default on).** The ARC is ZFS's RAM cache: at a
97-98 % hit rate it makes pool reads nearly free, but the kernel does not
count it in `MemAvailable`, so at ~20 GiB it starved the memory plan. e-lite
now manages the cap per phase on the 5 s `el-tune` tick (5 min dwell):

```text
[index]
zfs_arc_dynamic = true
zfs_arc_max_mib = 0      # IBD/drain cap; 0 = half of total RAM
zfs_arc_min_mib = 1024   # fully synced cap (floor 512)
```

- Core in IBD or the index draining: cap grows to `zfs_arc_max_mib`
  (or half of RAM) — the read warmth is worth it.
- Fully synced: cap drops to `zfs_arc_min_mib`, the RAM becomes visible to
  `MemAvailable` again and the cache/memtable plans recover.
- Writes go to `/sys/module/zfs/parameters/zfs_arc_max` (bytes) only when
  e-lite can write it (root or a writable parameter, probed by writing the
  current value back — a no-op on success). Without permission e-lite logs
  it once and keeps the warn-only mode. Every change logs
  `arc: cap X -> Y MiB (phase; ZFS ARC managed dynamically)` and the
  hourly uncapped-ARC warning yields to the manager.
- `zfs_arc_dynamic = false` freezes whatever cap is in place.

## 0.1.0-alpha.29.9.0 — 2026-11-11

### Prune to the safe maximum once the index is caught up

No schema, protocol or consensus change. Found on the mainnet ZFS host: a
from-zero IBD finished with Core's `pruneheight` frozen at **886146** while
the index sat at ~975k — Core was holding ~89k already-indexed blocks
(~140 GiB) because normal pruning is deferred while free space stays above
`prune_defer_free_gib = 20` (the host had ~170 GiB free). The safe cut
(`index - 288`) was ~90k blocks ahead and `min_gain_blocks` (5,000) was
already satisfied: the free-space deferral was the only blocker, and it never
expires on its own.

**The deferral ends at the tip.** The prune gate now bypasses the free-space
deferral when Core is out of IBD **and** the index matches Core's tip: the
next supervisor tick prunes to the safe maximum, releasing everything behind
it. Live mode then batches again under `min_gain_blocks` (default 5,000 —
roughly one prune per day at mainnet block rates), so there is no per-block
churn. During Core's IBD the behaviour is exactly as before.

Durability is untouched: the prune still starts from a durable tip behind
the `SyncWAL(true)` barrier, is capped by the committed tip, the undo window
(`index - 288`) and Core's own retention (`tip - 288`), and a failed request
retries the same safe height after backoff.

```text
[prune gate, caught up, ~170 GiB free]
before:  prune 886146 (deferred forever, ~140 GiB held)
after:   prune 975124 (safe max), then batching at min_gain
```

Tests: `caught_up_prune_bypasses_the_free_space_deferral` (bypass at the tip,
deferral still holds during IBD) and
`caught_up_live_mode_batches_after_the_transition_prune` (live batching).
`docs/AUDIT.md` AUD-122 records the evidence.

## 0.1.0-alpha.29.8.9 — 2026-11-11

### The index now reaches the tip: the last-blocks stall and honest READY

No schema, protocol or consensus change. Found on the mainnet console log of
a 29.8.7 run: the index drained to 2 blocks behind Core's tip (height 975409,
`lag 2`, "(100.0%)" on screen) and then stopped for the rest of the session —
no commits, no `READY`, no `FULLY SYNCED`, only memory lines.

**The tail could stall forever.** With fewer blocks left than the commit
minimum, every 500 ms catch-up call rebuilt the same 2-block target list and
the end-of-call commit could be skipped, so the call returned Ok without
progress and without a single log line. Three fixes together:

1. **A clean end-of-call always commits the applied prefix.** Undo is written
   for every applied block, so the commit is the cheap step; skipping it made
   the tail refetch the same blocks forever. Under disk pressure it now
   commits first (a `debug:` line notes it) and the disk guard still stops
   the *next* call from fetching more.
2. **The reader wait is bounded (10 s).** A wedged reader cannot block the
   apply loop silently anymore; on timeout the call ends with a
   `debug: pipeline:` line and the next call gets fresh readers.
3. **Readers that exit at the end of the target list return their permit.**

The `debug:` log (alpha default) now also traces the pipeline so the
signature is visible in the log alone:

```text
debug: pipeline: call start 975410 target 975411 wanted 2 ...
debug: pipeline: no progress: committed 975409 of target 975411 (wanted 2,
       applied 0, discarded 2, paused_by_disk false, disk_guard false)
```

(real-work calls log `pipeline: call start/end` with wanted/applied/discarded;
the no-progress note is rate-limited to one line per 10 s).

**`READY` no longer lies.** `sync.ready` required only an exact tip match, so
it fired during Core's IBD whenever the index briefly matched Core's short
tip ("READY ... Core still in IBD"). It now also requires Core to be out of
IBD, like the state machine already did.

**The percentage cannot fake the finish.** The height line capped at 100.0 %
by rounding (99.9998 % displayed as `(100.0%)` with lag 2). While blocks
remain it now caps at 99.9 %; the real 100 % is the `FULLY SYNCED` line.

**The catch-up event is visible.** `READY` / `FULLY SYNCED` include the
session time (`session 23m 18s`), so the sync duration is in the log at the
moment it completes.

## 0.1.0-alpha.29.8.8 — 2026-11-11

### Log levels, a bolder compaction plan and the ARC warning

No schema, protocol or consensus change. Three pieces from the mainnet A/B
logs of 29.8.6/29.8.7 (4C/8T, 32 GiB, ZFS pool).

**Log levels: `info` < `debug` < `trace`.** `[logging] level` / `--log-level`,
hot-reloadable via SIGUSR1 like every other hot field. The historical
`[e-lite]` lines are INFO and their format is unchanged, so existing parsing
keeps working; the new detail prints as `[e-lite] debug:` / `[e-lite] trace:`
lines. Alpha default is `debug`; stable releases will default to `info`.

At `debug` the log gains, every 10 minutes next to the `rocksdb:` line:

```text
debug: rocksdb-cf meta: l0 2 pending 12 MiB memtable 0 MiB sst 3200 MiB readers 10 MiB ok true   (one per family)
debug: rocksdb-levels: l0 27 l1 42 l2 108 l3 913 files (summed)
debug: rocksdb-stats: cache hit 93% (1.2M hits / 88K miss), compact r/w 2150M/3080M,
                      flush w 512M, wal w 1440M, wal fsync p50/p95/max 2/8/1200µs,
                      get p50/p95/max 0/2/90µs
```

and next to every hot `memory:` line:

```text
debug: cache-controller: shrink 2928 -> 2650 MiB (safety (...); available 4698 MiB, reserve 2048 MiB, dynamic ceiling 2650 MiB, profile cap 7994 MiB)
debug: io-pressure: some 1.20/0.80 full 0.00/0.10 avg10/avg60 | arc hit 97%
```

RocksDB statistics are enabled with `ExceptDetailedTimers` (WAL sync, get and
write timers kept; mutex and compression timers skipped). `trace` adds one
line per commit group (duration, blocks/txs/bytes, txids vs db split, sync
flag). Phase transitions print the byte-window arithmetic (`scheduler:`), and
`/status.rocksdb.cf` now includes `level0_files` per family.

**The compaction plan no longer starves on fast disks.** With the old plan
(`physical/2`, 2..4) a 4-core host ran one flush and one compaction thread;
the 29.8.7 log showed `pending_compaction` growing ~70 GiB/h at a 3.5 blocks/s
drain while the same window with four jobs stayed flat. SSD/NVMe profiles now
plan `background_jobs = physical cores` clamped to 2..8 (2 flush + N/2
compaction on a 4-core host); HDD keeps 2 (extra compaction threads only add
seeking) and virtual/unknown keeps the conservative `physical/2`. Explicit
`[index] background_jobs` still wins. The next mainnet log validates it:
flat pending debt, commit/blk back around 60-100 ms.

**Uncapped ZFS ARC warning.** ARC is invisible to `MemAvailable`; at ~20.5 GiB
it starved the dynamic cache (1.3 GiB against a 3.3-8 GiB plan) while the
controller acted on a false signal. e-lite now warns at startup and, at most
once per hour, whenever ARC exceeds a quarter of RAM, with the suggested cap
(`min(25% RAM, 8 GiB)`). A host capped at exactly that value never triggers
the warning.

## 0.1.0-alpha.29.8.7 — 2026-11-10

### Two measurement fixes found by the first 29.8.6 mainnet log

No schema, protocol or consensus change. The 29.8.6 diagnostics did their
job: the first log exposed that two counters printed wrong values.

**`block_cache` was multiplied by seven.** With a shared block cache,
RocksDB reports the usage of the whole cache for *every* column family
(documented as "the same for every column family"), so summing
`rocksdb.block-cache-usage` across the seven families multiplied the real
value by the CF count. The 29.8.6 log showed `block_cache 18854 MiB` next to
a 2705 MiB capacity (18854/7 = 2693); the previous DB-handle read returned 0
because the empty default family reports 0. The value is now read once
through the first data family that answers; expected reading is the live
cache usage, ≈ the capacity shown in the `memory:` line.

**ZFS ARC always printed 0.** The `arcstats` kstat rows are `name type
value`; the reader took the second column (the data type `4`) as the byte
value, so `arc 0 MiB` on every ZFS host regardless of the real ARC size. It
now reads the third column. On a ZFS pool this number matters: a capped or
empty ARC makes cold reads pay the pool directly, which showed up as
602-4850 ms block-read spikes in the same log. The value is unit-tested
against a realistic kstat layout.

The `l0` file count stays summed per family (that property really is
per-CF); `pending_compaction` stays an estimate whose trend is the signal.
No controller, default or budget change.

## 0.1.0-alpha.29.8.6 — 2026-11-10

### Compaction diagnostics: real block cache, L0 files and commit-spike context

No schema, protocol or consensus change. Follow-up to the mainnet log at the
end of IBD (96-99 %, ~5 blocks/s): `pending_compaction` grew monotonically
49 → 185 GiB in one hour with `running 1` the whole time, commit/blk degraded
from 57-80 ms to 150-210 ms, and one 45.5 s commit left no diagnostic trail.
Two of the counters needed to explain it were broken or missing.

**`block_cache` was always 0.** `rocksdb.block-cache-usage` is a per-CF
property: read through the DB handle it only reports the empty default family,
so the `rocksdb:` log line and `/status.rocksdb` showed `block_cache 0 MiB`
while the memory line showed a live cache ceiling. It is now summed across all
seven column families like the other per-CF properties.

**L0 file count is now visible.** `CompactionStats` gains `level0_files`
(summed `rocksdb.num-files-at-level0`). A growing L0 with one running
compaction is the actual flush/compaction stall signal; the
`estimate-pending-compaction-bytes` sum is an estimate that inflates while
ingest continues and cannot distinguish that on its own. The 10-minute
`rocksdb:` log line and `/status.rocksdb` now print:

```text
[e-lite] rocksdb: pending_compaction {} MiB, running {}, l0 {} files,
         memtable {} MiB, block_cache {} MiB, sst {} MiB,
         table_readers {} MiB, properties_ok {}
```

**Commit spikes explain themselves.** A full commit group that lasts at least
10 s and either overshoots twice the configured bulk time limit (20-60 s by
disk class) or costs 5x the previous full group's per-block cost now logs its
context, sampled at the spike: running compactions, L0 files, real memtables,
pending compaction and the Core backlog. Normal bulk groups (the full interval
on HDD/network included) stay silent; boundary commits never trigger it.

```text
[e-lite] commit spike: 45547 ms for 60 blocks / 154886 txs (92 MiB), reason Txs;
         rocksdb running 1, l0 23 files, memtable 218 MiB, pending_compaction
         184896 MiB; lag 10436 blocks; previous per-block cost 67 ms
```

Nothing about the controller, defaults or memory budget changed. To compare
compaction throughput, set `[index] background_jobs` (0 = plan: physical/2,
2..4; 4 turns one flush + one compaction thread into two of each) and watch
`running`, `l0` and the `pending_compaction` trend in the `rocksdb:` lines.

## 0.1.0-alpha.29.8.5 — 2026-11-10

### Dynamic cache ceiling without double-counting resident memory

No schema, protocol or consensus change. Follow-up to the 29.8.4 mainnet log:
the hot cache collapsed to 8-30 MiB while `available` was 2-5 GiB and
`reserve + memtables + runtime` was sized from a much larger startup value.

**The dynamic cache ledger no longer double-counts resident memory.**
`MemAvailable` already excludes memtables, runtime and RocksDB table readers.
The old formula reconstructed `effective` by adding the planned memtable and
runtime budgets to `MemAvailable`, then subtracted them again from the cache
ceiling, so those budgets were counted twice and the cache was starved to
`block_cache = 0`. The ceiling is now:

```text
reserve_dyn  = min(startup_reserve, max(1024 MiB, MemAvailable/2))
cache_dyn    = min(profile, MemAvailable - reserve_dyn)
```

On the 29.8.4 log sample (`available = 2870 MiB`, `reserve = 3066 MiB`) the
expected cache ceiling goes from 30 MiB to **1435 MiB**. Cache decisions,
pre-resume shrink and adaptive memtables use the same dynamic reserve.

**Visible, not planned.** `/status.memory` and the hot memory line now expose:

```text
effective_mib          MemAvailable (post cgroup), used as the cache basis
dynamic_reserve_mib    reserve_dyn
memtable_bytes_mib     actual RocksDB memtable memory
table_readers_mib      actual RocksDB index/filter memory
```

The hot log line also prints cache/profile cap, planned vs real memtables,
table readers, pending compaction and the dynamic/startup reserve.

**RocksDB descriptors.** `[index] max_open_files` / `--max-open-files`
(0 = 8192) lets an 8-16 GiB host lower the table-reader block (2048/4096) and
A/B it against the data cache.

Tests: fmt, clippy, workspace (230), including the dynamic-reserve test over
the exact `available = 2870 MiB` sample from the log. The log line and
`/status.memory` now keep planned and actual memtables apart.

## 0.1.0-alpha.29.8.4 — 2026-11-10

### cgroup I/O priority, prune defaults and the A/B harness

No schema, protocol or consensus change.

**cgroup v2 I/O priority.** `[core] cgroup_io_weight = 1..10000` (CLI
`--core-cgroup-io-weight`) creates an `e-lite-core` child under the current
cgroup v2, enables `+io` where permitted, writes `io.weight` and moves a Core
that e-lite launches or adopts into it. It is best-effort: without delegation
or the `io` controller it logs a warning; `ionice` remains the fallback.

**Prune deferral defaults.** Normal pruning now keeps a **20 GiB** free-space
margin (`prune_defer_free_gib = 20`) and advances every **5,000** safe blocks
(`min_gain_blocks = 5000`). Low-disk emergency pruning still bypasses both.
`tests/e2e_pruned_ibd.sh` sets `prune_defer_free_gib = 0` explicitly because it
must observe the gate on a small temporary disk.

**A/B harness.** `tests/ab_umbrel.sh` samples `/status` for a labelled run and
writes a CSV with phase rate, backlog, cache/memtables, readers, p95, commit
group/reason and state. `docs/AB_TESTS.md` defines the baseline/readers/cache/
bulk/alternate matrix and the decision thresholds. The A/B itself must run on
the Umbrel; this release provides and validates the harness.

Tests: fmt, clippy, workspace (229), e2e defaults/regtest/pruned/recovery/
multi/launch. The cgroup parser has unit tests; the A/B script was smoke-tested
against a local regtest `/status`.

### Not done automatically

- The actual mainnet/Umbrel A/B decisions (readers, cache, `max_bulk`,
  `alternate`) need the user's machine; defaults remain conservative.
- `adaptive_memtables`, `prefetch_deadline_ms`, `io_priority` and
  `cgroup_io_weight` are opt-in.

## 0.1.0-alpha.29.8.3 — 2026-11-10

### Adaptive memtables, prefetch deadline, ionice and explicit ZFS/overlay

No schema, protocol or consensus change.

**Opt-in hot memtable resize.** `[index] adaptive_memtables = true` (CLI
`--adaptive-memtables`) lets the controller resize the shared
`WriteBufferManager` **and** every CF's `write_buffer_size` from the live
`effective/16` target (512 MiB..2 GiB). A target must persist for 10 minutes,
changes are at least 10 minutes apart, cache growth/shrink has priority, and
the explicit `write_buffer_mib` disables it. The growth condition requires
room for cache + target + margin; shrink runs when the cache is at its floor
and available memory is below reserve + 512 MiB. The critical-memory one-way
halving remains.

**Prefetch deadline.** `[index] prefetch_deadline_ms` (50..60000, 0 = off)
splits a block's prevout prefetch into 512-key chunks and stops after the
deadline; apply already falls back to point lookups for missing entries.
Opt-in because it trades a slow reader for more apply work.

**Core I/O priority.** `[core] io_priority = 0..7` (CLI
`--core-io-priority`) applies best-effort `ionice -c2 -n<prio>` to a Core that
e-lite launches or adopts. Failure or a scheduler that ignores it (NVMe
`none`) is a warning, not fatal. Off by default.

**Disk classes.** `DiskKind` now distinguishes `zfs` and `overlay` explicitly;
`preflight` warns with fstype/source and the startup plan prints them. Both use
the local-SSD cache/reader profile, and ARC is visible in the memory line.

**Tests:** fmt, clippy, workspace (228), including the hot memtable resize
against a real RocksDB and the prefetch deadline path.

### Not defaulted

- `adaptive_memtables`, `prefetch_deadline_ms` and `io_priority` are opt-in;
  no default changes.
- Prune deferral and `max_bulk_lag_gib`/readers/cache A/B still require
  mainnet measurements.

## 0.1.0-alpha.29.8.2 — 2026-11-10

### Coherence pass: Core reservation, hot cache signals and diagnostics

No schema, protocol or consensus change. Follow-up to 29.8.1; 29.8.1 remains
the memory-policy release.

**Core reservation is real.** `run`/`index` probe the RPC port before the
memory plan. If Core is already running (or external), its `-dbcache` is already
inside `MemAvailable` and is **not** subtracted again. `serve` never reserves a
future Core cache. An operator `-dbcache=...` in `core.extra_args` is parsed and
used for the reservation instead of the fitted value.

**Cache growth uses p95, not a tail EWMA.** The tuner keeps the last 256
`prefetch`/`wait` samples and publishes p50/p95/max in `/status.tuning` and the
progress line. `memory_step` grows the cache only when p95 (not the 1/8 EWMA)
says prefetch is slow.

**Byte pause/resume dwell.** A byte watermark must persist for 10 s before
pausing or resuming Core; a single large-block burst cannot flip the phase.
Reader pressure still pauses immediately.

**Observability.**
- `machine plan` prints `fstype`, `source`, device and `same_device`.
- The memory line prints SReclaimable, Shmem, ZFS ARC (when present) and
  process RSS; `memory plan` already prints total/available/effective/core
  reserve.
- `/status.scheduler` adds `avg_block_bytes`, `phase`, `phase_blocks` and
  `phase_elapsed_ms`.
- `/status.tuning` adds `prefetch_p50/p95/max` and `wait_p50/p95`.
- The progress line prints phase name/rate, average block size, prefetch
  p50/p95/max, wait p50/p95, commit group size and reason.
- A `rocksdb:` line every 10 minutes reports pending compaction, running
  compactions, memtable/block-cache/SST/table-reader bytes and
  `properties_ok`.
- An unknown data-disk class prints a ZFS/overlay warning with the detected
  fstype/source.

**Intensive-sync lock.** `index_loop` acquires `IbdLock` **before** the first
512-block call when Core is in IBD or the lag exceeds 5,000 blocks. Previously
the first call could run for ~9 minutes without the slot; the log showed
`intensive-sync slot acquired` only at 0h09m.

**Alternate scheduler guard.** If `scheduler = "alternate"` is configured with
a Core e-lite cannot pause (external/already running), e-lite warns and keeps
`overlap`; `/status` no longer advertises an inactive mode.

**Tests:** fmt, clippy, workspace (224 tests). New tests cover p50/p95
percentiles, the 4 GiB network budget and the existing 8/16 GiB matrix. The
memory plan keeps `planned + reserve <= effective`.

### Not yet done

- Memtables are normally fixed at startup. Under critical memory
  (`MemAvailable < 256 MiB`) the shared `WriteBufferManager` is halved (never
  below 256 MiB) and the flush is logged; routine hot grow/shrink remains
  future opt-in work because it can stall commits.
- `ionice`/cgroup I/O priority and prefetch deadlines are not implemented.
- `alternate` remains opt-in; no scheduler default change.
- No mainnet/Umbrel measurement of the 29.8.2 diagnostics yet.

## 0.1.0-alpha.29.8.1 — 2026-11-09

### RAM budget follows available memory, not installed RAM

No schema, protocol or consensus change. This is a memory-policy release for
the 8-16 GiB machines that are the normal deployment; the 32 GiB host is the
outlier.

**Startup budget from `MemAvailable`.** `budget_with` received `available_mib`
but calculated every knob from total RAM. It now computes

```text
effective = min(total, available - core_reserve)
```

and derives reserve, ceiling, runtime, memtables, cache start, cache floor and
page-cache floor from `effective`. `core_reserve` is the fitted `-dbcache` when
e-lite is about to start Core; external Core and an already-running Core are
already included in `MemAvailable`. On an 8 GiB box with 6 GiB available the
plan is ~2.2 GiB of e-lite cache/memtables/runtime instead of 7 GiB; on 16 GiB
it is ~4-5 GiB; on 32 GiB it still grows with what is free.

**Cache ceilings scale with RAM.** Local SATA/unknown/virtual now allow
`RAM/4` (2 GiB at 8 GiB, 4 GiB at 16 GiB, 8 GiB at 32 GiB) instead of `RAM/8`;
HDD uses `RAM/8` and network `RAM/8`. The ceiling is not an allocation: the
hot controller only grows when `MemAvailable` leaves a proportional margin.

**Stable hot cache.** The cache controller now:

- uses `available < reserve` (not `reserve + page_cache_floor`, which
  double-counted reclaimable page cache) as the shrink trigger;
- shrinks immediately when the current cache exceeds its dynamic ceiling;
- requires two samples and a 5-minute reversal dwell, with a fast path below
  1 GiB;
- grows with a margin of `(effective/8).clamp(256 MiB, 2 GiB)`, not a fixed
  2 GiB;
- recomputes the ceiling as
  `min(profile_cap, available + current_e-lite - reserve - memtables - runtime)`.

`shrink_for_core_resume` is no longer a no-op: it acts only when
`MemAvailable < reserve + cache/4`.

**Core and memtables.** Managed Core's `-dbcache` is now
`min(total/8, available/2, 4096 MiB)`: ~1 GiB on 8 GiB, 2 GiB on 16 GiB, 4 GiB
on 32 GiB, smaller when the OS has little free memory. Memtables are
`clamp(effective/16, 512 MiB, 2048 MiB)` at startup. They are not resized hot
in this release: `WriteBufferManager::set_buffer_size` can force flushes and
stall commits; the cache is the safe hot knob.

**Block-size watermark.** `avg_block_bytes` uses a 1/1024 EWMA instead of 1/8,
so a few large blocks cannot move the byte backlog by 3-4x between pause and
resume.

**Commit controller.** `batch_blocks` is capped at 512 (the catch-up
checkpoint) and only grows for a `Blocks`-limited group. `Time` groups may grow
the interval; `Txs`/`Bytes` groups leave both alone. This removes the
`batch -> 1152` lines while real groups were 47-104 blocks.

**Logs.** Startup/doctor print total, available, effective, core reserve,
reserve, page floor, ceiling and cache start..max. The hot memory line prints
the dynamic and profile caps and the effective budget.

Tests: fmt, clippy, workspace; new unit tests cover the 8 GiB and 16 GiB
budgets, the dynamic cache action, the long block average and the batch cap.
e2e defaults, regtest, recovery, pruned-IBD, multi-instance and launch-Core
pass.

## 0.1.0-alpha.29.8.0 — 2026-11-09

### Byte backlog watermarks, opt-in alternate scheduler, commit/reader fixes

No schema, protocol or consensus change.

**Backlog is measured in bytes.** The bulk guard now pauses Core at
`max_bulk_lag_gib` (30) of unindexed data and resumes it at
`core_resume_ahead_gib` (7), using an EWMA of the raw block size
(`/status.scheduler.backlog_bytes`). A 20k-block lag is ~10 GiB at 100k and
~30 GiB at 800k, so the old block window was not a disk bound.
`max_bulk_lag_blocks`/`core_resume_ahead_blocks` remain as legacy fallbacks
when the corresponding GiB value is `0`. An existing config that sets only the
block fields must add `max_bulk_lag_gib = 0` to keep the old window, because the
new byte default (30 GiB) otherwise wins.

**`scheduler = "overlap" | "alternate"`.** `overlap` stays the default.
`alternate` (opt-in) stops e-lite indexing while Core downloads and stops Core
while e-lite drains the byte backlog, so only one writes at a time; serving
continues. It requires `pause_core_during_bulk = true`. The measured
break-even is Core alone > ~6.7 blocks/s; short phases lose to the switch cost.

**Commit controller uses full groups only.** The last commit records blocks,
txs, bytes, reason, cycle and the `txids`/RocksDB split
(`/status.tuning.commit_last_*`). Boundary and shutdown partial commits no
longer update the EWMA or the cycle. A `Txs`-limited group no longer shrinks
the batch, and an I/O-bound group (`db_us > 2 × txids_us`) is left alone
instead of being shrunk blindly.

**Reader control requires two samples and starvation.** More readers are added
only when `wait_ms > 50` and prefetch is the constraint; a high `read_ms` alone
no longer removes readers when `wait_ms` is high. Changes need two consecutive
samples and never go below half the physical cores (min 2).

**Disk identity.** `DiskInfo` now keeps `major:minor` and the mount source, and
`same_device` falls back through `major:minor`, parent block device and mount
source. This fixes `same=false` on bind mounts and paths such as `/data`.

Tests: fmt, clippy, workspace; new unit tests cover byte watermarks at
different heights, boundary commits not feeding the controller, tx-limited
reasons, the two-sample reader streak and disk identity. The e2e scripts run
regtest, recovery, multi-instance and pruned-IBD (prune + SIGKILL + restart).

## 0.1.0-alpha.29.7.1 — 2026-11-09

### Complete the machine-plan safety pieces

Follow-up to 29.7.0; no schema, protocol or consensus change.

**RPC retry on timeout/EAGAIN.** A read timeout on a busy Core surfaces as
`EAGAIN` (`os error 11`). `post_with_timeout` now retries the same request once
after 150 ms when the error is a socket timeout/reset/broken pipe. The RPCs
e-lite uses are idempotent (reads, `setnetworkactive`, `pruneblockchain`,
`sendrawtransaction`), so a duplicate is harmless. HTTP/RPC errors are not
retried. A unit test covers the classification.

**Bulk commits have a byte bound.** The group is committed when the raw block
bytes reach `clamp(RAM/128, 64 MiB, 256 MiB)` (256 MiB on 32 GiB) in addition
to 250k transactions, 512 blocks or 20-60 s. This keeps the `WriteBatch` and
overlay memory bounded on blocks that are large but transaction-light.

**Prune is deferred until after the commit barrier.** The supervisor only runs
at commit boundaries; the WAL barrier is taken there, then the prune is queued.
`el-prune` starts at the top of the next apply iteration, after the commit, and
logs `deferred prune at N: commit barrier applied`. The RPC never starts while
an uncommitted group exists.

Tests: fmt, clippy, workspace; e2e regtest, recovery and pruned-IBD (prune +
SIGKILL + restart). The machine-plan test asserts the 256 MiB byte bound.

## 0.1.0-alpha.29.7.0 — 2026-11-09

### Machine-aware plan, fewer commits and no prune stalls

This release turns the runtime knobs into a function of the machine, and stops
paying for commits/prunes that safety does not require.

**CPU detection uses physical cores.** `/proc/cpuinfo` is parsed for unique
`(physical id, core id)` pairs; affinity and the cgroup CPU quota are applied
afterwards. A 4C/8T i7-6700T now gets `-par=3`, `-rpcthreads=4`, `readers=4`
and `background_jobs=2` instead of 7/8/8/8. The previous defaults counted
logical SMT threads as cores and oversubscribed the machine.

**Disk detection.** Each relevant path (`data_dir`, `core_datadir`,
`blocks_dir`) is resolved through `/proc/self/mountinfo` and `/sys/dev/block`
to its parent device and classified as NVMe, SATA SSD, HDD, virtual (dm/md/
loop), network or unknown. The classification sets the point-read concurrency,
block-cache cap, RocksDB background jobs and the bulk commit interval. Data and
Core on the same device share the I/O budget; a separate device allows more
concurrency.

**RAM plan with a page-cache floor.** The reserve is `clamp(RAM/8, 2048, 4096)`
and a separate `page_cache_floor` of `clamp(RAM/16, 1024, 4096)` prevents the
block cache from evicting the page cache. Cache shrinks are now 12.5 % steps,
not 25 %, and growth still requires measured prefetch pressure. On 32 GiB the
plan is: reserve 4 GiB, page floor 2 GiB, memtables 2 GiB, cache 4 GiB on SATA
(8 GiB on NVMe).

**Fewer, safer commits.** Bulk commits are triggered by transactions (250k),
blocks (512) or time (20 s NVMe, 30 s SATA/virtual, 45 s HDD, 60 s network).
A time trigger with fewer than 8 blocks waits until 60 s, so 1-block commits
disappear. Live mode switches to 1.5 s. This is safe because the prune gate
uses the committed tip: Core never prunes blocks that an uncommitted batch
would need to replay. The commit barrier before pruning is unchanged.

**Weighted memtables.** `utxo` (weight 6), `history` and `utxo_by_script`
(weight 4) get larger memtables than `meta`/`undo`; the shared
`WriteBufferManager` still enforces the total. `/status.rocksdb.cf` now shows
pending compaction, memtable, SST and table-reader bytes per family, so the CF
causing a write stall is visible instead of hidden by the sum.

**Prune no longer blocks the index.** `pruneblockchain` runs in its own thread
after the WAL barrier; the apply thread continues. The RPC timeout for prune is
at least 120 s, so the 30 s read timeout that surfaced as
`Resource temporarily unavailable (os error 11)` no longer aborts a prune.
`/status.machine` exposes the whole detected plan.

Tests: workspace fmt/clippy/tests; e2e regtest, recovery (SIGKILL/reorg/write
failure), multi-instance and pruned-IBD with prune + SIGKILL + restart. The
pruned test also checks pipeline discarded work and the asynchronous prune.

## 0.1.0-alpha.29.6.0 — 2026-11-08

### IBD throughput, safe pruning and Core/Tor coordination

This release applies the performance plan derived from a real 29.5.0 mainnet
log. It targets work that was being wasted, not a blind increase in cache or
batch size. No consensus, undo or WAL durability rule was weakened.

**Continuous indexing pipeline.** The catch-up loop no longer returns every
10 s just to let the supervisor run. At 3.3 blocks/s that deadline tore down
readers and discarded prepared blocks roughly every 33 blocks. Supervisor
actions (prune, disk, state) now run at commit boundaries; the 512-block
checkpoint remains. `/status.pipeline` counts prepared, applied and discarded
blocks, and `tests/e2e_pruned_ibd.sh` asserts discarded work stays below 5 %.

**No lag ratchet.** The bulk lag baseline was rewritten with the current lag on
every resume, so the next pause threshold grew by `max_bulk_lag_blocks/2` per
cycle (14,884 → 24,858 → 34,858 in the log). A normal resume no longer touches
the baseline; only a full-pause prewarm anchors it. `E_LITE_LAG_POLICY=drain`
adds an opt-in time-based watermark: pause when the index would need more than
`E_LITE_LAG_TARGET_SECONDS` (default 1800) to drain the backlog at the last
commit cycle's rate, resume at half. It stays opt-in pending a mainnet A/B.

**Cheaper, safer prune barrier.** Before `pruneblockchain`, e-lite used to flush
all seven column families and then the WAL. The durability requirement is
`SyncWAL(true)`: a single `WriteBatch` is atomic across CFs and `txids.dat` is
fsynced before the batch. The forced SST flush was a multi-second stall on
every prune. `tests/e2e_pruned_ibd.sh` now SIGKILLs e-lite after a prune and
verifies the index recovers with the full history.

**Correct RocksDB metrics.** `estimate-pending-compaction-bytes`,
`cur-size-all-mem-tables`, `total-sst-files-size` and
`estimate-table-readers-mem` are per-CF and are summed; `num-running-
compactions` and `block-cache-usage` are DB-wide and are read once. A zero is
no longer ambiguous: `/status.rocksdb.properties_ok` says whether every
property was actually read.

**Commit decisions use the real cycle.** `commit_share` divides by the
start-to-start commit interval (`commit_cycle_ms`), not by the configured
interval, and the cache grows only when prevout prefetch is measurably slow —
free RAM alone is not a reason.

**One controller.** `maybe_adapt` has one caller and one `adapt_gate` mutex
covering the 30 s check and the mutation. The 15–25 s cache changes seen in the
29.5.0 log (two callers passing the gate concurrently) cannot recur. Bulk/Live
mode has hysteresis (leave Bulk below 500 blocks, re-enter above 2,000).

**Faster prevout lookups.** `get_utxos` uses `batched_multi_get_cf` over
deduplicated, sorted keys in the `utxo` family. Overlay precedence and the
"missing prevout is absent" contract are unchanged.

**Core/Tor.** A managed Core now gets `-listenonion=0`: the managed `torrc` has
no `ControlPort`, so Core's default `-listenonion=1 -torcontrol=
127.0.0.1:9051` retried a dead port forever. The operator can override it in
`extra_args`. `/status.tor` exposes the effective SOCKS port, whether Tor is
managed and `control_port: 0`; `doctor` prints the Core onion state and warns
if an operator forces it on. The per-user sync lock is unchanged and still
documented as intentional.

Docs: `TUNING.md` (pipeline, lag policy, WAL barrier, per-CF metrics),
`TOR.md` (only the Electrum onion is managed), `AGENTS.md`, `HANDOFF.md`,
`AUDIT.md`.

## 0.1.0-alpha.29.5.0 — 2026-11-09

### Memory, Core and IBD: one plan for the whole pipeline

This release is about making e-lite use the machine it is installed on, and
refusing to pretend when it cannot.

**Supported minimum (new).** `run`/`index` check the *effective* RAM (host vs
cgroup minimum) before creating certificates, opening the database or starting
Core: mainnet needs an 8 GB class machine (`>= 7500 MiB`), testnet4/signet/
regtest a 4 GB class one (`>= 3800 MiB`). The failure names the detected source
and the requirement. `serve` (existing index only) is exempt.
`--allow-low-memory` / `E_LITE_ALLOW_LOW_MEMORY=1` exists for CI.

**One fitted memory budget.** The ceiling is `min(RAM*fraction, RAM-reserve)`
and runtime + memtables + cache must fit inside it. An explicit fraction is
never raised to satisfy floors. The reported case (`RAM=7907`,
`memory_fraction=0.20`) used to plan 1738 MiB inside 1581 MiB and now fits
exactly. Memtables are capped through a shared RocksDB `WriteBufferManager`
(no more 64 MiB request becoming 168 MiB of buffers), and a tiny budget
degrades instead of panicking. Index/bloom filters stay outside the data cache
but their memory is reported as `table_readers_bytes`.

**Correct cgroup detection.** Limits are resolved from `/proc/self/cgroup` and
`/proc/self/mountinfo` and every ancestor is walked. Each limit is paired with
that level's usage, so a parent limit is not combined with a child's usage.
`doctor` distinguishes limited, unlimited and not detected.

**Descriptors.** `max_open_files` handles `RLIM_INFINITY` (which used to wrap to
a negative value and collapse to 256), reserves descriptors for client sockets
and RPC instead of a fixed 64, never pads above the soft limit, and is shown in
`doctor` and `/status.file_descriptors` with a warning when capped below 2048.

**Core started with a plan.** A Core that e-lite starts now gets
`-dbcache=clamp(RAM/4, 450, 2048)` (at most `RAM/3`), `-par=cpus-1` (1..8),
`-rpcthreads=clamp(cpus, 4, 16)` and an explicit `-prune` (including `-prune=0`,
which previously let `bitcoin.conf` decide). Operator values in `extra_args`
win; arguments e-lite owns are rejected with an explanation. The child is
registered before the pid file is written, adoption checks a start-time token,
and shutdown waits up to 120 s for a clean flush.

**Safer commits.** Undo is generated for every applied block and pruned at
commit time from the tip that is really committed. A commit forced by the time
limit, the transaction limit or the supervisor deadline can no longer leave a
tip without its reorg window; stale undo inserted by the same `WriteBatch` is
deleted too.

**Controller.** A dedicated `el-tune` thread samples every 5 s and changes at
most once per 30 s, so a long catch-up cannot delay memory protection. `index`
without a serving process no longer classifies every workload as live,
`batch_locked` is honoured, `prefetch_blocks` actually bounds the prepared
window, and `commit/blk` uses the blocks actually committed (an 8 s commit of
100 blocks is 80 ms/block, not 7).

**One intensive sync per user.** A `flock` makes a second simultaneous IBD
**wait** and start automatically instead of halving both throughputs. It is
taken before a from-zero managed Core starts and released within 1000 blocks of
Core. Separate containers are not coordinated.

**Measurement.** `tests/bench_sample.py` now uses monotonic elapsed time, the
real `CLK_TCK`, the simultaneous RSS peak, matching units and `stage_totals_us`;
it reports `completed` and the shell propagates a failure instead of hiding it.
Disk accounting no longer counts a nested Core datadir as index.

Docs: `TUNING.md` (minimum, budget, controller, descriptors, Core plan, containers
and UmbrelOS), `INSTALL.md`, `CLI.md`, `AGENTS.md`, `AUDIT.md` (AUD-081..090).

## 0.1.0-alpha.29.4.5 — 2026-11-08

### Fix: cap RocksDB open files to the process limit

- `max_open_files = 8192` (alpha.29.4.4) would fail table opens on hosts with a
  low `RLIMIT_NOFILE`. The value is now capped to `ulimit -n - 64` (minimum 256)
  before configuring RocksDB, so the prevout read improvement cannot turn into
  an `EMFILE` startup failure.

## 0.1.0-alpha.29.4.4 — 2026-11-08

### Performance: stable RocksDB cache and prevout read path (mainnet defaults)

Measured on a real mainnet IBD with default settings (~640k height):

- 4.2–4.8 blocks/s and ~9.5–10k tx/s.
- Per-block cost was dominated by **prevout prefetch (100–1500 ms)**, then
  decode (~25 ms), apply (~20–100 ms) and commit (~7–8 ms/block amortised).
  `read` was ~1–10 ms: block reading was not the bottleneck.
- The adaptive cache grew to 6.6 GiB and then collapsed to 256 MiB while the
  page cache fell to ~150 MiB, turning prevout lookups into random disk reads.

Defaults changed:

- **Block cache is capped at RAM/4** (not the whole memory ceiling) with a
  floor of `clamp(RAM/8, 512 MiB, 4096 MiB)` for adaptive shrinks. This keeps
  the recent UTXO working set stable instead of oscillating between 6 GiB and
  256 MiB.
- **Cache decisions use `MemAvailable`**, not a low page-cache reading: the
  RocksDB block cache is expected to hold the working set, and shrinking it
  because page cache is low caused thrash.
- **Index and bloom filters stay out of the data block cache** and every SST
  reader is kept open (`max_open_files = 8192`), so a shrinking data cache
  cannot evict the filters needed by prevout lookups.
- New `/status` fields for the next measurement: cumulative stage totals in
  microseconds, commit count/txs, `commit_breakdown_us` (txids fsync vs RocksDB
  write) and RocksDB pending compaction/memtable/SST counters.
- `tests/bench_ibd.sh` + `tests/bench_sample.py` measure a joint feeder-Core
  IBD with e-lite indexing, including CPU/RAM/disk, pauses and stage totals.

## 0.1.0-alpha.29.4.3 — 2026-11-07

### Fix: Java/Sparrow TLS trust (local CA + server certificate)

- **Why**: Java wallets reject a self-signed server certificate used as a
  `TrustAnchor` with `TrustAnchor ... is not a CA certificate`. e-lite generated
  a single leaf with no CA, so Sparrow could not validate it even after
  importing it.
- **New model**: e-lite now generates a per-instance local CA (`ca.crt`,
  `ca.key`, `CA:TRUE`) and signs the server certificate with it. The TLS chain
  file (`self-signed.crt`) contains leaf + CA, so a wallet that trusts
  `ca.crt` can build the path and validate the hostname/IP.
- The leaf is re-signed with the same CA when the SAN set changes (LAN IP,
  hostname, onion), so wallets keep trusting the CA across restarts and onion
  rotation.
- Existing single-cert installs migrate automatically on the next start: the
  leaf key is reused, the CA is created and the chain is rewritten. Import the
  new `ca.crt` in the wallet and remove the old trusted server certificate.
- The startup log prints the leaf fingerprint and the CA path/fingerprint.
- Tests: CA+leaf chain structure, migration from the old single-cert model,
  `openssl verify -CAfile ca.crt`, `CA:TRUE`, and wallet-style validation of
  `localhost` and the LAN IP (with a wrong host rejected).
- Docs updated with the trust instructions.

## 0.1.0-alpha.29.4.2 — 2026-11-07

### Patch: undo at commit boundaries, per-network SOCKS and multi-instance tests

- **Undo anchored to the commit group**: the reorg window is now relative to
  the end of the current commit group, and a time-based commit waits until the
  tip has its whole undo window. With the default batch (256 <= window + 64)
  every block of a group is covered; with larger bulk batches only the part
  that cannot become a commit tip is skipped. A partial group without undo is
  not committed, so no committed tip is left unrecoverable.
- **Per-network SOCKS ports**: a managed Tor uses 9050 on mainnet and
  55150/55250/55350 on testnet4/signet/regtest, so several managed instances
  can start together without racing for the same socket. Explicit
  `tor.socks_port` still wins.
- **Multi-instance test** (`tests/e2e_multi_instance.sh`): two regtest Cores and
  two e-lite instances with independent plain/TLS/metrics ports and data dirs;
  verifies both heights, distinct certificates, combined RSS, and that stopping
  or restarting one does not affect the other.
- **TLS trust test**: `e2e_defaults.sh` now validates the generated certificate
  as a CA with `check_hostname` for `localhost` and the LAN IP, and checks that
  a hostname outside the SAN is rejected.
- Benchmark block count now includes the genesis/premine blocks it indexes.

## 0.1.0-alpha.29.4.1 — 2026-11-07

### Patch: undo at the committed tip, TLS hardening and input validation

- **Undo anchored to the batch tip**: during bulk catch-up, undo is now kept
  for the last ~`reorg_window + 64` blocks of each catch-up batch instead of
  only near Core's tip. A lagging index can therefore rewind at its own
  committed tip after an interrupted batch. A Core rollback deeper than the
  window now fails with an explicit `NEEDS_RECOVERY` message instead of a
  missing-undo error.
- **TLS certificate hardening** (`bin/e-lite/src/tls.rs`):
  - SANs include the non-loopback IPs of the local interfaces, in addition to
    `localhost`, `127.0.0.1`, `::1`, the hostname and the onion.
  - The printed fingerprint is the SHA-256 of the DER leaf certificate (the
    value `openssl x509 -fingerprint -sha256` shows).
  - Key/certificate files are written with `0600` via a temporary file and an
    atomic rename; the key is reused when only the SAN set changes.
  - A corrupt auto-generated pair is detected and regenerated.
  - `tls_auto_cert = false` now accepts an existing valid pair instead of
    refusing to start; it only errors when no usable certificate exists.
  - Operator PEMs are validated as a pair at startup.
- **Config input validation**: default TLS paths are filled only when neither
  `tls_cert` nor `tls_key` was set, so a half-configured pair fails validation.
  Legacy Core datadirs are only detected when the chain's block directory
  exists, not merely because a `bitcoin.conf` is present.
- **Tests**: `tests/e2e_recovery.sh` (repeated SIGKILL, reorg at the indexed
  tip, write failure from a file-size limit, then clean recovery), plus TLS
  SAN/fingerprint assertions in `tests/e2e_defaults.sh` and unit tests for the
  undo window, corrupt-pair regeneration, opt-out with an existing pair and
  operator certificate preservation.
- Docs and AGENTS invariants updated.

## 0.1.0-alpha.29.4.0 — 2026-11-07

### Feature: per-network ports, automatic TLS and isolated instances

- **Per-network listeners**: plain/TLS/metrics default to
  `55011/55022/55084` (mainnet), `55111/55122/55184` (testnet4),
  `55211/55222/55284` (signet) and `55311/55322/55384` (regtest). The plain
  listener binds `0.0.0.0`; the TLS listener copies its host. Explicit
  `--listen`, `--tls-listen` and `--metrics` still win.
- **TLS on by default**: `[server] tls = true`. On first start e-lite generates
  a self-signed certificate in `<data-dir>/tls/` (SANs: `localhost`, `127.0.0.1`,
  `::1`, hostname, onion when known) and logs its path and SHA-256. `--no-tls` /
  `tls = false` disables it. Operator PEMs are never overwritten.
- **TLS independent of Tor**: certificate generation moved to
  `bin/e-lite/src/tls.rs`; a Tor failure no longer disables clearnet TLS.
- **One folder per network**: data defaults to `~/e-lite/<network>` and Core to
  `<data-dir>/bitcoin`, so several instances can run side by side. A single
  legacy Core datadir is detected with a migration warning; two candidates
  (`~/.bitcoin` + `~/e-lite/bitcoin`) are rejected instead of guessed. The old
  `~/.local/share/e-lite/<network>` index keeps being used until moved.
- **Multi-instance**: distinct Electrum, TLS, metrics and onion ports per chain;
  Core cannot share a datadir across chains, so each instance gets its own.
- **`run` serves while Core is down**: with an existing index, `run` now opens
  Electrum in `CORE_UNAVAILABLE` and keeps retrying Core instead of waiting
  before serving. A first run with no identity still waits for Core.
- Tests: per-network defaults, loopback pairing, TLS on/off validation, cert
  generation without Tor, operator cert preservation, plus `tests/e2e_defaults.sh`
  (plain + TLS + metrics with no TLS/Tor configuration) alongside regtest and
  pruned-IBD.
- Docs updated: README, CLI, OPERATIONS, TOR, INSTALL, GETTING-STARTED,
  FEATURES, ARCHITECTURE, PRIVACY and AGENTS.

## 0.1.0-alpha.29.3.7 — 2026-11-07

### Patch: prune locator cleanup, external rawtx guards, disk-first gain

- **Sidecar cleanup**: on open, locators for pruned `blk*.dat` files are dropped
  and the sidecar is rewritten compactly before the scanner starts. After a
  prune, the in-memory locator map is filtered and cached file descriptors are
  closed. This prevents stale entries and deleted files kept open.
- **ReaderTracker only for local heights**: rawtx resolution enters a reader
  guard only when the block is at or above Core's prune height. External-only
  resolutions no longer block pruning. Prefetch also no longer holds a guard.
- **Disk-first gain**: `min_gain_blocks` default lowered from 10,000 to 1,000.
  On mainnet that bounds the retained tail to roughly ~1.5 GiB above the safe
  cut instead of ~15 GiB; Core still prunes whole files, so actual retention
  rounds to file boundaries. `prune_interval_seconds` remains 30 s.
- Tests for locator filtering and sidecar compaction; regtest and pruned-IBD e2e
  pass.
- No schema or wire-format change.

## 0.1.0-alpha.29.3.6 — 2026-11-07

### Patch: durable pruning, confirmed pauses and bounded indexing memory

- **Durability before prune**: e-lite flushes RocksDB/WAL before asking Core to
  delete block files, so the height authorising the cut is durable even when
  bulk commits used `sync=false`.
- **Failed prune retries**: a failed `pruneblockchain` RPC is retried at the same
  safe height after a 30 s backoff instead of waiting for the safe height to
  move. The normal 10k-block gain and interval still apply.
- **Confirmed Core pauses**: pause/resume only update internal state after
  `setnetworkactive` succeeds; a failed RPC is retried instead of being treated
  as done. Lag-guard resumes no longer override a disk pause.
- **Disk hysteresis fixed**: a cold start above the low watermark no longer
  pauses just because it is below the resume watermark.
- **Exact readiness**: `READY`/`FULLY SYNCED` require the exact indexed tip, not
  `height + 1`; prune/resume paths recompute the state instead of forcing READY.
- **Indexing independent of providers**: `run` starts indexing immediately and
  only gates the wallet listeners on provider readiness, so a provider outage
  cannot stop local block progress.
- **Bounded prepared blocks**: an explicit window limits fetched/decoded/prevout
  blocks held in the channel and out-of-order `pending` map.
- **Supervisor cadence**: a long 512-block catch-up yields after the poll
  interval so pruning/disk actions are not delayed.
- **Space-first default**: `prune_defer_free_gib = 0` by default; pruning is
  bounded by the 10k gain and 30 s interval, not by free space.
- **Pruned block files**: dropping the bulk reader cache after a prune releases
  descriptors/handles that could keep deleted files allocated. Disk checks now
  include a custom `blocks_dir`.
- Tests: prune retry, existing chain/index tests, regtest and pruned-IBD e2e.

## 0.1.0-alpha.29.3.5 — 2026-11-07

### Patch: outpoint notifications off the notifier thread, complete protocol matrix

- **Outpoint resolution moved to bounded workers**: `blockchain.outpoint.subscribe`
  status may need Core or an external raw transaction. It now runs in a
  2-worker, queue-256 pool with pending-job retry instead of blocking the
  single notifier thread and delaying every other wallet. Reorg notifications
  are preserved.
- **Removed the temporary live provider test** (`live_esplora_health_cooldown`);
  the provider cooldown remains covered by the deterministic local tests.
- **Electrum protocol matrix**: added `docs/ELECTRUM_PROTOCOL.md` with the
  method/version matrix (1.4–1.4.2, 1.6, 1.7), notification semantics, limits,
  deliberate deviations and performance notes.
- No schema or wire-format change.

## 0.1.0-alpha.29.3.4 — 2026-11-07

### Patch: BIP144 witness requests, BIP141 validation and Electrum protocol audit

- **P2P requests witness blocks**: `getdata` now uses `MSG_WITNESS_BLOCK`
  (`0x40000002`) instead of `MSG_BLOCK`. The previous code advertised
  `NODE_WITNESS` but asked for the legacy serialization, so peers returned
  blocks without witnesses and validation failed with `missing witness reserved
  value`.
- **P2P peer checks**: peers without `NODE_WITNESS` are rejected, `notfound` is
  an immediate error, and the witness commitment is verified inside the P2P
  source before a peer is marked good. A bad peer is cooled instead of being
  retried for every transaction.
- **BIP141 correctness**: the highest-index coinbase commitment output is used,
  and the coinbase witness must contain exactly one 32-byte reserved value.
- **Electrum 1.7 notifications**: scripthash and scriptpubkey subscriptions are
  independent maps, so a client using both receives both notifications.
  Outpoint subscriptions also force a notification on reorg.
- **JSON-RPC parameters**: heights, counts, `cp_height` and `txout_idx` use
  checked conversions instead of truncating `u64` to `u32`.
- **Prefetch**: deduplication is by block hash, failed/queue-full work is not
  marked as done, and computing a subscription status no longer triggers
  external block downloads.
- **Cache**: block reads and `fsync` no longer hold the metadata lock; a lowered
  cache capacity is enforced at open, not only after the next write.
- **testmempoolaccept**: Core's `reject-reason` is mapped to Electrum's
  `reason`.
- **Bounded external work**: a 30 s deadline across external sources and a 45 s
  inflight wait prevent a single rawtx request from hanging for minutes.
- Tests updated for witness-block getdata, witness-service peers, BIP141
  commitment selection/reserved value, and the protocol changes. No schema
  change.

## 0.1.0-alpha.29.3.3 — 2026-11-07

### Patch: wallet-friendly rate limit and disk-minimal defaults

- **Rate limit is now a throttle, not an error**: the same 200 req/s with burst
  5000 applies to every client (loopback, LAN and public). When the bucket is
  empty the request waits for the next token (a few milliseconds at 200 req/s)
  instead of returning `-32000 "rate limit exceeded"`. A Sparrow gap scan no
  longer aborts around address 55; `server.rate_limit = 0` still disables the
  limiter.
- **Disk-minimal defaults for a max-pruned Core**:
  - rawtx cache default lowered from 2048 MiB to 256 MiB;
  - new `rawtx.cache_dir` lets the cache live on tmpfs (for example
    `/dev/shm/e-lite-rawtx`), keeping block data off persistent disk;
  - application log rotation lowered from 64 MiB to 16 MiB;
  - snapshots remain manual/off by default.
- **Disk breakdown** in `/status.disk`: `index_bytes`, `txids_bytes`,
  `cache_bytes`, `cache_dir`, `log_bytes`, `snapshot_bytes` and `total_bytes`.
- Tests: rate-limit unit and protocol tests now assert waiting/success instead
  of rejection. No schema, Electrum protocol or provider behaviour change.

## 0.1.0-alpha.29.3.2 — 2026-11-07

### Patch: never fail a wallet request without trying a provider

- **All-provider cooldown fallback**: when every provider is cooling down,
  e-lite now makes one forced attempt (last good provider first) instead of
  returning an error immediately. A single transient failure no longer turns
  into minutes of failed `transaction.get`/history loads.
- **Last good provider retried sooner**: after one failure it cools down for at
  most 30 s (never-successful providers keep the full cooldown).
- **Bounded SOCKS5 handshake**: the P2P Tor path no longer depends on the
  `socks` crate (which has no connect/read timeout). A minimal SOCKS5 CONNECT
  uses the P2P timeout for connect, auth, request and reply, so a stalled Tor
  SOCKS proxy cannot block a fetch indefinitely.
- **Inflight panic guard**: a panicking external fetch now completes the shared
  inflight entry with an error instead of leaving followers waiting.
- **Diagnostics**: failed `get_history` (scripthash) and failed rawtx
  resolutions are logged with the scripthash/txid, height and block hash.
- Tests: all-cooled providers still get one attempt, a failed provider is
  skipped when another works, SOCKS handshake timeout and password handshake.
- No schema or Electrum protocol changes.

## 0.1.0-alpha.29.3.1 — 2026-11-07

### Patch: stop dead providers and peers from stalling rawtx

- **Direct provider cooldown**: a failed Tor *or direct* request now cools that
  provider for `provider_cooldown_seconds` (default 300 s), and the last
  provider that answered is tried first. Previously a dead clearnet provider
  (for example `mempool.space/signet/api` timing out) cost 15 s on every block;
  the 17–34 s gaps in Sparrow matched that timeout.
- **Shorter rawtx timeout**: `request_timeout_seconds` default is now 8 s
  (was 15). Healthy Esplora providers answer in well under a second.
- **In-flight block dedup**: concurrent `transaction.get` calls and the history
  prefetcher for the same block share one download instead of racing.
- **Concurrency limit**: `max_parallel_fetches` (default 4) bounds external
  block downloads, including decoys.
- **P2P fail-fast**:
  - `p2p_timeout_seconds` (default 8) is separate from Core RPC's timeout and
    budgets v2 + v1 per peer; one block has a total 2× timeout cap.
  - Peers come from Core's current connections first (`getpeerinfo`), then
    addrman; up to three are tried per block.
  - The last peer that answered goes first; failed peers are skipped for the
    provider cooldown instead of being retried for every block.
  - A failed Tor SOCKS connect trips a circuit breaker: non-onion peers go
    direct for the cooldown window (with one warning) instead of retrying Tor
    per peer and per block.
  - Direct connects now use `connect_timeout`; previously the OS timeout could
    add minutes.
- **Cheaper decoys and prefetch**: `decoys` defaults to 1 (was 3), at most two
  decoys run at once, and the prefetch never spawns decoys. The prefetcher uses
  one worker and at most 16 heights per history response.
- `/status` and `/metrics` expose provider/P2P failures, cooling skips,
  decoys in flight and prefetch queued/done.
- Tests: silent P2P peer fails within the short timeout and is not reconnected
  while cooled; concurrent resolvers share one block download; a failed direct
  provider is not retried. Workspace tests and regtest e2e are green.
- No schema or Electrum protocol changes.

## 0.1.0-alpha.29.3 — 2026-11-07

### Patch: P2P witness, faster rawtx and history prefetch

- **P2P now advertises `NODE_WITNESS`** in `version` (v1 and v2). Without it,
  peers strip witnesses from blocks; the block hash/merkle still match, so
  e-lite accepted the bytes but the BIP141 check failed with `missing witness
  reserved value`. This was the cause of stuck `Retrieving transaction` on
  signet.
- **Esplora request timeout** reduced from 60 s to 15 s
  (`rawtx.request_timeout_seconds`) and **per-provider Tor cooldown**
  (`rawtx.provider_cooldown_seconds`, default 120 s): after a failed Tor request
  that provider goes direct instead of waiting a minute per transaction.
- **P2P clearnet fallback** when Tor fails (`tor_fallback_clearnet`), with a
  one-time warning.
- **Readiness probe now fetches the genesis raw block** (`/block/<hash>/raw`)
  and verifies its header hash, not only `/block-height/0`. A provider that
  cannot serve real blocks cannot open the wallet gate.
- **Tor warning fixed**: “Tor unavailable” only when no probe used Tor; partial
  provider fallback is reported separately.
- **History block prefetch**: when a script history is served, e-lite enqueues
  the confirmed heights in a bounded, deduplicated background queue (2 workers)
  so the subsequent `transaction.get` hits the block cache.
- Config: `request_timeout_seconds`, `provider_cooldown_seconds`; docs updated.
- Tests: NODE_WITNESS version payload, raw-block genesis probe (match/mismatch),
  Tor fallback state; existing 1.4/1.6/1.7 e2e stays green.
- No schema or protocol changes.

## 0.1.0-alpha.29.2 — 2026-11-06

### Patch: provider readiness checks the network genesis

The readiness probe accepted any 64-hex `/block-height/0` response, so a
provider serving a different network could pass the gate and let wallets open
against the wrong chain.

- `public_servers::probe` now receives the expected genesis (the indexed
  `NetworkIdentity` in `run`/`serve`; the known chain genesis in `doctor` when
  available).
- A provider whose `/block-height/0` differs is marked unhealthy with
  `genesis mismatch ... (wrong network)` and cannot satisfy `require_ready`.
- Tests: healthy provider with the expected genesis and a provider returning a
  different genesis (gate rejects it). Existing 1.4/1.6/1.7 regtest e2e remains
  green.
- No protocol or schema changes.

## 0.1.0-alpha.29.1 — 2026-11-06

### Patch: provider readiness and clearnet fallback

- **Readiness before wallets**: `run` probes the network's `public-servers.toml`
  entries (`GET /block-height/0`) and only opens the Electrum listeners after at
  least one provider answers. If none does within `ready_timeout_seconds`
  (default 60 s), it exits with instructions instead of letting a wallet receive
  an incomplete history.
- **Tor fallback to clearnet**: `[rawtx] tor_fallback_clearnet = true` (default)
  tries Tor first, then the clearnet endpoint if the Tor proxy is unavailable or
  the Tor request fails. `.onion` endpoints still require Tor. Clearnet fallback
  logs a warning and is visible in `/status`; use `false` for fail-closed.
- **Health surface**: `/status` now includes per-provider `{url, healthy,
  latency_ms, via, error}`, `provider_ready`, `providers_healthy`, `tor_ready`
  and `clearnet_fallback`; `/metrics` adds `e_lite_providers_healthy`.
  `doctor` probes and prints the same table.
- **`serve`** probes once but does not block (offline mode); it warns when no
  provider is ready. `index`/`doctor` never block.
- **Config**: `[public_servers] require_ready`, `probe_timeout_seconds`,
  `ready_timeout_seconds`; `[rawtx] tor_fallback_clearnet`.
- Tests: provider probe (healthy fallback), readiness gate decision, Tor
  fallback state, config parsing; existing 1.4/1.6/1.7 e2e regtest stays green.
- No schema or protocol changes.

## 0.1.0-alpha.29 — 2026-11-06

### Core RPC exposure guard

Core RPC is HTTP only and has no native TLS. e-lite now makes the cleartext
LAN/WAN risk visible and enforceable.

- Startup/commands warn when `rpc_url` is non-loopback: credentials cross the
  link in clear text; use an SSH/VPN tunnel.
- `[core] require_loopback_rpc = true` makes a non-loopback `rpc_url` a fatal
  configuration error with instructions (SSH forward + `127.0.0.1`).
- `doctor` prints the RPC transport (`loopback` or `non-loopback, cleartext`).
- `/status` adds `core_rpc_loopback`.
- Documentation: SSH tunnel recipe, config example, CLI table, privacy notes.
- Roadmap cleanup: remove the completed rate-limit/UTXO-prefetch/Core-RPC rows;
  systemd remains optional.
- No change for the default loopback Core setup.

## 0.1.0-alpha.28 — 2026-11-06

### Per-client Electrum rate limiting

Defends a non-loopback Electrum listener from request bursts without pretending
to authenticate wallets.

- New `[server] rate_limit` (requests/second per client) and
  `[server] rate_burst`. Absent/`None` means auto: **enabled at 50 req/s with a
  300 burst** when any listener is non-loopback, **disabled on loopback**.
  Explicit `0` disables it; any positive value forces it.
- One token bucket per connection; every JSON-RPC line costs one token. When the
  budget is exhausted the server returns `-32000 "rate limit exceeded; slow
  down"` and keeps the connection open. Notifications still get no response.
- The notifier (subscriptions) does not consume the bucket.
- `/status` exposes `rate_limit_rps`, `rate_burst` and `rate_limited`;
  `/metrics` adds `e_lite_rate_limited_total`.
- Docs: `config.example.toml`, `CLI.md`, `FEATURES.md`, `ROADMAP.md` (systemd is
  now explicitly optional, not a release blocker) and `AUDIT.md` (AUD-010/015).
- Tests: token-bucket unit test, a burst over a real socket that is limited but
  keeps the connection alive, and config parsing.

## 0.1.0-alpha.27.2 — 2026-11-06

### Patch: enable the verified mempool.space onion

Verified over Tor 0.4.9.15 on 2026-11-06: the official mempool.space onion
returns the raw block for block 9, the double-SHA256 matches the requested hash,
and the bytes are identical to the clearnet response.

- The mempool.space onion `/api` endpoint is now enabled by default in
  `public-servers.toml`.
- The Blockstream API onion candidate did not resolve over Tor (SOCKS5 cannot
  connect) and stays disabled; enabling it requires a future verification.
- Existing `public-servers.toml` files are never rewritten, so only new/first-run
  installs get the enabled onion; edit the file to add it manually if needed.

## 0.1.0-alpha.27.1 — 2026-11-06

### Patch: 1.7 scriptpubkey notifications

`blockchain.scriptpubkey.subscribe` notifications must use the
`blockchain.scriptpubkey.subscribe` method (the params still carry the
scripthash). The notifier previously sent every notification as
`blockchain.scripthash.subscribe`.

- Scriptpubkey subscribers now receive the correct method name; scripthash
  subscribers keep `blockchain.scripthash.subscribe`.
- The 1.7 e2e now mines after subscribing and asserts both the outpoint and the
  scriptpubkey notification method. It also filters the spend UTXO by the mining
  address so the `spk_hint` check is exercised correctly.

## 0.1.0-alpha.27 — 2026-11-06

### Electrum 1.7 compatibility

The server now negotiates 1.4–1.7. Additive methods are available to every
client; only `server.ping` and `server.features` change shape with the
negotiated version. No schema, index or snapshot changes: 1.7 is resolved from
existing data plus the whole-block resolver.

- **`server.features`**: `protocol_max = "1.7"`; `hash_function` is omitted for
  1.7 clients and kept for 1.4/1.6.
- **`server.ping`**: 1.7 signature `(pong_len=0, data="")` returns
  `{"data": "<pong_len hex chars>"}`; notifications get no response. The length
  is capped at 1 MiB to avoid memory abuse.
- **`blockchain.scriptpubkey.*`**: `get_balance`, `get_history`, `get_mempool`,
  `listunspent`, `subscribe`, `unsubscribe`; history/mempool return dicts, the
  notification still uses the scripthash, and an oversized history returns
  `10001`. Scriptpubkey subscribers are notified on reorg even if the status is
  unchanged.
- **`blockchain.outpoint.*`**: `get_status`, `subscribe`, `unsubscribe`. Live
  UTXOs use the O(1) fast path; spent outputs are resolved from the script
  history and only the candidate raw transactions are fetched. `spk_hint` is
  verified against the live UTXO / mempool output / funding transaction; a
  mismatch returns `{}` instead of inventing a spender. Subscriptions are capped
  at 1,000 per connection.
- **`mempool.recent`**: last 10 mempool entries with `txid`, `fee`, `vsize`.
- **`blockchain.transaction.testmempoolaccept`**: local Core only.
- **`listunspent`** (scripthash and scriptpubkey) now uses blockchain order
  `(height, tx_pos)` for confirmed UTXOs.
- **Privacy/efficiency**: outpoint and scriptpubkey methods add no index writes;
  the e2e asserts `rawtx.fetches == 0` on regtest, i.e. only local data was used.
- Tests: 1.7 features/ping/shapes, scriptpubkey subscribe/unsubscribe, outpoint
  unknown/unspent/mempool/confirmed, outpoint notifications, real
  `testmempoolaccept`, and a 1.6/1.4 regression on the same instance.

## 0.1.0-alpha.26.1 — 2026-11-06

### Patch: public providers live in an editable file

Public Esplora endpoints are no longer compiled into the binary. The effective
list is read from `public-servers.toml` next to `config.toml`; the shipped file
is the single source of truth.

- **First run / missing file**: e-lite copies the shipped template (config dir,
  working directory, next to the binary, or `/usr/share/e-lite/`). Existing
  files are never overwritten. `[public_servers] generate = false` disables
  creation; a missing file means local data only (fail closed).
- **Verified defaults** in the template: Blockstream and mempool.space for
  mainnet, signet and testnet4; the mempool onion is present but commented until
  a real-Tor e2e verifies the `/api` path. `regtest` is empty.
- **Validation**: clearnet must be `https://`; plain `http://` is accepted only
  for `.onion` hosts and still needs Tor. Invalid URLs are warned and skipped.
- **Precedence**: `[rawtx] esplora_urls` (CLI/`--set`) → `public-servers.toml`
  → none. There is no hidden fallback list in code.
- **New `--public-servers <path>`** and `[public_servers]` config
  (`file`, `generate`).
- **`/status`** includes the effective `public_servers` list; **`doctor`** shows
  the resolved file, its source and the endpoints per network.
- **Packaging**: the template ships in the tarball and `.deb`
  (`/usr/share/e-lite/public-servers.toml`); `install.sh` copies it to
  `~/.config/e-lite/` only when absent.
- Tests: first-run creation, no-overwrite, invalid file, generation disabled,
  URL validation and an e2e doctor/status check.

## 0.1.0-alpha.26 — 2026-11-06

### Electrum 1.6 compatibility

The server now negotiates 1.4–1.6 and serves the 1.6 method shapes to clients
that ask for 1.6. 1.4 clients keep the previous responses on the same instance.

- **`blockchain.block.headers`** returns `headers` (an array of hex headers)
  for 1.6 clients; 1.4 clients keep the concatenated `hex` string. `count`,
  `max`, `root` and `branch` are unchanged.
- **`server.version` first** is enforced for 1.6: if another message arrives
  before it, negotiation is capped at 1.4.2 instead of advertising 1.6.
- **`mempool.get_info`** returns `mempoolminfee`, `minrelaytxfee` and
  `incrementalrelayfee` from Core.
- **`blockchain.estimatefee(number, mode)`** accepts `unset`, `economical` or
  `conservative`, passes the mode to Core and caches per `(target, mode)`.
  Invalid modes are `-32602`.
- **`blockchain.transaction.broadcast_package`** wraps Core `submitpackage`.
  The stable response is `{"success": bool, "errors": [{"txid", "error"}]}`;
  `verbose=true` returns Core's dictionary. Requires protocol 1.6.
- Version negotiation still downgrades gaps (`1.4.3`, `1.5`) to `1.4.2` and
  future versions to the newest implemented version below them.
- Tests: 1.6 negotiation and first-message cap, headers shape per version,
  `mempool.get_info`, fee modes and a real parent+child package broadcast in
  `tests/e2e_electrum16.py`.

1.7 remains unimplemented and is not advertised.

## 0.1.0-alpha.25.4 — 2026-11-06

### Patch: negotiate down instead of disconnecting

A client that requested an unimplemented or skipped version (for example `1.5`
or `1.6`) was disconnected even though the server could serve it with `1.4.2`.
The server now picks the newest implemented version at or below the client's
maximum and reports it in the `server.version` result. Only clients that cannot
speak 1.4 at all are refused.

- `ProtocolVersion::highest_available` replaces the strict range intersection;
  the implemented versions are listed in `SERVER_VERSIONS`.
- `1.4.3` (Namecoin-only), `1.5` (skipped) and future `1.6`/`1.7` now negotiate
  to `1.4.2` instead of erroring.
- Inverted ranges (`max < min`) are still `-32602`; a client maximum below
  `1.4` still closes the connection.
- Tests cover the selection matrix, the socket close path and the protocol
  fallback for future versions.

This is a deliberate leniency over the strict “no intersection, close” rule so
wallets asking for a newer protocol keep working; the response tells them the
actual version.

## 0.1.0-alpha.25.3 — 2026-11-06

### Patch: Electrum 1.4 protocol conformance

Finishes the 1.4 surface before starting 1.6/1.7. No schema, config or index
changes; existing wallets keep working and negotiate the same version.

- **`server.version` negotiation**: versions are parsed numerically (`1.4`,
  `1.4.2`, …), the server advertises and returns the negotiated version, only
  the first call on a connection is accepted, extra arguments are tolerated, and
  a client with no common version receives the error and is disconnected.
- **`server.features`** now advertises `protocol_min = "1.4"` and
  `protocol_max = "1.4.2"` (1.4.2 adds `scripthash.unsubscribe`, already
  implemented; 1.4.3 is Namecoin-only and is not advertised).
- **`blockchain.transaction.get(txid, true)`**: confirmed transactions now carry
  `hex`, `blockhash`, `confirmations`, `time`, `blocktime` and
  `in_active_chain`, which Core's `decoderawtransaction` does not provide.
  Mempool responses are normalised with `confirmations: 0`.
- **JSON-RPC notifications**: a request without `id` is executed without
  sending a response (required by 1.7 `server.ping` compatibility and correct
  JSON-RPC behaviour).
- **`blockchain.scripthash.unsubscribe`** returns `true` only if the scripthash
  was subscribed; a second call returns `false`.
- **`server.add_peer`** returns `false` instead of `method not found`.
- **Mempool order**: history, `get_mempool` and the status hash use the
  canonical `(-height, txid)` order, so the status is deterministic before 1.6.
- **Tests**: version negotiation/close, notification handling, unsubscribe,
  verbose transaction decoration, canonical mempool order, and the Sparrow e2e
  now checks the confirmed verbose fields.

## 0.1.0-alpha.25.2 — 2026-11-06

### Patch: onion ports mirror the Electrum listeners

`--listen 0.0.0.0:51001 --tls-listen 0.0.0.0:51002` still published the onion
on the default 50001/50002 because `[tor] onion_port`/`onion_tls_port` had fixed
defaults.

- `onion_port` and `onion_tls_port` now default to **0 = same port as the
  Electrum listener**. The example above publishes `<addr>.onion:51001` and
  `:51002`.
- Explicit `[tor] onion_port`/`onion_tls_port` still override the mirrored
  ports; a collision between the two is rejected.
- The startup log, `torrc`, persisted `tor/state`, `/status` and
  `e-lite onion show` all use the effective ports.
- Tests: listener mirroring (51001/51002), explicit override, and torrc
  rendering.

## 0.1.0-alpha.25.1 — 2026-11-06

### Patch: load the bundled Tor libraries

On Debian 13.7 (and any host without `libevent-2.1.so.7`), the managed Tor
exited with status 127 because the expert bundle's `tor` has no RPATH and the
loader could not find the `libevent`/`libssl`/`libcrypto` files shipped next to
it.

- **`LD_LIBRARY_PATH` for Tor**: e-lite prepends the Tor binary directory to
  the child environment, so the bundled libraries are always found without
  installing system packages. Existing `LD_LIBRARY_PATH` values are preserved.
- **Actionable failure output**: if Tor exits unexpectedly, the last lines of
  `tor.log` (including the loader error) are printed instead of only the exit
  code.
- **`doctor` runs `tor --version`** with the same environment and shows the
  exact error if a library is still missing.
- Tests: unit coverage for the library-path construction; the real-Tor
  integration test and a minimal Debian 12 container check (bundle without
  system libevent) pass with the new environment.

## 0.1.0-alpha.25 — 2026-11-05

### Onion works with wildcard Electrum listeners

Binding Electrum to `0.0.0.0` (or `[::]`) made the onion service fail with
`refusing to publish non-loopback target`. The wildcard socket already accepts
loopback connections, so the onion can safely target `127.0.0.1` (or `[::1]`).

- **Wildcard mapping**: `0.0.0.0:PORT` → `127.0.0.1:PORT` and
  `[::]:PORT` → `[::1]:PORT` for both the plain and TLS onion ports. A specific
  non-loopback address is still refused.
- **Explicit targets**: `[tor] target_plain`/`target_tls` (and
  `--tor-target-plain`/`--tor-target-tls`) override the listener-derived target.
- **Accurate `onion show`**: e-lite persists the effective ports/targets in
  `<data-dir>/tor/state`, so `e-lite onion show` reports the real onion ports
  even when the daemon was started with CLI flags.
- Tests: wildcard/IPv6 mapping, explicit target rendering, and `onion show`
  with a state file.

## 0.1.0-alpha.24 — 2026-11-05

### Adaptive memory and a managed Tor onion service

**Memory (alpha.24)**

- **Hot memory budget.** e-lite derives a ceiling from total RAM (75 %), keeps a
  reserve for the OS/Core/page cache (`max(1024, RAM/8)`), and grows the RocksDB
  block cache in 256 MiB steps while `MemAvailable` and `buff/cache` stay
  healthy. It shrinks 25 % (floor 256 MiB) under pressure, with a two-sample
  hysteresis, and can shrink even a manual `cache_mib` (now a maximum).
- **Larger memtables.** The RocksDB memtable budget is `RAM/8` (was `RAM/16`),
  reducing flush/compaction churn; the WAL cap scales with it.
- **Pre-resume shrink.** When the bulk catch-up is about to resume Core, the
  cache drops to the live budget so Core has room for its block index.
- **Observability.** Startup budget line, per-change log lines, and
  `memory.cache_mib`, `cache_max_mib`, `memtables_mib`, `reserve_mib`,
  `budget_mib`, `cached_mib` in `/status`/`e-lite status`.

**Tor (alpha.24)**

- **Managed Tor by default.** `[tor] mode = "managed"` starts e-lite's own Tor
  process (no system daemon), publishes Electrum as an onion service, and uses
  that Tor's SOCKS for rawtx.
- **Plain + TLS onion ports**: 50001 and 50002. TLS uses an automatically
  generated self-signed certificate (ECDSA P-256, SAN includes the onion
  address) when no operator certificate is configured; `CertWatcher` reloads it.
- **Installer-provisioned Tor bundle.** `install.sh` downloads the official Tor
  expert bundle (pinned version, SHA-256 verified) to
  `<prefix>/lib/e-lite/tor`; falls back to `tor` in `PATH`. `--without-tor`
  skips it. If no binary is available, indexing continues with a warning.
- **`e-lite onion show` / `e-lite onion reset`**: show the address/ports/state;
  reset moves the onion keys to `onion.prev` and the next start generates a new
  address. Reset refuses while the managed Tor is running.
- **Status and security.** `/status` and `e-lite status` show the onion address
  and bootstrap state. A warning documents that the onion has no client
  authentication or rate limiting.

Tests: unit coverage for the memory policy, torrc rendering, onion validation,
self-signed certificate SAN/permissions, fake-Tor lifecycle, real-Tor onion
generation (ignored unless `E_LITE_REAL_TOR` is set), and `onion reset`.

## 0.1.0-alpha.23 — 2026-11-05

### Apply path: overlap UTXO reads with the indexer pipeline

Mainnet bulk indexing was dominated by `apply` (300 ms–2 s per block) while
block reads, decode and prefetch waits stayed near zero. `apply` was doing every
prevout lookup serially in the single apply thread, so random UTXO reads could
not overlap with anything.

- **Speculative prevout prefetch in reader threads.** Each reader now resolves a
  block's prevouts from the committed UTXO set immediately after decoding, while
  the apply thread works on an earlier block. The prefetched set is a superset
  (it ignores group-local creations/deletions); apply still consults the
  in-memory group overlay first and falls back to a point lookup for any miss,
  so correctness does not depend on the speculation. New `prefetch` figure in
  the progress line isolates this cost from `apply`.
- **RocksDB atomic flush** across the seven column families: one flush point and
  fewer, larger L0 files instead of seven independent flushes, which cuts
  compaction write amplification on multi-CF batches.
- **Compaction shape for bulk ingest**: 2 MiB compaction readahead and a 512 MiB
  level-1 base.
- Tests: the oracle-parity suites now drive the production
  `prepare → prefetch → apply_prepared_with_prefetch` path, including blocks
  that spend outputs created earlier in the same uncommitted group.

## 0.1.0-alpha.22 — 2026-11-05

### Faster bulk indexing, readiness and wallet banner

- **Removed the cache-hit metric** (progress/status/metrics/controller). It was
  never reliable; `--cache-mib` and the RocksDB block cache remain.
- **`commit_sync = auto` (default)**: fsyncs `txids.dat` and relaxes the
  RocksDB fsync during bulk/IBD; returns to strict sync when live. A crash can
  only require reindexing the last interval, never corrupt the index.
- **Core bulk scheduling**: `pause_core_during_bulk = true` pauses Core during
  the long catch-up, resumes `core_resume_ahead_blocks` (5,000) before the tip so
  peers reconnect, then applies the relative 20k guard.
- **Prune deferral**: normal pruning is skipped while free space is at or above
  `prune_defer_free_gib` (50 GiB); emergency low-disk pruning remains. Normal
  cadence stays every 10,000 safe blocks.
- **READY / FULLY SYNCED**: one-time log lines, `/status` fields, metrics and
  `e-lite status` output. `READY` means the index reached Core's validated tip;
  `FULLY SYNCED` means Core is out of IBD and the index reached the headers.
- **Wallet banner**: dynamic `server.banner` with built-in INDEXING/READY
  templates and optional `[server] banner_file` (auto-created with
  `[indexing]`/`[ready]` sections and placeholders).
- **Apply micro-optimizations**: output script hashes precomputed in readers,
  txid set instead of all outpoints, reused effect/drain buffers.
- **Benchmark helper**: `scripts/benchmark-index.sh`.
- Tests: commit policy, prune deferral, banner sections/formatting, readiness
  log/banner in regtest e2e.

## 0.1.0-alpha.21 — 2026-11-05

### Test reliability

- **Serialized store tests that use global failpoints.** `reopen_keeps_tip...`
  could race with `failpoint_before_commit...` and fail in CI even though the
  code was correct. A test mutex now makes the suite deterministic. No runtime
  behavior change from alpha.20.

## 0.1.0-alpha.20 — 2026-11-04

### Fixes for alpha.19

- **Bulk lag guard no longer resumes immediately**: while Core was paused, the
  relative window was following the decreasing lag down, so the resume
  threshold dropped below the current lag and Core resumed on the next check.
  The baseline now stays fixed while paused and only follows the lag down while
  Core is downloading.
- **Explicit readers are no longer CPU-capped on hot reload**: `--readers`
  continues to win over the `2 × CPUs` automatic cap.
- **Log path without a parent directory** (`--log-file e-lite.log`) no longer
  fails while creating an empty parent path.
- `PreparedBlock` fields are private so only validated blocks can be applied.

## 0.1.0-alpha.19 — 2026-11-04

### Logging, indexing throughput and pruning cadence

- **Application log**: `run`, `index` and `serve` tee stdout/stderr to
  `<data-dir>/e-lite.log` (same lines as the console), append mode, 64 MiB
  rotation to `e-lite.log.1`, `--log-file`/`--log-max-mib`/`--no-log-file` and
  `[logging] file/max_mib`. Snapshots exclude the log and runtime markers.
- **Prune every 10,000 blocks by default**: `min_gain_blocks` defaults to
  `10000` instead of `288`, so the prune gate is far less intrusive. Low disk
  still triggers an emergency prune.
- **Parallel decode/txid/Merkle**: reader threads now deserialize the block,
  verify its hash and Merkle root, and compute txids; the apply thread only does
  continuity, UTXO work and batch writes. This overlaps CPU-heavy work with the
  previous block's apply/commit.
- **Commit path**: `prune_undo` is skipped when a group wrote no undo records;
  the block hash is computed once; bulk commit interval can grow to 30 s and
  batch to 4096; the controller avoids growing batches when commit already takes
  >75% of the interval.
- **Instrumentation**: progress/status/metrics now expose `decode_ms`,
  `wait_ms`, `cache_hit_known` (`cache ?` when RocksDB does not report the
  counter). Lightweight RocksDB statistics are enabled.
- **Relative bulk lag guard**: `--max-bulk-lag-blocks` now bounds additional lag
  growth from the baseline instead of freezing Core for an entire large
  catch-up; it resumes at half the growth window.
- **CPU-aware readers**: automatic reader counts are capped at `2 × CPUs`
  (explicit `--readers` still wins), avoiding 8 readers on a 2-vCPU machine.
- **Tests**: log file presence/content in e2e, relative bulk-lag window,
  prune default, decode/wait metrics, plus the existing TLS/memory/startup
  assertions.

## 0.1.0-alpha.18 — 2026-11-04

### Startup, shutdown and memory/performance

- **Fixed `/proc/meminfo` parsing** (the colon was left in the value). This was
  the real cause of `memory pressure (0 MiB)`: e-lite always fell back to 1 GiB
  total and 0 MiB available, shrinking the cache to 128 MiB and capping readers
  on machines with plenty of RAM. `doctor`/`/status` now show total, available,
  cgroup limit/usage and reclaimable cache.
- **RocksDB memtable budget is global, not per column family**. The old
  `cache/4` per CF could reserve ~1.3 GiB of write buffers plus the block cache.
  The budget is now `RAM/16` (64 MiB..1 GiB) split across the 7 CFs, and
  background jobs are capped by the CPU count.
- **Ordered startup**: `run` starts/adopts Core, waits for RPC (including block
  index loading) and only then builds the resolver, discovers P2P peers and
  starts mempool, notifications and listeners. `serve` remains the offline mode.
  This removes `p2p discovery failed`, `mempool refresh: Connection refused` and
  transient `Loading block index` noise.
- **Ordered shutdown**: mempool/notification threads observe a shutdown flag and
  are joined before Core is stopped. `Ctrl+C` ends with a local-time
  `stopped at … (uptime …)` line and no post-shutdown errors/tuning lines.
- **Progress**: height and Core now show percentages over Core `headers`, plus
  verification progress during IBD and read/apply/commit/cache metrics. A
  milestone line is logged every `milestone_blocks` (default 10,000) or
  `milestone_seconds` (default 6 h) with interval throughput and disk usage for
  e-lite and Core. Works on all networks.
- **Monotonic Core height** in the progress line (no more 615734 → 613288).
- **Optional bulk lag guard** (`--max-bulk-lag-blocks`, default 0/off): pauses
  Core P2P when a managed pruned Core is in IBD and the index lags more than the
  threshold, resuming at half. Bounds the gap and disk contention.
- **CLI coverage**: `--profile`, `--adaptive/--no-adaptive`, `--cache-mib`,
  `--write-buffer-mib`, `--background-jobs`, `--commit-interval-ms`,
  `--prefetch-blocks`, `--milestone-blocks/seconds`, pruning thresholds,
  `--core-arg`, rawtx/server flags, and `--set section.key=value` for any TOML
  field. See `docs/CLI.md`.
- **Tests**: memory/cgroup parsers, RocksDB budget, milestone timing, CLI
  overrides, TLS, and e2e startup/shutdown log assertions.

### Optional Electrum TLS

- **TLS is available on a separate, opt-in listener** (`[server] tls_listen`,
  `tls_cert`, `tls_key`; CLI `--tls-listen`, `--tls-cert`, `--tls-key`). The
  plain `127.0.0.1:50001` listener is unchanged and remains the default.
- **Any PEM certificate chain works**, including a public CA certificate reused
  from a web server. TLS 1.2+ via rustls; no client authentication.
- **Certificate reload on change**: the listener checks the PEM files every 60 s
  and rebuilds the TLS configuration when they change, so a Let's Encrypt
  renewal does not require restarting e-lite. An invalid PEM keeps the previous
  certificate and logs a warning.
- **Bounded handshake**: 10 s TLS handshake timeout; the existing 1 MiB request
  line cap and 600 s idle timeout still apply.
- **Transport refactor**: plain and TLS connections share one read/write path, so
  scripthash notifications can write to the same TLS connection. Notifications
  are delivered with at most ~250 ms of latency while a client is idle.
- **Observability**: `/status` and `/metrics` expose `tls_listen`,
  `tls_handshakes` and `tls_handshake_failures`.
- **Docs**: certificate setup, Let's Encrypt deploy hook, split-DNS, private-IP
  caveats, renewal and the nginx `stream` alternative in `docs/OPERATIONS.md`.
- **Tests**: TLS handshake + JSON-RPC integration test, certificate reload tests,
  and the regtest e2e now queries both plain and TLS listeners.

TLS encrypts the transport but does **not** authenticate wallets or rate-limit
requests; use a firewall, SSH/VPN or Tor for non-loopback listeners.

## 0.1.0-alpha.16 — 2026-11-03

### Security audit and documentation rewrite

- **Electrum request lines are capped at 1 MiB** before allocation; a client can
  no longer grow a per-connection buffer indefinitely without a newline.
- **`max_clients` is reserved atomically before spawning a client thread**, so a
  burst cannot exceed the configured limit.
- **Snapshot import rejects symlinks, hardlinks and special files**, preventing
  archive extraction outside the target or special-file creation.
- **Esplora endpoints must be http/https**; invalid schemes are skipped.
- **Non-loopback Electrum/metrics listeners log a startup warning** (no TLS or
  authentication is implemented).
- **Project memory added**: `AGENTS.md` and `docs/PROJECT_MEMORY.md` lock the
  one-line install/update UX, Core variant, documentation contract, current/
  future feature sections and audit rules.
- **Documentation regenerated** around `INSTALL`, `ARCHITECTURE`, `OPERATIONS`,
  `PRIVACY`, `FEATURES`, `ROADMAP`, `TUNING`, `AUDIT` and `GETTING-STARTED`.
- **Audit report** added in `docs/AUDIT.md` with fixed findings and residual
  risks (TLS/auth, Core RPC TLS, snapshot archive limits, provider trust).
- **Session handoff** added in `docs/HANDOFF.md` (current state, open risks,
  next priorities) and linked from `AGENTS.md`; new sessions should start there.

## 0.1.0-alpha.15 — 2026-11-03

### Container safety and conservative reader scaling

- **Automatic readers are capped by profile and memory**: 4 (HDD), 8 (SSD),
  16 (NVMe), never 28+ just because a read was fast. The controller now adds
  readers only when read latency is high and the apply stage is not the
  bottleneck.
- **Explicit `readers`, `cache_mib` and `commit_interval_ms` are locked**: the
  controller does not override them, except a critical memory floor.
- **Container-aware memory accounting**: `MemTotal`/`MemAvailable` are min'd
  with the cgroup v2/v1 limit and usage, so LXC/Docker do not over-allocate
  cache or workers.
- **Memory pressure backs off cache first**, keeps a reader floor of 2, and no
  longer collapses to one reader/64 MiB on a transient dip.
- **Docs**: `docs/TUNING.md` now documents the reader caps, locks and LXC
  OOM troubleshooting.

## 0.1.0-alpha.14 — 2026-11-03

### Autotuning, profiles and hot reload

- **Startup profiles**: `auto | hdd | ssd | nvme | manual`, selected from the
  `rotational` hint and RAM. Profiles set readers, commit interval, batch size
  and block-cache size.
- **Event-driven controller**: only changes one knob every 30 s, with hard
  bounds and no action while the disk/prune guards or reorg repair are active.
  It adjusts readers, commit interval/batch and block cache from measured
  read/apply/commit latency, tx/s and cache hit rate.
- **Hot reload with `SIGUSR1`**: readers, cache, commit interval, batch,
  profile, adaptive flag and pruning thresholds are applied to the running
  process. Invalid files keep the previous config; cold fields are reported.
- **Dynamic RocksDB cache**: the shared block cache is retained by `Store` and
  resized with `Cache::set_capacity`; mutable `max_background_jobs` is applied
  at runtime when RocksDB accepts it.
- **Bigger automatic cache/jobs ceilings**: auto cache is no longer capped at
  2 GiB, and background jobs are no longer capped at 4.
- **Instrumentation**: `/status` exposes a `tuning` object with effective
  readers/commit/batch/cache/profile and read/apply/commit/tx/cache EWMA
  metrics; `/metrics` exports the same values.
- **Documentation**: `docs/TUNING.md` and updated README/config examples.
- **CLI help** corrected for the new manual-prune default.

## 0.1.0-alpha.13 — 2026-11-03

### From-zero pruned IBD is manual by default

- **`prune_mib` defaults to `1` (manual pruning).** Core no longer deletes block
  files on its own during IBD. e-lite's prune gate is the only pruning
  authority: it never requests a cut above `committed - 288`, so Core cannot
  prune a block the index still needs. `0` remains a full node.
- **Automatic pruning requires an explicit opt-in.** `prune_mib >= 550` is
  rejected unless `[pruning] allow_auto_prune = true`. This prevents the
  from-zero default from recreating the prune-gap race.
- **Lag guard is manual-mode aware.** It is skipped for `prune_mib = 1`. For
  automatic pruning, `lag_limit` is hard-capped at 224 blocks (64 below Core's
  288-block floor), the default is 192, it resumes at 50 %, and the live check
  runs every 100 ms instead of once per second.
- **Disk backpressure keeps draining.** Below `min_free_gib` Core downloads
  pause but indexing continues, so the prune gate can delete old block files and
  free space. Only below `critical_free_gib` does indexing stop. This removes
  the old all-stop deadlock.
- **Prune requests are cheaper.** `min_gain_blocks` defaults to 288 and a new
  `prune_interval_seconds` (default 30) throttles no-op `pruneblockchain` calls
  that made Core flush during IBD. Low disk bypasses both as an emergency prune.
- **Prune-gap repair.** If automatic pruning still leaves a gap (or an existing
  index is behind an externally pruned Core), the indexer can fetch only the
  missing blocks from the validated rawtx cache, P2P or Esplora sources,
  verify hash/merkle, and continue. `repair_max_gap_blocks` (default 2048)
  bounds the work. No snapshot or full-chain backup is created.
- **Progress lines show live Core/prune height.** The per-call snapshot is no
  longer used as live lag, so a stale `lag 0` cannot hide a prune race.
- **Scripts and docs** use `prune_mib = 1` for from-zero signet/testnet4 runs.
  No snapshot is required or created by the flow.

## 0.1.0-alpha.12 — 2026-11-02

### Download and prune fixes

- **The lag pause is no longer undone by the crash-recovery marker.** The
  `core-paused-by-e-lite` recovery ran on every catch-up call, so a live lag
  pause was resumed on the next iteration, bypassing the 75% hysteresis and the
  reader check. Recovery now only runs when no live lag pause is in progress.
- **Prune requests are throttled by the requested height.** Core prunes at
  block-file granularity and can return the same height for thousands of
  blocks; retrying on every supervisor tick forced Core to flush during IBD.
  The supervisor now records the last requested height and only retries after
  `min_gain` blocks of safe-height progress.
- **Default `max_core_lag_blocks` is 512** (still capped by `blocks -
  pruneheight - 32`). On signet with `prune=550` the retention is thousands of
  blocks, so the old 256 limit paused Core unnecessarily.
- **Progress lines include `core`, `lag` and elapsed seconds**, so the catch-up
  burst rate is interpretable. All-in-one and network tests now use 512.

## 0.1.0-alpha.11 — 2026-11-02

### Autonomous startup and Core lifecycle

- **`run` waits for Core and auto-initializes.** With an empty database it binds
  the ports, reports `WAITING_FOR_CORE` and initializes as soon as Core appears.
  With an existing database it serves cached history while Core is down.
- **`[core] manage = external | auto | managed`.** `auto` uses a running Core
  read-only or starts it; `managed` always starts it. Only a Core started by
  e-lite can be paused, pruned or stopped.
- **Core launcher/supervisor**: `bitcoind`, `core_datadir`, `prune_mib`
  (`550` max pruning, `1` manual, `0` full), `extra_args`, `restart`,
  `stop_on_exit`, `log_file`, PID tracking and adoption after a crash.
- **Local-only pruned IBD.** A pruned Core that e-lite did not start, or a
  remote pruned Core, is rejected for a from-scratch index with instructions;
  e-lite never pauses a node it does not own.
- **Dynamic lag guard** based on `blocks - pruneheight` (default 256, resume at
  75%) instead of a fixed 32. It is reader-aware and does not pause while
  `pruneheight == 0`.
- **Disk guard on both filesystems** (index and Core datadir): below
  `min_free_gib` it pauses Core downloads and indexing, with adaptive
  hysteresis. The check is independent of Core RPC (`disk_tick`) and the
  indexer checks again before Core RPC and during block application; an
  uncommitted group is discarded so the disk is not pushed further. A starting
  warning reports free space below the threshold.
- **Reorg safety:** after a rewind the indexer re-reads the committed tip
  before replaying blocks.
- **From-zero disk requirement:** signet needs ≥15 GiB free and testnet4 ≥20 GiB
  for a full acceptance run; `tests/e2e_networks.sh` refuses to start below
  `E_LITE_E2E_REQUIRED_FREE_GIB` and documents how to override it.
- **Pause marker** so a crash while Core was paused is recovered on restart.
- **Prune height** now records the height Core actually returns, not the
  requested one.
- **Error classification**: transient “pruned data”/Core outages are retried;
  genesis/schema/txids/prune-gap failures are fatal.

### Configuration and networks

- `network = "auto"` detects the chain from Core or an existing database.
- Per-network Esplora defaults (mainnet, signet, testnet4; none on regtest).
- `rpc_user`/`rpc_password` for a remote Core; WAN is documented as HTTP over
  SSH/VPN.
- `parse_url` uses each network's default RPC port and supports IPv6 literals.
- Simplified config: removed `[index] mode`, `[pruning] enabled`,
  `undo_margin_blocks`, `core_recent_blocks_margin`, `min_gain_blocks`,
  `resume_core_lag_blocks`, `max_group_txs`, `commit_interval_ms` and
  `p2p_timeout_seconds`; renamed `bulk_readers`→`readers`,
  `db_cache_mib`→`cache_mib`, `cache_gib`→`cache_mib`.
- Validation: `prune_mib` must be `0`, `1` or `>= 550`; `data_dir` cannot be
  inside the Core datadir; `managed` requires an explicit network and loopback
  RPC.

### Performance and correctness

- The global index lock is no longer held across Core or external network I/O;
  a slow `transaction.get` cannot block index commits.
- Mempool refresh builds the candidate outside the write lock.
- `db_bytes` is cached; header checkpoint proofs use a cached header chain;
  history is capped at 100k entries per script.
- Notifications use a dirty-set: the indexer marks scripts touched by each
  confirmed block and the mempool marks additions/removals/height changes, so
  only affected subscriptions are recomputed. Reorgs force a full refresh.
- RocksDB tuning is automatic (`cache_mib = 0`, `readers = 0`) and open files are
  bounded.
- `family_digests` streams the flat TXID file instead of allocating it whole.

### Bulk block ingest

- **Bulk block ingest**: e-lite can read Core's `blk*.dat` directly
  (`[index] block_source = "auto" | "rpc" | "files"`, `--block-source`,
  `--blocks-dir`). Handles Core's default `-blocksxor` key (`blocks/xor.dat`),
  preallocated files, partial tails and file pruning. A persistent locator
  (`blockfiles.idx`) makes restarts cheap. RPC is always the fallback.
- **Faster ingest**: one batched `getblockhash` per window instead of one RPC per
  block; `readers` parallel fetch/read workers with an ordered writer;
  adaptive group commits (`batch_blocks`, internal TX/time limits); no fixed 1 s
  sleep between windows; TXIDs computed once and reused for the Merkle root.
- **Undo only inside the reorg window**: historical blocks no longer write undo
  records that would be deleted a few commits later. `prune_undo` now removes
  every record below the window in one pass, so the canonical digest is
  independent of the commit batch size.
- **RocksDB tuning**: shared block cache (`cache_mib`), bloom filters, larger
  write buffers and batched prevout reads (`multi_get_cf`).
- **Prune/ingest guard**: with automatic pruning, e-lite pauses Core downloads
  when the index lags more than the dynamic `max_core_lag_blocks` limit, then
  resumes at 75% of that limit.
- `doctor` reports the blocks directory, file count, XOR state and magic.
- Verified: A/B digest parity (RPC vs files), crash/restart parity, existing
  regtest and pruned-IBD E2E.

### Documentation

- New `scripts/e-lite-all-in-one.sh`: one command to install/update
  dependencies, Rust, e-lite and Bitcoin Core; configure; run a regtest quick
  test, the local e2e suite or the full signet/testnet4 acceptance; and write a
  report (`~/e-lite-test/last-report.txt`).
- New `docs/GETTING-STARTED.md`: prerequisites, disk planning, the one-command
  flow, regtest quickstart, full signet/testnet4 acceptance, troubleshooting and
  checklist.
- `docs/benchmarks/F2.md` records the two full-sync attempts that filled the
  disk, the root cause, the fix and the remaining manual acceptance.
- `docs/ADR-020-CORE-LIFECYCLE.md` documents the Core lifecycle and guards.

## 0.1.0-alpha.10 — 2026-11-02

- **Reorg both ways**: when Core's chain is shorter than the index
  (`invalidateblock`, manual rollback), the index now rewinds to the common
  ancestor instead of staying on the orphaned tip. Longer-chain reorgs keep
  using the undo window.
- **Mempool-aware history**: `blockchain.scripthash.get_history` appends
  unconfirmed transactions (`height` 0, or -1 when a parent is unconfirmed).
- **Correct balance semantics**: `confirmed` ignores mempool events and
  `unconfirmed` carries the net delta; previously a mempool spend was subtracted
  twice.
- **Chained mempool height**: `get_mempool`/`get_history` report `-1` for
  transactions with unconfirmed parents.
- **Checkpoint proofs**: `blockchain.block.header` and `blockchain.block.headers`
  accept `cp_height` and return the header merkle `root` plus `branch`.
- E2E additions: transaction merkle proof against the header root, checkpoint
  proof over the whole header chain, sender balance semantics, and a full
  reorg cycle (confirm -> invalidate -> rollback -> re-mine).

## 0.1.0-alpha.9 — 2026-11-02

- **Offline startup**: `e-lite run` now starts while Core is down. The database
  identity is trusted from disk, the Electrum/metrics ports bind, and confirmed
  history/balances are served from the cached index (`CORE_UNAVAILABLE`).
- **Lazy Core cookie**: construction no longer fails when the cookie file is
  missing (Core deletes it on shutdown); the cookie is loaded on the first
  request and refreshed on 401.
- Disk backpressure is scoped to `pruned-ibd`, so a full-core node is never
  paused by e-lite.
- Resilience E2E now restarts e-lite while Core is down and verifies 101 history
  entries, then recovery and SIGTERM exit 0.

## 0.1.0-alpha.8 — 2026-11-02

- **Silent payments opt-in gate**: `[silent_payments] enabled = false` by
  default. The subscribe/unsubscribe methods return an explicit server error
  instead of `unknown method`; enabling them reports that the BIP352 indexer is
  not in this build.
- **ADR-018** documents the protocol shape, the scan-key privacy constraints,
  the prevout-pubkey requirement (schema v2 or on-demand scan) and the future
  implementation/test plan.
- No protocol or schema change for normal wallets.

## 0.1.0-alpha.7 — 2026-11-01

- **Mode B (`pruned-ibd`)**: index while Core runs with `prune=1` (manual).
- **Disk supervisor** with hysteresis: pauses Core downloads
  (`setnetworkactive(false)`) below the low watermark and resumes above the high
  watermark.
- **Safe prune gate**: minimum of committed-tip minus undo margin, earliest
  active rawtx reader minus one, and Core tip minus recent-blocks margin. Never
  prunes while paused, behind, or with an active reader; `min_gain` prevents
  request storms. Only `pruneblockchain` is used; no block files are deleted by
  e-lite.
- Bounded indexing steps so the supervisor can act during long catch-ups.
- Reader tracking around `transaction.get` so pruning respects in-flight rawtx
  resolutions.
- Tests: gate formula, hysteresis, reader blocking, prune gain, and a regtest
  `prune=1 -fastprune` E2E: 2000 blocks indexed, Core pruned to 1530, old block
  unavailable in Core while e-lite still served 2000 history entries.

## 0.1.0-alpha.6 — 2026-11-01

- **BIP324 v2 P2P transport** (encrypted, via the `bip324` crate) with fallback
  to v1 for peers that do not support it. Per-peer Tor isolation is unchanged.
- Correct BIP324 application framing (1/13-byte message type) and short type IDs
  for block/ping/pong/getdata.
- New `[rawtx] p2p_v2` option (default true).
- Tests: v2 mock peer, v1 fallback, and live v2/v1 fetches against Bitcoin Core.

## 0.1.0-alpha.5 — 2026-11-01

- **Clean shutdown**: SIGTERM/SIGINT stop at a commit boundary, flush the pending
  group and exit 0.
- **Core outage resilience**: transient RPC failures set `CORE_UNAVAILABLE`,
  keep serving confirmed reads from the index and retry with backoff (1s→30s).
  Core cookies are reloaded on 401, so a Core restart (which rotates the cookie)
  recovers automatically.
- **Snapshot export/import**: `export-snapshot --output` writes a tar.gz with a
  manifest (network, genesis, tip, txnum, schema, canonical digest);
  `import-snapshot --input` restores into an empty data directory and verifies
  the digest.
- Rate-limited protocol error logging (method + code, no sensitive data).
- New resilience test: Core stop → reads still work → Core restart with rotated
  cookie → recovery → SIGTERM exit 0.
- E2E now also checks snapshot digest equality after restore.

## 0.1.0-alpha.4 — 2026-11-01

- **Mempool overlay** rebuilt from Core: per-script unconfirmed history, balance
  deltas, mempool UTXOs and spent outpoints (including chained transactions),
  refreshed every 3 s and immediately after a new block.
- **Fees**: `blockchain.estimatefee`, `blockchain.relayfee` and
  `mempool.get_fee_histogram`; `estimatefee` never returns `-1`.
- **Broadcast and verbose tx**: `blockchain.transaction.broadcast`,
  `transaction.get(..., true)` and `blockchain.block.stats`.
- **Push notifications**: headers and scripthash status are pushed to subscribed
  clients; status includes mempool entries.
- `scripthash.get_mempool`, unconfirmed balances and `listunspent` now include
  mempool data.
- `run` always follows the tip (`--no-follow` applies only to `index`).
- Per-request line limit (1 MiB) and notification counters in `/status` and
  `/metrics`.
- Tests: Sparrow-like connect sequence, broadcast, mempool overlay, notifications,
  fee fallbacks and block stats.

## 0.1.0-alpha.3 — 2026-11-01

- **Pruned Core support for serving**: the index must already cover the chain;
  `doctor` and the indexer refuse a prune gap with a clear message.
- **Raw transaction resolver** with a strict order: local Core -> validated cache
  -> external sources. Whole blocks only, never TXID lookups.
- **Verification**: block hash/header against the local chain, Merkle root, TXID
  and BIP141 witness commitment. Invalid data is never served or cached.
- **Sources**: Esplora-compatible `/block/:hash/raw` endpoints and a minimal
  Bitcoin P2P client (version/verack, getdata, NODE_NETWORK filtering).
- **Privacy defaults**: Tor SOCKS5h with stream isolation and fail-closed without
  Tor; 3 decoys per external fetch; 10 GiB validated LRU cache; no sensitive
  labels in metrics.
- `transaction.get` serves from the cache on the second access.
- New `[rawtx]` configuration and `rawtx` counters in `/status` and `/metrics`.
- Tests: witness/Merkle verification, cache eviction, source fallback, decoys,
  fail-closed Tor, P2P handshake, prune-gap guard, plus live Esplora and live P2P
  checks against real endpoints.

## 0.1.0-alpha.2 — 2026-11-01

- Fix: the undo-record reader required 85 bytes per spent input instead of the
  81 written by the encoder, which aborted indexing with `impossible spent count`
  once the reorg window started pruning old records (seen on Signet at ~4.6k).
- Pruning no longer decodes old undo records: deleting them at the window
  boundary is idempotent and cannot stop indexing.
- Undo layout sizes are now shared constants for writer and reader; the minimum
  record length check is exact (92 bytes).
- Tests: spent-heavy undo round-trip, minimal record, prune boundary with
  input-heavy records.
- No schema change: existing databases are read as-is.

## 0.1.0-alpha.1 — 2026-11-01

- First public alpha: full-core indexing from Bitcoin Core (RocksDB).
- Electrum 1.4 server on loopback: headers, scripthash history/balance/UTXOs,
  transaction.get, get_merkle, id_from_pos, server.features/version.
- Networks: regtest, signet, testnet4, mainnet (explicit opt-in).
- Reorg handling with a bounded undo window (288 blocks).
- Prometheus /metrics and JSON /status on loopback.
- One-line installer/updater with optional official Bitcoin Core install.
- Debian 12+ package (.deb) built in debian:12; no systemd unit.

### Known limitations
- Confirmed chain only: no mempool, no push notifications, no broadcast.
- `full-core` only (Core prune=0); rawtx requires the block to exist in Core.
