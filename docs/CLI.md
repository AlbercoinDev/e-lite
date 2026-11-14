# Command-line reference

Every configuration field can be set from the command line. Explicit flags are
convenience aliases; `--set section.key=value` is the universal escape hatch and
works for any TOML field, including arrays.

Precedence: config file → `--set` (in order) → explicit flags. Validation runs
after all overrides.

All options apply to every network (`regtest`, `signet`, `testnet4`, `mainnet`).
Values scale with the real hardware (RAM, CPUs, disk hint); the examples below
are just starting points.

## Commands

```text
init             validate network/genesis against Core and create the database
doctor           check config, Core, disk, ports, database and memory
index            index the active chain (--no-follow stops at tip)
serve            serve Electrum from an already indexed database (works offline)
run              index and serve in one process; waits for Core RPC
status           print health (HTTP if running, database if stopped)
core status      show the Core supervisor state and RPC reachability
core logs        print the last 50 lines of a Core launched by e-lite
onion show       show the Tor onion address, ports and managed-Tor state
onion reset      generate a new onion address (stop e-lite first)
cache status     size, limits and free space of the rawtx cache (the only
                 on-disk cache: verified transactions and repair blocks)
cache clear      delete the rawtx cache (needs --yes; --dry-run previews it).
                 Never touches the index, Core, TLS or Tor; restart e-lite to
                 release the in-memory block cache as well
export-snapshot  print the canonical digest or write --output
                 (--with-core bundles the network's Core chain data with a
                 SHA-256 per file; index stored, Core gzip 3; --compress-level sets both)
import-snapshot  restore --input into an empty target (atomic: staged, then renamed)
                 (--verify checks per-file hashes; --verify=full opens the
                 restored Core and runs a bounded verifychain;
                 --expect-digest <hex> refuses an archive that is not yours)
arc-setup        print the steps that let e-lite manage the ZFS ARC cap
                 without root (group-writable zfs_arc_max + the
                 e-lite-arc-cap.service oneshot); --apply runs them (root)
version          print version
```

## Common options

| Flag | Meaning |
|---|---|
| `--config <path>` | TOML file (default `~/.config/e-lite/config.toml`) |
| `--public-servers <path>` | provider list (default: `public-servers.toml` next to the config) |
| `--network <name>` | `auto`, `regtest`, `signet`, `testnet4`, `mainnet` |
| `--data-dir <path>` | index directory (default `~/e-lite/<network>`) |
| `--core-rpc <url>` | Core RPC URL (HTTP; use SSH/VPN for WAN) |
| `--set core.require_loopback_rpc=true` | refuse a non-loopback Core RPC endpoint |
| `--rpc-user`, `--rpc-password`, `--cookie-file` | Core credentials |
| `--core-datadir <path>`, `--blocks-dir <path>` | Core datadir (default `<data-dir>/bitcoin`) / block files |
| `--block-source <src>` | `auto`, `rpc`, `files` |
| `--manage <mode>` | `external`, `auto`, `managed` |
| `--prune-mib <n>` | `1` manual (default), `0` full, `>=550` auto + opt-in |
| `--output <path>` | snapshot file for `export-snapshot` |
| `--input <path>` | snapshot file for `import-snapshot` |
| `--with-core`, `--no-hash-files`, `--compress-level <n>` | export-snapshot: bundle the Core chain data (`blocks/`, `chainstate/`, `indexes/` only; never wallets, keys or `bitcoin.conf`), with a SHA-256 per file unless `--no-hash-files` (gzip: index 0, Core 3; the flag sets both) |
| `--verify [basic\|full]` | import-snapshot: per-file hash check, or open the restored Core and `verifychain` |
| `--expect-digest <hex>` | import-snapshot: refuse unless the recomputed index digest equals the `logical_snapshot_hash` printed by your export |
| `--yes`, `-y` | onion reset / cache clear: do not ask for confirmation (required by `cache clear`) |
| `--dry-run` | cache clear: report what would be removed and delete nothing |
| `--json` | machine-readable output (`status`, `cache status`, `cache clear`) |
| `--min-free-gib <n>`, `--critical-free-gib <n>` | disk guards |
| `--listen <addr>`, `--metrics <addr>` | Electrum plain / metrics listeners |
| `--tls` / `--no-tls` | enable (default) or disable Electrum TLS |
| `--tls-listen <addr>`, `--tls-cert`, `--tls-key` | TLS listener and PEM files |
| `--json` | JSON output for `status` |
| `--allow-mainnet` | required for mainnet |
| `--set <key=value>` | override any TOML field |

## Tuning and performance

| Flag | TOML field | Default |
|---|---|---|
| `--profile <name>` | `index.profile` | `auto` |
| `--adaptive` / `--no-adaptive` | `index.adaptive` | `true` |
| `--readers <n>` | `index.readers` | `0` (profile/auto) |
| `--cache-mib <n>` | `index.cache_mib` | `0` (auto; explicit = maximum) |
| `--write-buffer-mib <n>` | `index.write_buffer_mib` | `0` (effective/16, 512 MiB..2 GiB) |
| `--memory-reserve-mib <n>` | `index.memory_reserve_mib` | `0` (clamp(effective/8, 2-4 GiB)) |
| `--memory-fraction <f>` | `index.memory_fraction` | `0` (0.75 of **effective** RAM) |
| `--allow-low-memory` | `index.allow_low_memory` | `false` (unsupported; CI/regtest only) |
| `--background-jobs <n>` | `index.background_jobs` | `0` (profile, capped by CPUs) |
| `--commit-interval-ms <n>` | `index.commit_interval_ms` | `0` (profile; 500..30000) |
| `--commit-sync <mode>` | `index.commit_sync` | `auto` (`txids` in bulk, `always` live) |
| `--prefetch-blocks <n>` | `index.prefetch_blocks` | `0` (auto; >0 caps the window) |
| `--prefetch-deadline-ms <n>` | `index.prefetch_deadline_ms` | `0` (off; 50..60000) |
| `--max-open-files <n>` | `index.max_open_files` | `0` (8192; 2048/4096 for 8-16 GB) |
| `--adaptive-memtables` / `--no-adaptive-memtables` | `index.adaptive_memtables` | `false` |
| `--batch <n>` | `index.batch_blocks` | `256`/profile, capped at 512 |
| `--reorg-window <n>` | `index.reorg_window` | `288` |
| `--milestone-blocks <n>` | `index.milestone_blocks` | `10000` |
| `--milestone-seconds <n>` | `index.milestone_seconds` | `21600` |
| `--set index.zfs_arc_dynamic=false` | dynamic ZFS ARC cap (grow during IBD/drain, shrink when synced; see `arc-setup`) | `true` |
| `--set index.zfs_arc_max_mib=<n>` | ARC cap during IBD/drain (0 = half of RAM) | `0` |
| `--set index.zfs_arc_min_mib=<n>` | ARC cap once fully synced (floor 512) | `1024` |

`auto` uses the disk `rotational` hint. Virtual disks and some RAID/USB devices
report it incorrectly; set `--profile ssd` or `--profile nvme` when you know the
real hardware.

Explicit readers/commit values lock that knob. `cache_mib` is a **maximum**: the
memory policy can lower it for safety and grow back up to it. The automatic
plan is fitted to `MemAvailable`, not to installed RAM: 8 GiB machines get about
2 GiB of cache, 16 GiB about 4 GiB and 32 GiB up to 8 GiB, always subject to
what the OS has free. See [`TUNING.md`](TUNING.md#memory-budget-fitted-to-available-ram).

### Examples

Normal PC with NVMe and 16-32 GiB RAM:

```bash
e-lite run --network mainnet --allow-mainnet --profile nvme
```

Normal PC with SATA SSD:

```bash
e-lite run --network mainnet --allow-mainnet --profile ssd
```

Laptop/HDD or a disk that reports `rotational=1`:

```bash
e-lite run --network signet --profile hdd
```

Low-RAM machine or container (1-4 GiB):

```bash
e-lite run --network signet --profile hdd \
  --cache-mib 256 --write-buffer-mib 128 --background-jobs 2
```

Fixed values, no controller:

```bash
e-lite run --network mainnet --allow-mainnet \
  --profile ssd --no-adaptive --readers 4 --cache-mib 1024 --commit-interval-ms 3000
```

Snapshot backup of one network, restored on another host (run per network;
stop e-lite and Core first — the export refuses otherwise):

```bash
e-lite export-snapshot --network mainnet --allow-mainnet \
  --output /backups/full-mainnet-<tip>.tar.gz --with-core

e-lite import-snapshot --network mainnet --allow-mainnet \
  --data-dir ~/e-lite/mainnet --input /backups/full-mainnet-<tip>.tar.gz \
  --verify=full --expect-digest <logical_snapshot_hash from the export>
```

## Pruning and Core

| Flag | TOML field | Default |
|---|---|---|
| `--max-core-lag-blocks <n>` | `pruning.max_core_lag_blocks` | `192` (auto-prune only) |
| `--max-bulk-lag-blocks <n>` | `pruning.max_bulk_lag_blocks` | `20000` (legacy) |
| `--max-bulk-lag-gib <n>` | `pruning.max_bulk_lag_gib` | `30` |
| `--pause-core-during-bulk` | `pruning.pause_core_during_bulk` | `true` |
| `--no-pause-core-during-bulk` | same | disable the full bulk pause |
| `--core-resume-ahead-blocks <n>` | `pruning.core_resume_ahead_blocks` | `5000` (legacy) |
| `--core-resume-ahead-gib <n>` | `pruning.core_resume_ahead_gib` | `7` |
| `--scheduler <mode>` | `pruning.scheduler` | `overlap` |
| `--prune-defer-free-gib <n>` | `pruning.prune_defer_free_gib` | `20` |
| `--min-gain-blocks <n>` | `pruning.min_gain_blocks` | `5000` |
| `--set pruning.min_gain_live_blocks=<n>` | once fully synced, prune every N safe blocks (0 = `min_gain_blocks`) | `144` |
| `--min-free-gib <n>` | `pruning.min_free_gib` (pause Core downloads below it) | `10` |
| `--critical-free-gib <n>` | `pruning.critical_free_gib` (pause indexing below it) | `1` |
| `--prune-interval-seconds <n>` | `pruning.prune_interval_seconds` | `30` |
| `--poll-seconds <n>` | `pruning.poll_seconds` | `10` |
| `--repair-max-gap-blocks <n>` | `pruning.repair_max_gap_blocks` | `2048` |
| `--allow-auto-prune` | `pruning.allow_auto_prune` | `false` |
| `--core-arg <arg>` | `core.extra_args` (appended) | none |
| `--core-io-priority <0-7>` | `core.io_priority` | unset (opt-in ionice) |
| `--core-cgroup-io-weight <n>` | `core.cgroup_io_weight` | unset (opt-in cgroup v2 io.weight) |
| `--stop-on-exit` / `--no-stop-on-exit` | `core.stop_on_exit` | `true` |
| `--restart` / `--no-restart` | `core.restart` | `true` |

By default (`pause_core_during_bulk = true`), a long bulk catch-up pauses
Core's P2P when the unindexed backlog reaches `max_bulk_lag_gib` (30 GiB) and
resumes it at `core_resume_ahead_gib` (7 GiB). The backlog is
`lag_blocks × avg_block_bytes`, so the threshold means the same disk footprint
at 100k and at 800k; blocks are not comparable across heights. The older
`max_bulk_lag_blocks`/`core_resume_ahead_blocks` pair is still honoured only
when the corresponding GiB value is `0`.

`scheduler = "overlap"` (default) keeps e-lite indexing while Core downloads.
`scheduler = "alternate"` pauses e-lite indexing during the Core phase and
pauses Core during the e-lite phase, so only one of them writes at a time. It
requires `prune_mib = 1` (manual prune) and `pause_core_during_bulk = true`;
serving (Electrum, Tor, metrics) continues while indexing is paused. Use it to
test whether removing I/O contention beats the lost overlap; it is not the
default.

`commit_sync = auto` relaxes the RocksDB fsync during bulk/IBD (`txids.dat` is
still fsynced, so a crash only reindexes the last interval) and returns to
strict `sync=true` when live. Use `always` or `txids` to force a policy.

Normal pruning keeps a 20 GiB free-space margin by default
(`prune_defer_free_gib = 20`); the 5k-block gain and 30 s interval still apply.
Low disk bypasses the deferral.
Set a positive value to defer normal pruning on a large disk.

Low-RAM Core example:

```bash
e-lite run --network mainnet --allow-mainnet \
  --core-arg=-dbcache=128 --core-arg=-maxmempool=64
```

## Rawtx, server and TLS

| Flag | TOML field | Default |
|---|---|---|
| `--max-clients <n>` | `server.max_clients` | `32` |
| `--set server.rate_limit=<n>` | `server.rate_limit` | `200` for every client; `0` disables |
| `--set server.rate_burst=<n>` | `server.rate_burst` | `5000` |
| `--banner-file <path>` | `server.banner_file` | built-in templates |
| `--p2p-peers <a,b>` | `rawtx.p2p_peers` | discovered from Core |
| `--no-p2p` | `rawtx.use_p2p=false` | enabled |
| `--no-esplora` | `rawtx.use_esplora=false` | enabled |
| `--decoys <n>` | `rawtx.decoys` | `1` (max 2 in flight) |
| `--rawtx-cache-mib <n>` | `rawtx.cache_mib` | `256` |
| `--set rawtx.cache_dir=<path>` | `rawtx.cache_dir` | `<data-dir>/rawtx-cache`; tmpfs keeps it off disk |
| `--tor-socks <addr>` | `rawtx.tor_socks` | `127.0.0.1:9050` |
| `--allow-clearnet` | `rawtx.allow_clearnet` | `false` |
| `--set rawtx.tor_fallback_clearnet=true` | use clearnet when Tor fails (exposes your IP to the provider) | `false` |
| `--set rawtx.prefetch=true` | download a wallet's history blocks in the background (faster first load, no decoys) | `false` |
| `--plain-lan` / `--set server.plain_lan=true` | plain Electrum on `0.0.0.0` | `false` |
| `--set server.advertise_electrumx_compat=true` | `server.version` banner `e-lite X (electrumx-compatible)`, so Sparrow uses JSON-RPC batches; hot-reloadable | `false` |
| `--set core.network=auto\|private\|clearnet` | P2P networks of a Core started by e-lite | `auto` |
| `--set core.private_broadcast=true` | broadcast wallet transactions over short-lived Tor/I2P connections (experimental in Core) | `false` |
| `--set core.i2p_sam=auto\|host:port` , `--set core.cjdns=true` | extra private networks for Core | `auto`, `false` |
| `--set rawtx.request_timeout_seconds=<n>` | per-provider block timeout, direct (clearnet) | `8` |
| `--set rawtx.tor_request_timeout_seconds=<n>` | per-provider block timeout through Tor (new circuit + whole block) | `20` |
| `--set rawtx.provider_cooldown_seconds=<n>` | cool down a failed provider (Tor or direct) | `300` |
| `--set rawtx.max_parallel_fetches=<n>` | concurrent external block downloads | `8` |
| `--set rawtx.p2p_timeout_seconds=<n>` | P2P connect/handshake/block timeout | `8` |
| `--set public_servers.require_ready=false` | start without a ready provider | `true` |
| `--set public_servers.probe_timeout_seconds=<n>` | per-provider probe timeout (parallel probe; Tor attempts get at least `rawtx.tor_request_timeout_seconds`) | `5` |
| `--set public_servers.ready_timeout_seconds=<n>` | one readiness round before it is logged and retried | `60` |
| `--set public_servers.open_without_provider=true` | pruned Core: open Electrum with local data after `ready_timeout_seconds` if no provider answered | `false` |
| `--no-rawtx` | `rawtx.enabled=false` | enabled |
| `--tls` / `--no-tls` | `server.tls` | `true` |
| `--tls-listen <addr>` | `server.tls_listen` | host of `listen` + network TLS port |
| `--tls-cert`, `--tls-key` | `server.tls_cert`, `server.tls_key` | auto-generated self-signed |
| `--set server.tls_auto_cert=false` | `server.tls_auto_cert` | `true` |

## Listener defaults per network

Empty values are filled from the active network, so several e-lite instances
(one per chain) can run at once:

| Network | Plain | TLS | Metrics (loopback) | Default data dir | Default Core datadir |
|---|---:|---:|---:|---|---|
| mainnet | 55011 | 55022 | 55084 | `~/e-lite/mainnet` | `~/e-lite/mainnet/bitcoin` |
| testnet4 | 55111 | 55122 | 55184 | `~/e-lite/testnet4` | `~/e-lite/testnet4/bitcoin` |
| signet | 55211 | 55222 | 55284 | `~/e-lite/signet` | `~/e-lite/signet/bitcoin` |
| regtest | 55311 | 55322 | 55384 | `~/e-lite/regtest` | `~/e-lite/regtest/bitcoin` |

By default TLS binds `0.0.0.0` and the plain listener `127.0.0.1` (wallets on
this host and the onion service). `--plain-lan` / `[server] plain_lan = true`
puts plain on `0.0.0.0` too. An explicit `listen` is used as given and the TLS
listener then copies its host, so `[server] listen = "127.0.0.1:55011"` keeps
TLS on loopback. `--listen`,
`--tls-listen` and `--metrics` override the ports.

The default data directory only applies to new installs. An existing
`~/.local/share/e-lite/<network>` is still used until it is moved. A single
legacy Core datadir (`~/.bitcoin` or `~/e-lite/bitcoin`) with chain data is
detected automatically; if two candidates exist, set `[core] core_datadir`
explicitly. Each network must use its own Core datadir: Bitcoin Core cannot
serve two chains from the same datadir at once.

Requests above `server.rate_limit` **wait for the next token** instead of
returning an error, so a wallet's initial gap scan is not aborted.

## Tor and onion

| Flag | TOML field | Default |
|---|---|---|
| `--tor-mode <mode>` | `tor.mode` | `managed` |
| `--no-tor` | `tor.mode=off` | — |
| `--tor-plain` / `--no-tor-plain` | `tor.plain` | `true` |
| `--tor-tls` / `--no-tor-tls` | `tor.tls` | `true` |
| `--tor-auto-cert` / `--no-tor-auto-cert` | `tor.tls_auto_cert` | `true` |
| `--tor-onion-port <n>` | `tor.onion_port` | `0` (same as `server.listen`) |
| `--tor-onion-tls-port <n>` | `tor.onion_tls_port` | `0` (same as `server.tls_listen`) |
| `--tor-socks-port <n>` | `tor.socks_port` | `0` (per-network: 9050 mainnet, 55150/55250/55350; then 19050+) |
| `--tor-binary <path>` | `tor.binary` | installer bundle, then PATH |
| `--tor-target-plain <addr>` | `tor.target_plain` | `server.listen` (wildcard mapped to loopback) |
| `--tor-target-tls <addr>` | `tor.target_tls` | `server.tls_listen` (wildcard mapped to loopback) |

`mode = managed` starts e-lite's own Tor and publishes Electrum as an onion
service. `--tor-socks <addr>` (in the previous table) remains the rawtx proxy;
the managed Tor overrides it only when it is unset or still the default
`127.0.0.1:9050`.

```bash
e-lite onion show  --network mainnet --allow-mainnet
e-lite onion reset --network mainnet --allow-mainnet --yes
e-lite onion reset --network mainnet --allow-mainnet --yes --purge   # delete instead of onion.prev
e-lite run --network mainnet --allow-mainnet --no-tor
```

See [`TOR.md`](TOR.md) for installation, ports, the self-signed TLS certificate,
reset semantics and security.

## Rawtx cache maintenance (`cache`)

The rawtx cache is the **only** memoised data e-lite keeps on disk: verified
transactions with their merkle branch (`tx/<xx>/<txid>.tx`) and whole validated
blocks kept for prune-gap repair (`*.blk`). It lives in
`<data-dir>/rawtx-cache` unless `[rawtx] cache_dir` points elsewhere (tmpfs is a
supported choice), and it is bounded by `[rawtx] cache_mib` (256 MiB: a quarter
for whole blocks, 1-64 MiB, the rest for transactions).

```bash
e-lite cache status [--json] [--network <n>]
e-lite cache clear  [--yes] [--dry-run] [--json] [--network <n>]
```

- `status` prints the directory, the files/bytes held, the configured limits and
  the free space of that filesystem. The directory walk is explicit (it is the
  only command that reads the cache on demand).
- `clear` deletes **only** regular `*.blk` files in the cache root and `*.tx`
  files under a two-hex `tx/<xx>/` shard. Empty shards are removed; anything
  else is reported as skipped and left in place. Without `--yes` it only says
  what it would delete; `--dry-run` does the same explicitly.
- It refuses (non-zero exit, nothing deleted) when the resolved path is the data
  directory, contains it, is a filesystem root, holds a RocksDB `CURRENT`, or
  is/contains `index/`, `bitcoin/`, `tls/` or `tor/`. The index, Core's datadir,
  the TLS identity and the onion keys are never touched.
- It works with the daemon running (a vanished file is a cache miss), but the
  in-memory block cache is only released by restarting e-lite.
- It does not need Core: `network = auto` resolves from the existing data
  directories; pass `--network` when several exist.

JSON contract (used by the Umbrel app):

```json
{"network":"mainnet","path":"/data/networks/mainnet/e-lite/rawtx-cache",
 "enabled":true,"capacity_bytes":268435456,
 "blocks":{"files":3,"bytes":2097152,"limit_bytes":67108864},
 "txs":{"files":812,"bytes":4308992,"limit_bytes":201326592},
 "total":{"files":815,"bytes":6406144},"skipped":0,"free_bytes":13316915200}
```

`clear --yes --json` returns `removed` and `remaining` (same shape as `total`),
`skipped` and `free_bytes_before`/`free_bytes_after`.

## Wallet banner

`server.banner` returns a dynamic banner. Built-in templates show `INDEXING`
with height/percent/bar/ETA, or `READY`/`FULLY SYNCED` with the Electrum
endpoints. Set `--banner-file <path>` (or `[server] banner_file`) to customize:

- the file is created with the default template if it does not exist;
- a relative path is resolved inside the network `data-dir`;
- `[indexing]` and `[ready]` sections are used for each state;
- a file without sections is used for both;
- placeholders: `{version}`, `{network}`, `{state}`, `{height}`, `{headers}`,
  `{percent}`, `{bar}`, `{core_height}`, `{core_percent}`, `{lag}`,
  `{blocks_per_sec}`, `{tx_per_sec}`, `{eta}`, `{prune}`, `{pruneheight}`,
  `{listen}`, `{tls}`, `{log_file}`, `{uptime}`, `{ready}`, `{ready_note}`;
- the file is read on each `server.banner` request (max 32 KiB), so edits apply
  without restart; wallets that cache the banner may need to reconnect.

The protocol has no banner push: the authoritative readiness signal is the log
line (`READY`/`FULLY SYNCED`), `/status` and `e-lite status`.

## Performance environment variables

These are escape hatches for measurements, not normal configuration:

| Variable | Effect |
|---|---|
| `E_LITE_LAG_POLICY=drain` | Use the opt-in drain-time lag watermark instead of the fixed block window |
| `E_LITE_LAG_TARGET_SECONDS=<n>` | Drain target for `drain` (default 1800, clamped 120..21600) |
| `E_LITE_NO_SYNC_LOCK=1` | Allow two intensive syncs at once (not recommended) |
| `E_LITE_ALLOW_LOW_MEMORY=1` | Bypass the RAM preflight (CI only) |

`/status` exposes the new diagnostics:

- `pipeline.prepared/applied/discarded`: work prepared by readers versus blocks
  actually applied. `discarded` is the in-flight window of the last 512-block
  checkpoint, not one window per 10 s.
- `tor.managed/socks_port/control_port`: effective SOCKS address for external
  requests. `control_port` is always `0`; e-lite does not create one.
- `rocksdb.properties_ok`: `false` means a zero in that block is unknown, not
  empty. `rocksdb.block_cache_bytes` is the real bytes charged to the shared
  cache, read once (with a shared cache every family reports the whole-cache
  usage, so it is not summed; expected ≈ the memory plan's cache capacity).
  `rocksdb.level0_files` is the summed
  level-0 file count: growing `l0` with `running_compactions = 1` is the
  flush/compaction stall signal. `rocksdb.cf` breaks pending compaction,
  memtables, SST and table readers down by column family.
- `machine`: the detected CPU (physical/logical/effective/SMT/quota), memory
  (reserve, page-cache floor), disk class and the resulting knobs. Explicit
  config values are reflected in `plan`. `plan.max_group_bytes` is the raw-byte
  bound for a bulk commit group.
- `tuning.commit_cycle_ms`: real start-to-start commit interval between **full**
  commit groups. Boundary/shutdown partials are excluded from the EWMA and the
  cycle. `tuning.commit_last_*` exposes the last full group: `ms`, `cycle_ms`,
  `blocks`, `txs`, `bytes`, `txids_us`, `db_us` and `reason`
  (1 blocks, 2 txs, 3 bytes, 4 time, 5 boundary, 6 shutdown).
- `commit_breakdown_us.txids_append_fsync` and `rocksdb_write`: separate the
  flat-file fsync from the RocksDB write.
- `scheduler`: `mode` (`overlap`/`alternate`), `index_paused`,
  `backlog_bytes` (`lag × average raw block size`), `avg_block_bytes`,
  `high_bytes`, `low_bytes`, `phase` (`indexing`/`draining`/`core`),
  `phase_blocks` and `phase_elapsed_ms`. In `alternate`, `index_paused = true`
  means Core is downloading and e-lite is serving but not indexing. If the
  configured mode is `alternate` but Core cannot be paused, the effective mode
  is `overlap` and a warning is logged.
- `tuning.prefetch_p50_ms`, `prefetch_p95_ms`, `prefetch_max_ms`,
  `wait_p50_ms` and `wait_p95_ms`: the cache controller uses p95, not the
  tail-dominated EWMA.

## Logging

| Flag | TOML field | Default |
|---|---|---|
| `--log-file <path>` | `logging.file` | `<data-dir>/e-lite.log` |
| `--log-max-mib <n>` | `logging.max_mib` | `16` (0 disables rotation) |
| `--log-level <level>` | `logging.level` | `debug` (alpha; stable: `info`) |
| `--no-log-file` | — | logging enabled |

`logging.level` selects how much extra detail is logged:
`info` < `debug` < `trace`. The historical `[e-lite]` lines are INFO and are
always printed with an unchanged format; the extra detail prints as
`[e-lite] debug:` / `[e-lite] trace:` lines and the level is hot-reloadable
(SIGUSR1) like the other hot fields.

- `debug`: per-family RocksDB counters (`rocksdb-cf`), per-level file counts
  (`rocksdb-levels`), live RocksDB statistics with cache hit ratio and
  WAL-sync latency (`rocksdb-stats`), Linux I/O pressure and the ZFS ARC hit
  rate next to every memory sample (`io-pressure`), the cache controller's
  decision with its inputs (`cache-controller`), the byte-window arithmetic
  at every scheduler phase change (`scheduler:`) and the wallet-session
  diagnostics (`wallet:`, `wallet session:` — counts and milliseconds only,
  never a hash, script or txid).
- `trace`: additionally one line per commit group with its txids/db split.

The file receives exactly the same lines as the console (stdout + stderr),
including progress, milestones, tuning and TLS messages. Rotation keeps one
backup (`e-lite.log.1`) and the file mode is `0640`. Logs are per network because
the default path lives in the network data directory. `e-lite core logs` still
shows Core's own log.

## `--set` examples

```bash
# Any tuning field
e-lite run --set index.profile=nvme --set index.cache_mib=4096

# Arrays and nested values (TOML syntax)
e-lite run --set 'rawtx.p2p_peers=["203.0.113.10:8333","[2001:db8::1]:8333"]'
e-lite run --set 'core.extra_args=["-dbcache=256","-maxmempool=128"]'

# Pruning and server fields
e-lite run --set pruning.max_bulk_lag_gib=40 --set pruning.scheduler=alternate
e-lite serve --set server.max_clients=64
```

Bare words are treated as strings (`--set index.profile=ssd`); `true`, numbers
and TOML arrays are parsed as TOML.

## Progress output

`run`/`index` print a line after each commit with:

- indexed height and percentage of Core's `headers`;
- Core blocks/headers and percentage, plus verification progress during IBD;
- lag, blocks/s, tx/s, elapsed time and read/decode/prefetch/wait/apply/commit
  metrics, plus `commit/blk`;
- `phase X b/s` is the average since the phase changed (the whole session if
  it never changes). `recent` is the rate over the last 60 s of full commit
  groups — the honest "right now" — and `eta` is `lag / recent` when caught
  up is still ahead;
- a milestone line every `milestone_blocks` or `milestone_seconds` (whichever
  happens first) with interval throughput and disk usage for e-lite and Core.

The percentage never rounds up to 100.0 % while blocks remain (it caps at
99.9 %); the catch-up completes with two explicit lines, printed once:

```text
[e-lite] READY: index caught up to Core at height 975411 (975411 headers; Core caught up; session 23m 18s); ...
[e-lite] FULLY SYNCED: height 975411/975411; session 23m 18s (indexing and serving since the start); ...
```

`READY` also requires Core to be out of IBD; `FULLY SYNCED` additionally
requires Core's headers at the same height. `/status.ready` and
`/status.fully_synced` expose the same flags for the UI.

Start and stop timestamps are printed in local time. `SIGTERM`/`Ctrl+C` stops
mempool and notification threads before stopping Core, so the shutdown log has
no connection errors.
