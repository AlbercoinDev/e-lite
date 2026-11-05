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
export-snapshot  print the canonical digest or write --output
import-snapshot  restore --input into an empty target
version          print version
```

## Common options

| Flag | Meaning |
|---|---|
| `--config <path>` | TOML file (default `~/.config/e-lite/config.toml`) |
| `--network <name>` | `auto`, `regtest`, `signet`, `testnet4`, `mainnet` |
| `--data-dir <path>` | index directory |
| `--core-rpc <url>` | Core RPC URL (HTTP; use SSH/VPN for WAN) |
| `--rpc-user`, `--rpc-password`, `--cookie-file` | Core credentials |
| `--core-datadir <path>`, `--blocks-dir <path>` | Core datadir / block files |
| `--block-source <src>` | `auto`, `rpc`, `files` |
| `--manage <mode>` | `external`, `auto`, `managed` |
| `--prune-mib <n>` | `1` manual (default), `0` full, `>=550` auto + opt-in |
| `--min-free-gib <n>`, `--critical-free-gib <n>` | disk guards |
| `--listen <addr>`, `--metrics <addr>` | Electrum / metrics listeners |
| `--tls-listen <addr>`, `--tls-cert`, `--tls-key` | optional Electrum TLS |
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
| `--write-buffer-mib <n>` | `index.write_buffer_mib` | `0` (RAM/8) |
| `--memory-reserve-mib <n>` | `index.memory_reserve_mib` | `0` (max(1024, RAM/8)) |
| `--memory-fraction <f>` | `index.memory_fraction` | `0` (0.75 of total RAM) |
| `--background-jobs <n>` | `index.background_jobs` | `0` (profile, capped by CPUs) |
| `--commit-interval-ms <n>` | `index.commit_interval_ms` | `0` (profile; 500..30000) |
| `--commit-sync <mode>` | `index.commit_sync` | `auto` (`txids` in bulk, `always` live) |
| `--prefetch-blocks <n>` | `index.prefetch_blocks` | `0` (reserved) |
| `--batch <n>` | `index.batch_blocks` | `256`/profile |
| `--reorg-window <n>` | `index.reorg_window` | `288` |
| `--milestone-blocks <n>` | `index.milestone_blocks` | `10000` |
| `--milestone-seconds <n>` | `index.milestone_seconds` | `21600` |

`auto` uses the disk `rotational` hint. Virtual disks and some RAID/USB devices
report it incorrectly; set `--profile ssd` or `--profile nvme` when you know the
real hardware.

Explicit readers/commit values lock that knob. `cache_mib` is a **maximum**: the
memory policy can lower it for safety and grow back up to it. See
[`TUNING.md`](TUNING.md#memory-budget).

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

## Pruning and Core

| Flag | TOML field | Default |
|---|---|---|
| `--max-core-lag-blocks <n>` | `pruning.max_core_lag_blocks` | `192` (auto-prune only) |
| `--max-bulk-lag-blocks <n>` | `pruning.max_bulk_lag_blocks` | `20000` |
| `--pause-core-during-bulk` | `pruning.pause_core_during_bulk` | `true` |
| `--no-pause-core-during-bulk` | same | disable the full bulk pause |
| `--core-resume-ahead-blocks <n>` | `pruning.core_resume_ahead_blocks` | `5000` |
| `--prune-defer-free-gib <n>` | `pruning.prune_defer_free_gib` | `50` |
| `--min-gain-blocks <n>` | `pruning.min_gain_blocks` | `10000` |
| `--prune-interval-seconds <n>` | `pruning.prune_interval_seconds` | `30` |
| `--poll-seconds <n>` | `pruning.poll_seconds` | `10` |
| `--repair-max-gap-blocks <n>` | `pruning.repair_max_gap_blocks` | `2048` |
| `--allow-auto-prune` | `pruning.allow_auto_prune` | `false` |
| `--core-arg <arg>` | `core.extra_args` (appended) | none |
| `--stop-on-exit` / `--no-stop-on-exit` | `core.stop_on_exit` | `true` |
| `--restart` / `--no-restart` | `core.restart` | `true` |

By default (`pause_core_during_bulk = true`), a long bulk catch-up pauses
Core's P2P until `core_resume_ahead_blocks` remain (5,000). It then resumes
Core early so peers reconnect while e-lite finishes; after that,
`max_bulk_lag_blocks` (20,000) bounds additional growth relative to the lowest
lag seen and resumes at half. Use `--no-pause-core-during-bulk` for the relative
guard only.

`commit_sync = auto` relaxes the RocksDB fsync during bulk/IBD (`txids.dat` is
still fsynced, so a crash only reindexes the last interval) and returns to
strict `sync=true` when live. Use `always` or `txids` to force a policy.

Normal pruning is deferred while free space is at or above
`prune_defer_free_gib` (50 GiB). Low-disk emergency pruning still runs.

Low-RAM Core example:

```bash
e-lite run --network mainnet --allow-mainnet \
  --core-arg=-dbcache=128 --core-arg=-maxmempool=64
```

## Rawtx, server and TLS

| Flag | TOML field | Default |
|---|---|---|
| `--max-clients <n>` | `server.max_clients` | `32` |
| `--banner-file <path>` | `server.banner_file` | built-in templates |
| `--p2p-peers <a,b>` | `rawtx.p2p_peers` | discovered from Core |
| `--no-p2p` | `rawtx.use_p2p=false` | enabled |
| `--no-esplora` | `rawtx.use_esplora=false` | enabled |
| `--decoys <n>` | `rawtx.decoys` | `3` |
| `--rawtx-cache-mib <n>` | `rawtx.cache_mib` | `2048` |
| `--tor-socks <addr>` | `rawtx.tor_socks` | `127.0.0.1:9050` |
| `--allow-clearnet` | `rawtx.allow_clearnet` | `false` |
| `--no-rawtx` | `rawtx.enabled=false` | enabled |
| `--tls-listen <addr>` | `server.tls_listen` | disabled |
| `--tls-cert`, `--tls-key` | `server.tls_cert`, `server.tls_key` | none |

## Tor and onion

| Flag | TOML field | Default |
|---|---|---|
| `--tor-mode <mode>` | `tor.mode` | `managed` |
| `--no-tor` | `tor.mode=off` | — |
| `--tor-plain` / `--no-tor-plain` | `tor.plain` | `true` |
| `--tor-tls` / `--no-tor-tls` | `tor.tls` | `true` |
| `--tor-auto-cert` / `--no-tor-auto-cert` | `tor.tls_auto_cert` | `true` |
| `--tor-onion-port <n>` | `tor.onion_port` | `50001` |
| `--tor-onion-tls-port <n>` | `tor.onion_tls_port` | `50002` |
| `--tor-socks-port <n>` | `tor.socks_port` | `0` (9050 if free, else 19050+) |
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

## Logging

| Flag | TOML field | Default |
|---|---|---|
| `--log-file <path>` | `logging.file` | `<data-dir>/e-lite.log` |
| `--log-max-mib <n>` | `logging.max_mib` | `64` (0 disables rotation) |
| `--no-log-file` | — | logging enabled |

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
e-lite run --set pruning.max_bulk_lag_blocks=20000
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
- a milestone line every `milestone_blocks` or `milestone_seconds` (whichever
  happens first) with interval throughput and disk usage for e-lite and Core.

Start and stop timestamps are printed in local time. `SIGTERM`/`Ctrl+C` stops
mempool and notification threads before stopping Core, so the shutdown log has
no connection errors.
