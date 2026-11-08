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

## Memory budget

One accounting, three consumers. e-lite never plans more than its ceiling:

```
reserve   = memory_reserve_mib, else clamp(RAM/8, 512 MiB, 2048 MiB)
                        (RAM/4 for machines of 2 GiB or less)
usable    = RAM_total - reserve
ceiling   = min(RAM_total * memory_fraction, usable)   # 0 -> 0.75, never raised
runtime   = clamp(RAM_total/32, 96 MiB, 1024 MiB)      # block buffers, overlay,
                                                        # mempool, queries, Tor
memtables = write_buffer_mib, else clamp(RAM_total/8, 128 MiB, 2048 MiB)
            shrunk so that memtables + runtime + 64 MiB <= ceiling
cache_max = ceiling - runtime - memtables
            (also capped at max(RAM/4, 1024 MiB) when automatic)
cache_start = cache_max for an explicit cache_mib, else min(RAM/4, cache_max)
```

`planned = runtime + memtables + cache_start` is always `<= ceiling`. An
explicit `memory_fraction` is a hard request: if the components do not fit, the
memtables and the cache are reduced instead of raising the ceiling. The old
formula could plan 1738 MiB inside a 1581 MiB ceiling (`RAM=7907`,
`memory_fraction=0.20`); that case is now a regression test.

Two different budgets are reported and must not be confused:

* **planned** — what e-lite intends to keep resident;
* **observed** — `/status.rocksdb` (`memtable_bytes`, `block_cache_bytes`,
  `table_readers_bytes`) and the process RSS.

Indexes and bloom filters stay outside the data block cache for read speed, so
their memory (`rocksdb.estimate-table-readers-mem`) is reported separately. The
plan reserves runtime for them; if they grow past it, raise the reserve or lower
`max_open_files`.

`cache_mib` is a **maximum**, not a fixed value. The controller may use less
under pressure and grow back up to it. `write_buffer_mib` is fixed when the
store opens (changing it requires a restart).

The block cache is adjusted at most once every 30 seconds:

- **grow +256 MiB** while `MemAvailable >= 1.5 × reserve` and below `cache_max`;
- **shrink 25 %** when `MemAvailable < reserve`, with a soft floor of
  `clamp(RAM/8, 512 MiB, 4096 MiB)`;
- below 256 MiB available the floor no longer applies, readers are reduced, and
  the cache may fall to 8 MiB — a low-memory emergency must be able to return
  memory;
- two consecutive samples in the same direction are required (hysteresis).

Cache decisions use `MemAvailable` only. A low page-cache number is expected
while the RocksDB cache holds the UTXO working set; shrinking because of it was
what produced the 6.6 GiB → 256 MiB oscillation.

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
| `-dbcache` | `clamp(RAM/4, 450, 2048)` MiB, at most `RAM/3` |
| `-par` | `cpus - 1`, clamped to 1..8 (leaves a core for the index) |
| `-rpcthreads` | `clamp(cpus, 4, 16)` |
| `-prune` | always explicit, from `[core] prune_mib` (including `-prune=0`) |

`prune_mib = 0` used to omit the flag, so a `prune=` line in an existing
`bitcoin.conf` could silently prune a "full" node. It is now always stated.

An operator value in `extra_args` wins for `-dbcache`, `-par` and
`-rpcthreads`. Arguments that e-lite owns (`-datadir`, `-chain`, `-server`,
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

`commit_sync`, the WAL and the prune ordering are unchanged: `txids.dat` is
fsynced before the tip advances and the index is flushed before Core is allowed
to delete blocks.

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
it `core_resume_ahead_blocks` (5,000) before the tip, so peers reconnect while
e-lite finishes. After that, `max_bulk_lag_blocks` (20,000) bounds additional
growth relative to the lowest lag seen and resumes at half.

### Measured on a real mainnet IBD with defaults (2026-11-08)

At ~640k height on a 32 GiB machine, default settings:

| Signal | Observation |
|---|---|
| Throughput | 4.2–4.8 blocks/s, ~9.5–10k tx/s |
| Per-block cost | prevout prefetch **100–1500 ms** (dominant), decode ~25 ms, apply 20–100 ms, commit ~7–8 ms amortised, read 1–10 ms |
| Commit group | adaptive interval escalated to 30 s; commit ~1.4–8.4 s per group |
| Block cache | grew to 6.6 GiB, then collapsed to 256 MiB; page cache ~150 MiB |

Conclusions applied as defaults in 0.1.0-alpha.29.4.4:

- The cache is capped at `RAM/4` with a floor of
  `clamp(RAM/8, 512 MiB, 4096 MiB)`; shrinking uses `MemAvailable`, not a low
  page-cache number.
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

Normal pruning runs every `min_gain_blocks` safe blocks (default **1,000**) and
is not deferred by free space by default (`prune_defer_free_gib = 0`). At an
average mainnet block size of ~1.5 MiB, 1,000 blocks is roughly 1.5 GiB of
retained tail before Core rounds the cut to whole block files; 10,000 blocks was
roughly 15 GiB. Increase the gain only if prune I/O is a measured problem.
Low-disk emergency pruning still runs. `--core-arg` can also reduce Core's
memory/IO footprint (for example `--core-arg=-dbcache=128`).

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
