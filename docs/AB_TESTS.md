# A/B protocol (mainnet and other test machines)

These are the measurements that decide the remaining tuning defaults. e-lite
does not change them automatically; run one variant at a time on any test
machine and keep the previous CSV. On a closed box (Umbrel, appliance) the
protocol is the same: run the variant, then pass the log or CSV to the
analysis. `tests/ab_umbrel.sh` is the generic sampler despite its historical
name.

## 0. Baseline

```toml
[index]
readers = 4          # or 0/auto
cache_mib = 0
adaptive_memtables = false
prefetch_deadline_ms = 0

[pruning]
scheduler = "overlap"
max_bulk_lag_gib = 30
core_resume_ahead_gib = 7
```

Start e-lite, wait for a normal Core/e-lite phase, then sample for 30-60 min:

```bash
tests/ab_umbrel.sh baseline 1800
```

`E_LITE_STATUS_URL` defaults to `http://127.0.0.1:55084/status` (mainnet
metrics); set it if your instance maps the port differently. The CSV is written
to `/tmp/e-lite-ab-baseline.csv`.

## 1. Readers

```toml
[index]
readers = 6
```

Restart, run `tests/ab_umbrel.sh readers6 1800`, compare:
`phase=core` (Core downloading) and `phase=draining` (Core paused).
Keep 6 only if the active-phase rate improves and the drain rate does not fall.

For an 8 GiB box, skip readers=6 unless `wait_p95` was high; 4 is the safer
default.

## 2. Cache

Only if the baseline log shows sustained `MemAvailable` > 6 GiB and
`prefetch_p95` high:

```toml
[index]
cache_mib = 6144     # 8 GiB boxes: 4096
```

Run `tests/ab_umbrel.sh cache6g 1800`. Watch `memory.available_mib` in the CSV;
if it approaches 1 GiB or the OOM killer fires, revert.

## 3. Byte watermark

After the 29.8.3 slow block average has produced a normal phase:

```toml
[pruning]
max_bulk_lag_gib = 20
core_resume_ahead_gib = 5
```

Run `tests/ab_umbrel.sh bulk20g 1800`. Keep it only if the phase switch cost
does not outweigh the shorter queues; the CSV's `backlog_bytes` shows the
actual bytes.

## 4. Core solo (`alternate` decision)

With Core downloading and e-lite running:

```bash
PID=$(pgrep -f 'e-lite run' | head -1)
bitcoin-cli -datadir=<core-datadir> getblockcount
kill -STOP "$PID"; sleep 300
bitcoin-cli -datadir=<core-datadir> getblockcount
kill -CONT "$PID"
```

If Core solo is **> 6 blocks/s**, test:

```toml
[pruning]
scheduler = "alternate"
prune_mib = 1
pause_core_during_bulk = true
max_bulk_lag_gib = 20
core_resume_ahead_gib = 5
```

Run `tests/ab_umbrel.sh alternate 1800`. `alternate` is only a candidate when
Core solo is clearly above the break-even; otherwise keep `overlap`.

## 5. Optional prefetch/memtable experiments

```toml
[index]
adaptive_memtables = true
prefetch_deadline_ms = 2000
```

These are **not** defaults. Run `tests/ab_umbrel.sh memtables-adaptive 1800`
and watch:
- `memtables_mib` should move only after a 10-minute persistent target;
- `prefetch_p95` should fall;
- `wait_p95` and apply/commit should not regress.

## 6. Compaction (`background_jobs`)

Baseline evidence (mainnet, end of IBD, ~5 blocks/s): `pending_compaction`
grew 49 → 185 GiB in one hour with `running 1` the whole time, commit/blk went
from 57-80 ms to 150-210 ms, and one 45.5 s commit was unexplained. With the
default plan on a 4-core host, `max_background_jobs = 2` means one flush and
one compaction thread.

```toml
[index]
background_jobs = 4    # two flush + two compaction threads (4-core host)
```

Run for 30-60 min in the same phase as the baseline and compare the 10-minute
`rocksdb:` lines (and `/status.rocksdb`):

| Signal | Keep 4 if… |
|---|---|
| `running` | goes from constant `1` to `2` (compactions in flight) |
| `pending_compaction` | stops growing at the same block rate, or falls |
| `l0` | stays bounded instead of climbing between lines |
| commit/blk | returns to the pre-degradation cost; `commit spike:` lines stop appearing |
| `memory.available_mib` | does not regress (more compaction threads need CPU/RAM headroom) |

If the debt stops growing but commit spikes remain, capture a
`[e-lite] commit spike:` line: it now includes running compactions, L0 files,
memtable, pending compaction and the Core backlog. Changing this default
requires the same evidence on a slow-disk class before shipping.

## How to read the CSV

`phase_blocks / phase_elapsed_ms` is the current phase rate. Ignore the
cumulative `blocks/s` in the log. Compare the same phase at the same height:

| Signal | Keep the variant if… |
|---|---|
| `phase=core` | rate improves ≥ 0.2 b/s and Core still downloads |
| `phase=draining` | rate does not fall and backlog reaches the low mark |
| `memory.available_mib` | stays above ~1 GiB; no OOM |
| `prefetch_p95_ms` | falls without `wait_p95` rising |
| commit | `commit_group`/reason do not collapse to tiny groups |

If a variant is ambiguous, revert; defaults remain conservative.
