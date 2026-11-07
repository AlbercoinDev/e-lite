# Autotuning and hot reload

e-lite keeps a small set of runtime knobs so the same binary can run on a slow
rotational disk and on a fast NVMe drive. The controller is deliberately
conservative: it changes one thing at a time, has a cooldown, and never
overrides the disk/prune safety guards.

All settings work on every network. See [`CLI.md`](CLI.md) for the full list of
command-line overrides and `--set section.key=value`.

## Startup profile

`[index] profile` selects the initial values:

| Profile | readers | commit | batch | cache | jobs |
|---|---:|---:|---:|---:|---:|
| `auto` | detect | profile | profile | profile | CPU-capped |
| `hdd` | 2 | 5 s | 512 | RAM/4, max 4 GiB | 4 (capped by CPUs) |
| `ssd` | 8 | 2 s | 256 | RAM/4, max 8 GiB | 8 (capped by CPUs) |
| `nvme` | 16 | 2 s | 256 | RAM/3, max 16 GiB | 12 (capped by CPUs) |
| `manual` | config | config | config | config | config |

`auto` uses the Linux `rotational` hint for the data directory. Virtual
machines, RAID and network filesystems can report this incorrectly; the hint is
only the starting point, not a hard classification. Use `--profile` (or
`--set index.profile=...`) when you know the real hardware.

RocksDB memory is budgeted globally, not per column family. The default memtable
budget is `RAM/8` (256 MiB..2 GiB), split across the 7 CFs. Background jobs are
capped by the CPU count so a 2-core machine does not run 8 compaction threads.

## Memory budget

e-lite reserves RAM for the OS, Core and the page cache, and uses the rest as a
ceiling for its own cache/memtables/runtime:

```
reserve   = max(1024 MiB, RAM_total/8)
ceiling   = 75 % of RAM_total
memtables = RAM_total/8          (fixed at startup)
runtime   = RAM_total/16         (batch, overlay, prefetch, Tor)
cache_max = ceiling - memtables - runtime
```

The block cache starts at the profile value (`RAM/4`) and is adjusted at most
once every 30 seconds:

- **grow +256 MiB** while `MemAvailable >= 1.5 × reserve`, page cache
  (`Buffers + Cached`) is at least `reserve`, and the cache is below `cache_max`;
- **shrink 25 %** (floor 256 MiB) when `MemAvailable < reserve` or page cache is
  below `reserve/2`;
- two consecutive samples are required in the same direction (hysteresis).

Growing the RocksDB cache consumes page cache, which is why the page-cache floor
exists: it keeps block-file reads fast. If `read` latency rises while the cache
grows, the policy backs off.

`cache_mib` is a **maximum**, not a fixed value: the controller may use less
under pressure and grow back up to it. A critical memory situation can shrink
even a manual value. `write_buffer_mib` is fixed when the store opens.

Before Core resumes downloads after a bulk pause, e-lite shrinks the cache to
`max(1024 MiB, RAM/4)` so Core has room for its block index and verification.

Explicit values cap the automatic policy:

```toml
[index]
profile = "auto"
readers = 4              # explicit override
cache_mib = 4096         # maximum cache, not a fixed value
write_buffer_mib = 1024  # fixed memtable budget
memory_reserve_mib = 0   # 0 = max(1024, RAM/8)
memory_fraction = 0      # 0 = 0.75 of total RAM
commit_interval_ms = 5000
adaptive = true
```

Set `adaptive = false` to keep the profile/config values fixed. Explicit
`readers`/`commit_interval_ms` remain locked; `cache_mib` is a ceiling that the
memory policy can lower for safety.

The same values can be passed without a config file:

```bash
e-lite run --network mainnet --allow-mainnet --profile nvme
e-lite run --network signet --profile hdd --readers 4 --cache-mib 1024
e-lite run --set index.profile=ssd --set index.commit_interval_ms=3000
```

## What the controller observes

The indexer records EWMA values for:

- block-read latency (`read_ms`),
- block decode/hash latency (`decode_ms`),
- prevout prefetch latency (`prefetch_ms`),
- pipeline wait (`wait_ms`),
- `apply_block` latency (`apply_ms`),
- commit + fsync latency (`commit_ms`) and `commit/blk`,
- indexed blocks and transactions per second.

They are exported in `/status` under `tuning` and in `/metrics` as
`e_lite_tuning_*`, `e_lite_tx_per_sec`, `e_lite_blocks_per_sec` and
`e_lite_commit_ms`. `prefetch_ms` is shown in the progress line only.

On HDDs and small hosts, prevout lookups were the largest part of `apply`:
RocksDB point reads for one block ran serially in the apply thread. Since
alpha.23 the reader threads resolve a block's prevouts speculatively before it
reaches apply, so random UTXO reads overlap with the previous block's work. The
result is a superset (group-local creations/deletions are ignored); apply uses
the group overlay first and falls back to a point lookup, so the pipeline never
depends on the speculation. If `prefetch` is high and `apply` is low, the disk
is the limit; if `prefetch` is low and `apply` is high, the remaining cost is
CPU/HashMap/history work.

## What it changes

Only these hot fields are adjusted automatically:

- `readers`: less on a rotational disk when read latency is high; more only on
  non-rotational disks when reads are actually slow, never just because a read
  was fast. Automatic readers are capped at 4 (HDD), 8 (SSD) or 16 (NVMe), by
  available memory and by `2 × CPU count` (explicit `--readers` wins).
- `commit_interval_ms` and `batch_blocks`: larger groups when commit/fsync is
  slow and commit is less than half of the interval; if commit dominates the
  interval (>75%), the batch shrinks instead. The bulk maximum is 30 s / 4096
  blocks, with the usual crash-window tradeoff.
- `cache_mib`: grows/shrinks inside the memory budget described above. The
  cache-hit metric was removed because RocksDB did not expose it reliably;
  `--cache-mib` still sets the maximum.

The controller runs at most once every 30 seconds. It is inhibited while the
disk guard is active, during a prune-gap repair, during a reorg and during
shutdown. The memory policy uses `MemAvailable` (including the cgroup v2/v1
limit in LXC/containers) and `Buffers + Cached`; two consecutive samples are
required before a change. `memory.current` includes reclaimable page cache, so
`inactive_file` is subtracted before declaring pressure; this avoids the false
`0 MiB available` state that used to shrink the cache on machines with plenty of
RAM.

Every change is logged:

```text
[e-lite] adaptive tuning: commit 620ms: interval -> 7500ms, batch -> 768
[e-lite] memory: available 5317 MiB, buff/cache 4443 MiB, reserve 1024 MiB: cache 1977 -> 2233 MiB (max 4449, memtables 988 MiB, readers 8)
```

## Hot reload

Send `SIGUSR1` to reload the same config file without restarting:

```bash
kill -USR1 "$(pgrep -f 'e-lite run')"
```

Hot fields:

- `index.readers`
- `index.cache_mib`
- `index.commit_interval_ms`
- `index.batch_blocks`
- `index.profile`
- `index.adaptive`
- pruning thresholds (`min_free_gib`, `critical_free_gib`,
  `prune_interval_seconds`, `min_gain_blocks`)

Cold fields are reported as requiring a restart:

- `core.prune_mib`
- `index.block_source`
- `index.reorg_window`
- `index.write_buffer_mib`
- `index.milestone_blocks` / `index.milestone_seconds`
- `pruning.max_core_lag_blocks` / `pruning.max_bulk_lag_blocks`
- `network.data_dir`
- server listeners/TLS paths
- network/identity and schema

If the file is invalid, the previous configuration stays in effect. Changes
made through CLI flags are not replayed on reload; put persistent values in the
TOML file.

## Recommended starting points

- **HDD / SMR**: `profile = "hdd"`, `adaptive = true`, `cache_mib` at least
  2048–4096 if RAM allows. Fewer readers and larger commit groups usually beat
  parallelism on a single spindle.
- **SATA SSD**: `profile = "ssd"`, `adaptive = true`.
- **NVMe**: `profile = "nvme"`, `adaptive = true`; if the apply thread is the
  bottleneck, more readers will not help until decode/prefetch is parallelized.
- **Benchmark/fixed**: `profile = "manual"`, `adaptive = false`, explicit
  `readers`/`commit_interval_ms`/`cache_mib`.

## Containers (LXC/Docker)

Memory accounting uses the lower of `/proc/meminfo` and the cgroup limit
(`/sys/fs/cgroup/memory.max` or cgroup v1 `memory.limit_in_bytes`). If a
container still reports the host's memory, the controller also checks
`memory.current` minus `inactive_file` and backs off readers/cache when truly
available memory is low. `e-lite doctor` and `/status` show total, available,
cgroup limit/usage and reclaimable cache.

If the process is killed by the kernel (`Killed` in the shell), check
`dmesg -T | tail` or `journalctl -k` for the OOM report. Reduce the container's
`--cache-mib`/`--write-buffer-mib`/`--readers` or set `--no-adaptive` while
investigating.

## Long catch-up and Core separation

With `prune_mib = 1` (manual pruning), Core can download faster than e-lite
indexes. By default e-lite pauses Core during the long bulk catch-up and resumes
it `core_resume_ahead_blocks` (5,000) before the tip, so peers reconnect while
e-lite finishes. After that, `max_bulk_lag_blocks` (20,000) bounds additional
growth relative to the lowest lag seen and resumes at half.

Normal pruning runs every `min_gain_blocks` safe blocks (default **10,000**) and
is not deferred by free space by default (`prune_defer_free_gib = 0`).
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
