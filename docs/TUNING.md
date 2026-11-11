# Autotuning and hot reload

e-lite keeps a small set of runtime knobs so the same binary can run on a slow
rotational disk and on a fast NVMe drive. The controller is deliberately
conservative: it changes one thing at a time, has a cooldown, and never
overrides the disk/prune safety guards.

All settings work on every network. See [`CLI.md`](CLI.md) for the full list of
command-line overrides and `--set section.key=value`.

## Supported minimum and startup checks

Indexing and validating the chain at the same time is what makes the memory
requirement non-trivial. e-lite refuses to start `run`/`index` below the
supported class and explains what it detected:

| Network | Minimum usable RAM |
|---|---:|
| mainnet | **8 GB class** (`7500 MiB` after firmware/kernel reservations) |
| testnet4, signet, regtest | **4 GB class** (`3800 MiB`) |

The number is the **effective** total: the minimum of `/proc/meminfo` and every
visible cgroup v2/v1 limit. A container limited to 2 GiB on a 32 GiB host is
treated as a 2 GiB machine. `doctor` prints the detected source, host total and
cgroup state; `--allow-low-memory` (or `E_LITE_ALLOW_LOW_MEMORY=1`) skips the
check for CI/regtest, and is not supported for real use. `serve` does not
enforce the minimum: it only answers queries from an existing index.

`run`/`index` also reject, before creating certificates or opening the
database, any memory plan that does not fit or any Core argument that e-lite
must own (see below).

## Machine plan (CPU, RAM, disk)

At startup e-lite detects the machine and derives the defaults:

- **CPU**: unique `(physical id, core id)` pairs from `/proc/cpuinfo`, then
  affinity and the cgroup CPU quota (`cpu.max` v2, `cpu.cfs_quota_us` v1).
  Logical SMT threads are not counted as cores.
- **RAM**: `/proc/meminfo` plus the cgroup v1/v2 hierarchy, with a separate
  reserve and page-cache floor.
- **Disk**: `/proc/self/mountinfo` + `/sys/dev/block` classify the data and Core
  paths as NVMe, SATA SSD, HDD, virtual, network, ZFS, overlay or unknown; it
  also records whether both live on the same device.

Example on a 4C/8T i7-6700T with 32 GiB, 18 GiB available and a SATA SSD:

```text
cpu 4C/8T (eff 8, cpuinfo, smt) | mem total 32 GiB avail 18 GiB effective 14 GiB
(core reserve 4 GiB, reserve 2 GiB, page floor 0.9 GiB)
data disk sata-ssd (same=yes) | core par=3 rpcthreads=4 dbcache=4096
index readers=4 jobs=2 cache 3.5..8 GiB memtables=880MiB commit=30000ms batch=512
```

The same line is printed at startup and stored in `/status.machine`. Explicit
settings in `[index]`/`[core]` still win; the plan is the automatic default.

| Machine | readers | jobs | core par | core rpcthreads | cache ceiling (32 GiB) |
|---|---:|---:|---:|---:|---:|
| 4C/8T SATA SSD | 4 | 2 | 3 | 4 | 8 GiB |
| 4C/8T NVMe | 4 | 2 | 3 | 4 | 8 GiB |
| 8C/16T NVMe | 8 | 4 | 7 | 8 | 8 GiB |
| 2C/4T HDD | 2 | 2 | 1 | 4 | 4 GiB |

Virtual/unknown/ZFS/overlay devices use the SATA values and a warning. Network
filesystems are treated as the slowest class.

## Memory budget (fitted to available RAM)

The installed RAM is a ceiling, not the allocation basis. e-lite plans from
what the OS can give it now:

```
core_reserve = fitted or operator -dbcache if e-lite will start Core
               (0 for external, already-running Core or `serve`, whose memory is
               already in MemAvailable)
effective    = min(total, MemAvailable - core_reserve)
reserve      = memory_reserve_mib, else clamp(effective/8, 2048 MiB, 4096 MiB)
                         (effective/4 for machines of 2 GiB or less)
page_floor   = clamp(effective/16, 512 MiB, 4096 MiB)   # reported, not added
                                                        # to the shrink floor
usable       = effective - reserve
ceiling      = min(effective * memory_fraction, usable)  # 0 -> 0.75, never raised
runtime      = clamp(effective/32, 96 MiB, 1024 MiB)
memtables    = write_buffer_mib, else clamp(effective/16, 512 MiB, 2048 MiB)
               shrunk so that memtables + runtime + 64 MiB <= ceiling
cache_max    = min(ceiling - runtime - memtables, disk cap)
cache_start  = cache_mib or min(effective/4, cache_max)
```

The cache ceiling depends on the disk class:

| Disk | 8 GiB | 16 GiB | 32 GiB |
|---|---:|---:|---:|
| NVMe / SATA / unknown / virtual | 2 GiB | 4 GiB | 8 GiB |
| HDD | 1 GiB | 2 GiB | 4 GiB |
| Network | 1 GiB | 2 GiB | 2 GiB |

Note: these are **ceilings**, not allocations. The hot controller only grows
towards them when `MemAvailable` leaves room.

Typical plans (the exact numbers depend on `MemAvailable`):

| Installed | Available | Core reserve | effective | reserve | memtables | cache start..max | planned |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 8 GiB | 6 GiB | 1 GiB | 5 GiB | 2 GiB | 512 MiB | 1.25..2 GiB | ~1.9 GiB |
| 16 GiB | 12 GiB | 2 GiB | 10 GiB | 2 GiB | 640 MiB | 2.5..4 GiB | ~3.5 GiB |
| 32 GiB | 18 GiB | 4 GiB | 14 GiB | 2 GiB | 880 MiB | 3.5..8 GiB | ~4.8 GiB |

On 8 GiB, e-lite plus Core and the reserve fit in about 5.5-6 GiB, leaving
room for Umbrel and the page cache. On 16 GiB the cache doubles; on 32 GiB it
can reach 8 GiB if the OS actually has it free. More RAM means more cache, not
a bigger fixed reservation.

`planned = runtime + memtables + cache_start` must satisfy
`planned + reserve <= effective`. An explicit `memory_fraction` is a hard
request: if the components do not fit, memtables and cache are reduced instead
of raising the ceiling. Regression tests cover 8 GiB and 16 GiB machines.

Two different budgets are reported and must not be confused:

* **planned** — what e-lite intends to keep resident;
* **observed** — `/status.rocksdb` (`memtable_bytes`, `block_cache_bytes`,
  `table_readers_bytes`) and the process RSS.

Indexes and bloom filters stay outside the data block cache for read speed, so
their memory (`rocksdb.estimate-table-readers-mem`) is reported separately. On a
260 GiB index and with `max_open_files = 8192` that block can be 4-6 GiB; it
must appear in `/status.memory`. Lower it with `[index] max_open_files` when
memory is tight (2048/4096 on 8/16 GiB) and A/B it against the cache.

`cache_mib` is a **maximum**, not a fixed value. The controller may use less
under pressure and grow back up to it. `write_buffer_mib` is fixed when the
store opens (changing it requires a restart); this release deliberately does
not resize RocksDB memtables hot because `WriteBufferManager::set_buffer_size`
can force flushes and stall commits.

### Hot cache controller

The ceiling uses `MemAvailable` directly. That value already excludes
memtables, runtime and RocksDB table readers, so adding and re-subtracting the
memtable/runtime budgets counted that memory twice and starved the cache.

```
reserve_dyn  = min(startup_reserve, max(1024 MiB, MemAvailable/2))
cache_ceiling = min(profile_cap, MemAvailable - reserve_dyn)
```

- **grow +256 MiB** only when `MemAvailable >= reserve_dyn + cache + margin` and
  **p95 prefetch** is slow. `margin = clamp(effective/8, 256 MiB, 2 GiB)`.
  A single 10-15 s block cannot drive growth by itself.
- **shrink** when `cache > cache_ceiling` or `MemAvailable < reserve_dyn`.
- **critical**: below 1 GiB available, act on one sample; below 256 MiB reduce
  readers and allow the cache to fall to 8 MiB.
- two consecutive samples and a 5-minute reversal dwell prevent the
  grow/shrink flapping that previously evicted the UTXO set.

`/status.memory` keeps planned memtables and **actual** RocksDB memory apart:
`memtables_mib` is the budget, `memtable_bytes_mib` is the real memtable, and
`table_readers_mib` is the index/filter block that lives outside the data
cache. Lower `[index] max_open_files` when that block is too large on 8-16 GiB.

Cache decisions use `MemAvailable` only. A low page-cache number is expected
while the RocksDB cache holds the UTXO working set. `/status.tuning` exposes
`prefetch_p50/p95/max` and `wait_p50/p95`; the progress line prints them. A
byte pause/resume requires 10 s at the watermark (`reader_pressure` pauses
immediately). The memory line prints SReclaimable, Shmem, ZFS ARC when
available and the process RSS.

**ZFS ARC is invisible to `MemAvailable`.** When ARC grows past a quarter of
RAM, the cache controller shrinks the RocksDB cache on a false signal while
ARC holds the memory. e-lite warns at startup and at most once per hour when
that happens, suggesting `zfs_arc_max = min(25% RAM, 8 GiB)`. On a ZFS pool,
cap the ARC so the kernel's `MemAvailable` and e-lite's ledger describe the
same reality (for example `echo 8589934592 > /sys/module/zfs/parameters/
zfs_arc_max` for 8 GiB on a 32 GiB host, persisted in `/etc/modprobe.d/`).
`/status` and the `debug:` lines report the ARC hit rate, so you can check
what the cap costs in read warmth.

**Dynamic ARC (default on when e-lite can write it).** `zfs_arc_dynamic`
lets e-lite manage the cap per phase instead: while Core downloads or the
index drains, the cap grows to `zfs_arc_max_mib` (0 = half of total RAM —
the block reads are nearly free at a 97-98 % hit rate); once fully synced
the cap drops to `zfs_arc_min_mib` (default 1024 MiB) and the RAM becomes
visible to `MemAvailable` again, so the memory plan and the cache recover.
Writes go to `/sys/module/zfs/parameters/zfs_arc_max` (bytes) on the 5 s
`el-tune` tick with a 5 min dwell, logged as `arc: cap X -> Y MiB (phase)`.
It needs root or a writable parameter: without permission e-lite stays in
warn-only mode and logs it once. Setting `zfs_arc_dynamic = false` freezes
whatever cap is in place — remember to raise it again before a long IBD.

## Controller cadence

The controller is a dedicated thread (`el-tune`), not part of the indexing
loop, so one long `catch_up_limited` call cannot delay protection:

| Loop | Cadence | Purpose |
|---|---:|---|
| sampling / protection | 5 s tick, acts immediately on critical memory | stop growth, shrink, drop readers |
| optimization | 30 s minimum between changes | cache/commit/batch/readers |

Every change is logged with the reason and published in
`/status.tuning.adapt_steps`. Automatic changes never override a locked
`batch_blocks` value.

## RocksDB descriptors and open files

`max_open_files` defaults to 8192, but it is capped by the process
`RLIMIT_NOFILE` minus a reserve for client sockets, Core RPC, providers and
logs. `RLIM_INFINITY` is handled explicitly: it used to wrap to a negative
integer and collapse the target to 256.

`doctor` and `/status.file_descriptors` show the soft limit, the reserve, the
requested value and the effective value. A reduction below 2048 logs a warning
and suggests raising the limit; e-lite never raises it for you.

```bash
# systemd unit or shell
ulimit -n 16384
```

A low limit is not fatal: RocksDB reopens SSTs, which only makes prevout reads
slower. A limit so low that no descriptor is left for RocksDB (< 20) is a clear
startup error instead of an `EMFILE` later.

## Core parameters chosen by e-lite

When e-lite starts Core (`manage = "auto"` with no running node, or
`"managed"`), it adds a fitted plan:

| Argument | Rule |
|---|---|
| `-dbcache` | `min(total/8, available/2, 4096)` MiB, at most `RAM/3` |
| `-par` | physical cores - 1, clamped to 1..8 (leaves a core for the index) |
| `-rpcthreads` | `clamp(physical cores, 4, 16)` |
| `-listenonion` | `0`: e-lite publishes its own Electrum onion and does not create a Tor control port |
| `-prune` | always explicit, from `[core] prune_mib` (including `-prune=0`) |

`prune_mib = 0` used to omit the flag, so a `prune=` line in an existing
`bitcoin.conf` could silently prune a "full" node. It is now always stated.

Core's own onion is disabled by default because the managed `torrc` has only a
`SocksPort` and hidden-service ports, no `ControlPort`. With Core's default
`-listenonion=1 -torcontrol=127.0.0.1:9051`, Core would retry that port forever
and log `Error connecting to address 127.0.0.1:9051`. An operator who really
wants Core's onion can set `-listenonion=1` (and a working `-torcontrol`) in
`extra_args`; e-lite then leaves the flag alone and `doctor` warns that the
managed Tor has no control port. An external Core is never reconfigured, so
disable its onion in `bitcoin.conf` if the log noise is unwanted.

An operator value in `extra_args` wins for `-dbcache`, `-par`, `-rpcthreads` and
`-listenonion`. Arguments that e-lite owns (`-datadir`, `-chain`, `-server`,
`-daemon`, `-daemonwait`, `-rpcbind`, `-rpcport`, `-rpcallowip`, `-prune`) are
rejected with an explanation. An external Core is never configured or
restarted.

Core's shutdown grace is now two minutes (it was 30 s) so a multi-GiB UTXO
cache can flush instead of being killed mid-flush.

## One intensive sync at a time

Each user gets one intensive synchronisation slot (a `flock` under
`$XDG_RUNTIME_DIR/e-lite/ibd.lock`, or `/tmp/e-lite-<uid>/ibd.lock`). A second
instance **waits** and logs who holds the slot; it keeps serving its existing
index and starts automatically when the slot is released. Acquiring happens
before a from-zero managed Core starts downloading, and the slot is released as
soon as the instance is within 1000 blocks of Core. The kernel drops the lock on
crash, so a dead process cannot block the queue. `E_LITE_NO_SYNC_LOCK=1`
disables it (not recommended).

This is a per-user lock. Separate containers do not see each other's locks.

## End-to-end IBD shape

The pipeline is still Core → read → decode/prevout → apply → commit → prune,
and the controller treats it as one system:

- `commit/blk` is now measured from the blocks actually committed. The old
  figure divided by the configured batch, so a 100-block group committing in
  8 s was reported as 7 ms/block instead of 80 ms/block.
- Undo is generated for **every** applied block and pruned at commit time from
  the tip that is really committed. A time- or transaction-limited commit can
  no longer leave a tip without its reorg window.
- The prepared-block window is bounded by `prefetch_blocks` when set (0 keeps
  the automatic `workers × 2`), instead of a value that was printed but unused.
- `/status.stage_totals_us` and `commit_breakdown_us` separate read, decode,
  prefetch, wait, apply, fsync of `txids.dat` and the RocksDB write.
- The supervisor now acts at commit boundaries. A catch-up call is no longer
  cut every 10 s, so readers and prepared prevouts survive; the only remaining
  teardown is the 512-block checkpoint. `/status.pipeline` counts prepared,
  applied and discarded blocks (discarded should be roughly the in-flight
  window of one checkpoint, not one window per 10 s).
- The lag window no longer rewrites its baseline on every resume. The old code
  did `baseline = lag` when Core resumed, which raised the next pause threshold
  by `max_bulk_lag_blocks/2` per cycle (14,884 → 24,858 → 34,858 in a real
  29.5.0 log). The default is now an absolute byte window
  (`max_bulk_lag_gib`/`core_resume_ahead_gib`); the legacy block window is used
  only when the GiB value is 0. `E_LITE_LAG_POLICY=drain` opts into a
  time-based watermark: pause when the index would need more than
  `E_LITE_LAG_TARGET_SECONDS` (default 1800) to drain the backlog at the last
  commit cycle's rate, resume at half. It is opt-in until a mainnet A/B.
- RocksDB properties are classified correctly: `estimate-pending-compaction-
  bytes`, `cur-size-all-mem-tables`, `total-sst-files-size` and
  `estimate-table-readers-mem` are per-CF and are summed; `num-running-
  compactions` and `block-cache-usage` are DB-wide and are read once.
  `/status.rocksdb.properties_ok` distinguishes an unknown value from zero.
  `/status.rocksdb.cf` shows the same breakdown per family. Bulk commits also
bound raw block bytes, so a group of large-but-transaction-light blocks cannot
inflate the `WriteBatch`.
- Memtables are weighted by write pressure: `utxo` 6, `history` and
  `utxo_by_script` 4, `blocks`/`tx_occurrences` 2, `meta`/`undo` 1. The shared
  `WriteBufferManager` still enforces the total. By default the budget is fixed
  at startup; under critical memory (`MemAvailable < 256 MiB`) it is halved as
  a one-way safety valve, which can force a flush and is logged. With
  `[index] adaptive_memtables = true` the controller re-fits the WBM and every
  CF's `write_buffer_size` to `effective/16` (512 MiB..2 GiB) after a
  10-minute persistent target and a 10-minute cooldown; cache changes have
  priority and an explicit `write_buffer_mib` disables the feature.
- `[index] prefetch_deadline_ms` (50..60000, 0 = off) splits prevout prefetch
  into 512-key chunks and stops after the deadline; apply falls back to point
  lookups. It trades a tail reader for more apply work.
- `[core] io_priority = 0..7` applies best-effort `ionice` to a Core launched
  or adopted by e-lite. `[core] cgroup_io_weight = 1..10000` additionally tries
  to place Core in `e-lite-core` under the current cgroup v2 and set
  `io.weight`; it requires a delegated `io` controller. Both are warnings, not
  fatal, when the kernel/filesystem does not support them (NVMe `none`).
- Prevout lookup uses `batched_multi_get_cf` over deduplicated, sorted keys.
- **Commits in bulk are deliberately infrequent**: transactions (250k), blocks
  (512), raw bytes (`clamp(RAM/128, 64-256 MiB)`) or time (20 s NVMe, 30 s
  SATA/virtual, 45 s HDD, 60 s network). A time trigger with fewer than 8
  blocks waits until 60 s, so tiny commits disappear. Live mode switches to
  1.5 s. This is safe because the prune gate never prunes above the committed
  tip; a crash loses at most the current group and the blocks are still in
  Core.
- **Prune is asynchronous and deferred**: the WAL barrier runs at the commit
  boundary, then the request waits for the top of the next apply iteration
  (never in the middle of an uncommitted group). `el-prune` calls
  `pruneblockchain` with a 120 s timeout; the index keeps applying while Core
  deletes files. The supervisor collects the result at the next commit/tick.
- **RPC retry**: a socket timeout/EAGAIN/reset is retried once after 150 ms;
  HTTP and RPC errors are not. This prevents a busy Core from tearing down the
  pipeline.

`commit_sync` and the WAL ordering are unchanged: `txids.dat` is fsynced before
the tip advances. Before Core deletes blocks, e-lite now calls `SyncWAL(true)`
instead of forcing every memtable to SST. A single `WriteBatch` is atomic across
column families and the WAL is enabled, so the committed tip is recoverable;
the flush was a large stall on every prune and is not needed for durability.
`tests/e2e_pruned_ibd.sh` SIGKILLs e-lite after a prune and verifies the index
recovers with the full history.

## Containers (LXC/Docker)

Memory is resolved from the process's real position in the cgroup tree:

1. `/proc/self/cgroup` gives the path (v2 `0::<path>`, v1 `:memory:<path>`);
2. `/proc/self/mountinfo` gives the cgroup2/cgroup mount point and its root;
3. every directory from the leaf up to the mount root is read
   (`memory.max`, `memory.current`, `memory.stat`; v1 uses the `memory/*`
   equivalents).

The effective limit is the smallest finite limit on that path, and the
available memory pairs **each limit with the usage of that same cgroup** — a
parent's limit is never combined with a child's usage. `memory.max = max` and
the v1 `2^60` sentinel mean "unlimited"; a missing controller means "host RAM".
`doctor` distinguishes `detected (limited)`, `detected (unlimited)` and
`not detected`, instead of printing `0` for all three.

Practical notes:

- The minimum-RAM check uses this effective total, so a container with
  `--memory=2g` is rejected even on a 64 GiB host.
- Docker defaults `nofile` to 1024 in most setups; that caps `max_open_files`
  at `1024 - reserve`. Raise it (`--ulimit nofile=16384:16384`) for the best
  prevout read speed. e-lite logs the effective value and warns.
- If the kernel kills the process (`Killed` in the shell), check
  `dmesg -T | tail` or `journalctl -k`. Lower `--cache-mib`,
  `--write-buffer-mib` or `--memory-fraction`, not just the cache.
- Managed Tor runs inside the same container and shares the budget.

### UmbrelOS

The Umbrel package is not built from this repository, so its Compose file is not
verified here. When packaging e-lite for UmbrelOS/Docker, keep in mind:

- a container memory limit is what e-lite plans against (good, but it must be
  at least the 4/8 GB class for the selected network);
- Bitcoin Core usually runs in another container: use `manage = "external"` and
  an `rpc_url`/cookie that reaches it; e-lite will not configure or restart it;
- `/proc` and cgroup mounts must be visible for resource detection; with a
  restricted namespace the check falls back to host RAM, which is wrong inside
  a small container — keep the cgroup filesystem mounted or set
  `memory_reserve_mib`/`memory_fraction` explicitly;
- index, Core blocks and the rawtx cache must be on volumes, not the writable
  overlay layer, and the Core datadir should not be nested inside e-lite's
  data directory (it is excluded from the reported index size when it is);
- allow enough time for SIGTERM: Core can need up to two minutes to flush;
- the per-user sync lock does not coordinate separate containers.

## Long catch-up and Core separation

With `prune_mib = 1` (manual pruning), Core can download faster than e-lite
indexes. By default e-lite pauses Core during the long bulk catch-up and resumes
it at `core_resume_ahead_gib` (7 GiB) of unindexed backlog, so peers reconnect
while e-lite finishes. After that, `max_bulk_lag_gib` (30 GiB) pauses Core
again.

### Backlog in bytes, not blocks

A 20,000-block lag is ~10 GiB at height 100k and ~30 GiB at height 800k, so the
watermark is `lag_blocks × avg_block_bytes`. The average is an EWMA of the raw
serialized blocks applied (`/status.scheduler.backlog_bytes`). To keep the old
block window, set `max_bulk_lag_gib = 0`; then
`max_bulk_lag_blocks`/`core_resume_ahead_blocks` apply with their historical
growth-relative semantics.

### `scheduler = "overlap" | "alternate"`

- `overlap` (default): Core downloads while e-lite keeps indexing. This is the
  historical behaviour and usually wins because the index still advances during
  the Core phase.
- `alternate` (opt-in): while Core downloads, e-lite stops indexing (serving
  continues); when the backlog reaches `max_bulk_lag_gib`, Core stops and
  e-lite drains it to `core_resume_ahead_gib`, then the cycle repeats. This
  removes I/O contention at the cost of the overlap. It requires
  `prune_mib = 1` and `pause_core_during_bulk = true`.

Strict alternation only wins if Core alone is faster than roughly

```text
v_core_solo > v_overlap / (1 - v_overlap / v_index_solo)
```

With the measured 29.7.1 mainnet rates (1.36 blocks/s overlapping, 5.6–6
blocks/s index-only), the break-even is ~6.7 blocks/s. Measure Core alone with a
5-minute `SIGSTOP` of e-lite before changing the default:

```bash
PID=$(pgrep -f 'e-lite run' | head -1)
bitcoin-cli -datadir=<core-datadir> getblockcount
kill -STOP "$PID"; sleep 300
bitcoin-cli -datadir=<core-datadir> getblockcount
kill -CONT "$PID"
```

The blocking `setnetworkactive` switch and peer reconnect make short phases
expensive; use 40–60 GiB phases if you test `alternate`, not 7–30 GiB. The
`SIGSTOP` test also pauses Electrum/Tor for those 5 minutes; that is acceptable
for a one-off measurement but not during wallet use.

### Commit controller

The commit controller now uses the last **full** commit group, with its own
cycle (`commit_last_cycle_ms`) and cost split (`txids` vs RocksDB). Boundary
commits (the 512-block checkpoint, shutdown) and tx-limited groups are
deliberately excluded:

- a `Txs`-limited group means the batch is not the binding limit, so the batch
  is left alone;
- an I/O-bound group (`db_us > 2 × txids_us`) is not shrunk: smaller groups
  only add commits without reducing per-block compaction;
- a high commit share only shrinks the batch when the block limit was the
  reason (`reason = Blocks`).

Readers also need two consecutive samples and pipeline starvation (`wait_ms`)
before more concurrency is added; a high `read_ms` alone no longer reduces the
pool, because it can be the block source rather than the pool. Reader changes
never go below `min_readers` (half the physical cores, at least 2).

### Compaction debt, L0 files and `background_jobs`

The 10-minute `rocksdb:` log line and `/status.rocksdb` sample the counters
needed to attribute slow commits:

| Signal | Meaning |
|---|---|
| `pending_compaction` | `estimate-pending-compaction-bytes` summed over every family. It is an **estimate** that inflates while ingest continues; use its *trend*, not its absolute value |
| `l0` | summed level-0 file count. A growing `l0` with `running 1` is the real flush/compaction stall signal |
| `running` | compactions in flight. `max_background_jobs` splits the pool between flushes and compactions, so the default plan (`physical/2`, 2..4) gives one of each on a 4-core host |
| `block_cache` | real bytes charged to the shared cache, read once (with a shared cache every family reports the whole-cache usage, so it is not summed); expected ≈ the configured ceiling in the `memory:` line |
| `memtable` / `table_readers` | actual memtable bytes and index/filter reader memory |

A full commit group that reaches 10 s and either costs twice the configured
bulk time limit or 5x the previous per-block cost prints a
`[e-lite] commit spike:` line with that context plus the Core backlog, so an
outlier can be attributed after the fact. Normal full-interval bulk groups do
not warn.

RocksDB falls back to slow/stop writes when L0 accumulates faster than a
single compaction thread can drain it; that shows up as long commits with
`running 1` and rising `l0`. Since 0.1.0-alpha.29.8.8 the plan gives SSD/NVMe
disks `background_jobs = physical cores` clamped to 2..8 (two flush plus half
the physical cores as compaction threads on a 4-core host; HDD keeps 2 and
virtual/unknown keeps `physical/2`), because the old `physical/2` default let
the debt grow ~70 GiB/h at drain rates. `[index] background_jobs` still
overrides the plan for A/B; compare the `pending_compaction` trend and
commit/blk in the same phase before changing it back.

### Measured on a real mainnet IBD with defaults (2026-11-08)

At ~640k height on a 32 GiB machine, default settings:

| Signal | Observation |
|---|---|
| Throughput | 4.2–4.8 blocks/s, ~9.5–10k tx/s |
| Per-block cost | prevout prefetch **100–1500 ms** (dominant), decode ~25 ms, apply 20–100 ms, commit ~7–8 ms amortised, read 1–10 ms |
| Commit group | adaptive interval escalated to 30 s; commit ~1.4–8.4 s per group |
| Block cache | grew to 6.6 GiB, then collapsed to 256 MiB; page cache ~150 MiB |

Conclusions applied as defaults in 0.1.0-alpha.29.4.4:

- The cache is capped at `total/4` (profile) and by the effective room, with a
  floor of `clamp(effective/8, 512 MiB, 2048 MiB)`; shrinking uses
  `MemAvailable` and the OS reserve, not a low page-cache number.
- Index and bloom filters stay out of the data block cache and
  `max_open_files = 8192`, so prefetch lookups do not pay an extra disk read
  when the data cache shrinks.
- `commit` is not the dominant per-block cost; prefetch is. Measure the
  `commit_breakdown_us` split before changing fsync policy.
- The 30 s adaptive commit interval is still under evaluation; it improved
  neither block rate nor lag in this run.

### Measured on the test LXC (4 vCPU, 8 GiB, 2026-11-07)

`tests/bench_index.sh` mines a disposable regtest chain and indexes it with
several variants. Indexing is measured after Core already has the blocks, so it
isolates e-lite from download speed.

| Chain | Variant | Time | Blocks/s | Peak RSS | Index |
|---|---|---:|---:|---:|---:|
| 5,000 light blocks (5.7k txs) | default (256) | 0.306 s | 16,340 | 22 MiB | 3.7 MB |
| | `readers=2` | 0.272 s | 18,382 | 22 MiB | 3.7 MB |
| | `readers=4` | 0.297 s | 16,835 | 22 MiB | 3.7 MB |
| | `readers=8` | 0.280 s | 17,857 | 22 MiB | 3.7 MB |
| | `batch=64` | 0.481 s | 10,395 | 21 MiB | 3.7 MB |
| | `batch=1024` | 0.283 s | 17,668 | 23 MiB | 3.7 MB |
| 400 blocks × 100 outputs (511 indexed incl. premine) | default (256) | 0.485 s | 1,054 | 71 MiB | 21.8 MB |
| | `batch=1024` | 0.484 s | 1,056 | 67 MiB | 20.9 MB |

The first table was measured on 0.1.0-alpha.29.4.0 with the old block count
(it excluded the 110 premine blocks). The second table is the same workload on
0.1.0-alpha.29.4.2 with the corrected count and the commit-group undo coverage:
wall time is unchanged (0.485 s vs 0.490 s) and the index is ~4 % larger because
the last reorg window is always retained. That is the accepted cost of a
committed tip that can always be rewound.

Conclusions kept: `batch_blocks = 256` is the best default (`64` is ~30-50 %
slower on output-heavy blocks; `1024` is not faster and holds more work).
`readers` 2/4/8 are within noise on 4 vCPUs, so the adaptive default is kept.
The bottleneck on this host was the wallet used to mine dense blocks (UTXO
scan), not e-lite; the synthetic chain is much smaller than mainnet, so these
numbers compare variants, not real chain throughput.

Normal pruning runs every `min_gain_blocks` safe blocks (default **5,000**) and
keeps a 20 GiB free-space margin by default (`prune_defer_free_gib = 20`). At
an average mainnet block size of ~1.5 MiB, 5,000 blocks is roughly 7.5 GiB of
retained tail before Core rounds the cut to whole block files. Increase the
gain only if prune I/O is a measured problem. Low-disk emergency pruning still
runs. `--core-arg` can also reduce Core's memory/IO footprint (for example
`--core-arg=-dbcache=128`).

**The free-space deferral ends at the tip.** While Core downloads (IBD),
plenty of free space defers normal pruning — it would be churn. Once Core is
out of IBD **and** the index matches Core's tip, the deferral is bypassed:
the gate prunes to the safe maximum (`index - 288`) on the next tick,
releasing every already-indexed block a from-zero IBD leaves behind (~90k
blocks / ~140 GiB on mainnet). Live mode then batches again under
`min_gain_blocks`. Durability is unchanged: the prune still starts from a
durable tip behind the WAL barrier and retries the same safe height after a
failure.

`commit_sync = auto` (default) uses `txids` durability during bulk/IBD and
strict `always` when live. `txids` still fsyncs `txids.dat`; a crash can only
lose the last interval, which is reindexed from Core.

## Application log

`run`, `index` and `serve` tee stdout/stderr to `<data-dir>/e-lite.log` (same
lines as the console). Use `--log-file`, `--log-max-mib` or `--no-log-file` to
override; rotation keeps one `e-lite.log.1`.

## Safety and correctness

Tuning never changes block order, commit atomicity, the undo window, pruning
decisions or the canonical digest. Only the number of fetch workers, group
boundaries, cache capacity and mutable RocksDB options change. Digest parity
across group sizes and profiles is part of the test suite.
