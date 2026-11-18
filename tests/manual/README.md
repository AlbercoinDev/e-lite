# Manual A/B runs on a real node

`ab_umbrel.sh` samples a running e-lite's `/status` into a CSV so two
configurations can be compared on the same machine (mainnet or any test box).
It does not change configuration or restart anything. The name is historical:
it works on any host, not only Umbrel.

These measurements decide tuning defaults; e-lite does not change them
automatically. Run one variant at a time and keep the previous CSV. On a closed
box (Umbrel, appliance) the protocol is the same: run the variant, then pass the
log or CSV to the analysis. The compaction (`background_jobs`) experiment is
already decided; see [`docs/TUNING.md`](../../docs/TUNING.md#compaction-debt-l0-files-and-background_jobs).

## 0. Baseline

```toml
[index]
readers = 4          # or 0/auto
cache_mib = 0

[pruning]
max_bulk_lag_gib = 30
core_resume_ahead_gib = 7
```

Start e-lite, wait for a normal Core/e-lite phase, then sample for 30-60 min:

```bash
tests/manual/ab_umbrel.sh baseline 1800
```

`E_LITE_STATUS_URL` defaults to `http://127.0.0.1:55084/status` (mainnet
metrics); set it if your instance maps the port differently. The CSV is written
to `/tmp/e-lite-ab-baseline.csv`.

## 1. Readers

```toml
[index]
readers = 6
```

Restart, run `tests/manual/ab_umbrel.sh readers6 1800`, compare:
`phase=indexing` (Core downloading while e-lite indexes) and `phase=draining`
(Core paused).
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

Run `tests/manual/ab_umbrel.sh cache6g 1800`. Watch `memory.available_mib` in the CSV;
if it approaches 1 GiB or the OOM killer fires, revert.

## 3. Byte watermark

After a normal Core/e-lite phase:

```toml
[pruning]
max_bulk_lag_gib = 20
core_resume_ahead_gib = 5
```

Run `tests/manual/ab_umbrel.sh bulk20g 1800`. Keep it only if the phase switch cost
does not outweigh the shorter queues; the CSV's `backlog_bytes` shows the
actual bytes.

## How to read the CSV

`phase_blocks / phase_elapsed_ms` is the current phase rate. Ignore the
cumulative `blocks/s` in the log. Compare the same phase at the same height:

| Signal | Keep the variant if… |
|---|---|
| `phase=indexing` | rate improves ≥ 0.2 b/s and Core still downloads |
| `phase=draining` | rate does not fall and backlog reaches the low mark |
| `memory.available_mib` | stays above ~1 GiB; no OOM |
| `prefetch_p95_ms` | falls without `wait_p95` rising |
| commit | `commit_group`/reason do not collapse to tiny groups |

If a variant is ambiguous, revert; defaults remain conservative.

## Wallet recovery in Sparrow (29.26.1 versus 29.26.2)

Use a disposable, fully synced mainnet instance. Keep Core, index, providers,
Tor settings, privacy profile and client transport the same. Stop e-lite
before changing the binary or `rawtx.cache_dir`; use a new cache directory
per run and leave the production cache alone. Do not downgrade a live
installation to run the baseline.

Run each version at least three times, alternating order. Use a new Sparrow
watch-only wallet on each run or otherwise confirm Sparrow actually asked
for the transactions again. Keep the xpub and the wallet local. Take the
first `Loading transactions`, first `Retrieving` and `Finished loading`
timestamps from Sparrow, and note whether the same number of transactions
loaded. A run immediately after startup and a run after the provider probes
settle are different conditions: compare like for like.

Capture `/status` before and after and the matching e-lite log window. Count
failed `stream session` and `stream block` attempts separately from canceled
losing hedges. In 29.26.2 also record probe overhead and
`rawtx.tx_mode.items_by_operator`; in 29.26.1 derive the split from endpoint
and item counts in the stream logs, grouping aliases of the same operator.
Compare medians, worst runs and operator shares, not just the fastest run.
The [offline provider benchmark](../../docs/benchmarks/provider-qualification.md)
is a separate check and does not use Sparrow or live Tor.
