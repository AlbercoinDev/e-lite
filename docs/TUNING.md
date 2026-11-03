# Autotuning and hot reload

e-lite keeps a small set of runtime knobs so the same binary can run on a slow
rotational disk and on a fast NVMe drive. The controller is deliberately
conservative: it changes one thing at a time, has a cooldown, and never
overrides the disk/prune safety guards.

## Startup profile

`[index] profile` selects the initial values:

| Profile | readers | commit | batch | cache |
|---|---:|---:|---:|---:|
| `auto` | detect | profile | profile | profile |
| `hdd` | 2 | 5 s | 512 | RAM/4, max 4 GiB |
| `ssd` | 8 | 2 s | 256 | RAM/4, max 8 GiB |
| `nvme` | 16 | 2 s | 256 | RAM/3, max 16 GiB |
| `manual` | config | config | config | config |

`auto` uses the Linux `rotational` hint for the data directory. Virtual
machines, RAID and network filesystems can report this incorrectly; the hint is
only the starting point, not a hard classification.

Explicit values win over the profile:

```toml
[index]
profile = "auto"
readers = 4              # explicit override
cache_mib = 4096
commit_interval_ms = 5000
adaptive = true
```

Set `adaptive = false` to keep the profile/config values fixed. Explicit
`readers`/`cache_mib`/`commit_interval_ms` are treated as locked: the controller
will not change them, except a critical memory floor (<256 MiB) which reduces
workers for safety.

## What the controller observes

The indexer records EWMA values for:

- block-read latency (`read_ms`),
- `apply_block` latency (`apply_ms`),
- commit + fsync latency (`commit_ms`),
- indexed transactions per second,
- RocksDB block-cache hit percentage.

They are exported in `/status` under `tuning` and in `/metrics` as
`e_lite_tuning_*`, `e_lite_tx_per_sec` and `e_lite_commit_ms`.

## What it changes

Only these hot fields are adjusted automatically:

- `readers`: less on a rotational disk when read latency is high; more only on
  non-rotational disks when reads are actually slow, never just because a read
  was fast. Automatic readers are capped at 4 (HDD), 8 (SSD) or 16 (NVMe) and
  by available memory.
- `commit_interval_ms` and `batch_blocks`: larger groups when commit/fsync is
  slow.
- `cache_mib`: grows when the cache hit rate is low and RAM is available;
  shrinks when `MemAvailable` is low.

The controller runs at most once every 30 seconds. It is inhibited while the
disk guard is active, during a prune-gap repair, during a reorg and during
shutdown. If `MemAvailable` (including the cgroup v2/v1 limit in LXC/containers)
falls below 512 MiB, it reduces readers/cache before doing anything else.

Every change is logged:

```text
[e-lite] adaptive tuning: commit 620ms: interval -> 7500ms, batch -> 768
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
- `network.data_dir`
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
`memory.current` and backs off readers/cache when free memory is low.

If the process is killed by the kernel (`Killed` in the shell), check
`dmesg -T | tail` or `journalctl -k` for the OOM report. Reduce the container's
`cache_mib`/`readers` or set `adaptive = false` while investigating.

## Safety and correctness

Tuning never changes block order, commit atomicity, the undo window, pruning
decisions or the canonical digest. Only the number of fetch workers, group
boundaries, cache capacity and mutable RocksDB options change. Digest parity
across group sizes and profiles is part of the test suite.
