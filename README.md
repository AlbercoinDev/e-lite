# e-lite

**Electrum 1.4 server for Bitcoin Core.** Query the history, balance and UTXOs of
any `scriptPubKey` in the active chain — **without registering wallets**.

`e-lite` indexes the chain from Bitcoin Core and serves Electrum JSON-RPC on
loopback by default. Core remains the authority for consensus and the active
chain; e-lite never invents data.

> **Status: `v0.1.0-alpha.10` — experimental, no funds.**
> Use on test networks first.
>
> **Testing from scratch:** see [`docs/GETTING-STARTED.md`](docs/GETTING-STARTED.md)
> for prerequisites, disk requirements, the regtest quickstart, the full
> signet/testnet4 acceptance and troubleshooting.

## One command: install/update, test and report

From a checkout (uses your working tree):

```bash
# Regtest end-to-end: installs/updates, starts Core, mines, queries and reports
bash scripts/e-lite-all-in-one.sh --network regtest --quick --with-core --yes

# Full signet acceptance (needs >=15 GiB free)
bash scripts/e-lite-all-in-one.sh --network signet --acceptance --with-core --yes

# Full testnet4 acceptance (needs >=20 GiB free)
bash scripts/e-lite-all-in-one.sh --network testnet4 --acceptance --with-core --yes
```

From GitHub (published `main`):

```bash
curl -fsSL https://raw.githubusercontent.com/AlbercoinDev/e-lite/main/scripts/e-lite-all-in-one.sh \
  | bash -s -- --network signet --acceptance --with-core --yes
```

The script installs dependencies and Rust if missing, installs/updates Bitcoin
Core, builds and installs e-lite, writes a config under `~/e-lite-test/`, starts
Core through e-lite, runs the selected test, and writes a report to
`~/e-lite-test/last-report.txt`.

## Supported networks

`regtest` · `signet` · `testnet4` · `mainnet` (explicit opt-in).
Testnet3 is intentionally not supported.

## Requirements

- Linux **Debian 12+ / LMDE 6+**, x86_64, glibc ≥ 2.36.
- Bitcoin Core 28+ (official binary from bitcoincore.org). `txindex` is **not** needed.
- For a first pruned IBD, e-lite and Core must be on the **same host**, with
  **read** access to Core's `blocks/` directory (including `xor.dat`). This is
  the only supported way to index from scratch against a pruned Core.
- The index lives in its own directory; e-lite never writes to the Core datadir.
- Mainnet production is usually restored from a snapshot; a from-scratch mainnet
  index is possible but I/O-heavy (see “Disk expectations”).

## Install (one line, also updates)

```bash
curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh | sh
```

The installer needs `curl` and `ca-certificates` (present on standard Debian
desktops). The same command installs or **updates** e-lite (atomic binary
replacement, config and database untouched) and **asks** whether to install or
update the latest official Bitcoin Core. Without a terminal it defaults to *no*.

Non-interactive variants:

```bash
# e-lite + Bitcoin Core, no questions
curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh | sh -s -- --with-core --yes

# only e-lite
curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh | sh -s -- --no-core

# system-wide .deb (root)
curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/e-lite_amd64.deb -o /tmp/e-lite.deb && sudo dpkg -i /tmp/e-lite.deb
```

Installed to `${E_LITE_PREFIX:-$HOME/.local/bin}` by default, **no sudo**.

## Quickstart: e-lite starts and manages Core

```bash
# From an empty datadir, with maximum Core pruning (550 MiB target).
e-lite run --network signet --core-datadir ~/.bitcoin --data-dir ~/.local/share/e-lite/signet
```

With the default `[core] manage = "auto"`:
- if Core is already running, e-lite uses it read-only;
- if Core is not running, e-lite starts it, waits for its RPC and indexes;
- `init` is **not required**: `run` auto-initializes the database when Core is
  available;
- if Core is down, e-lite binds the ports, shows `WAITING_FOR_CORE` and retries;
- if the database already exists, e-lite serves cached history while Core is down.

For explicit one-shot setup:

```bash
e-lite doctor --network signet --core-datadir ~/.bitcoin
e-lite init   --network signet --core-datadir ~/.bitcoin --data-dir ~/.local/share/e-lite/signet
e-lite run    --network signet --core-datadir ~/.bitcoin --data-dir ~/.local/share/e-lite/signet
```

Connect Electrum or Sparrow to `127.0.0.1:50001` (no SSL).

## Core management

```toml
[core]
manage = "auto"   # external | auto | managed
```

| Mode | Behaviour |
|---|---|
| `external` | e-lite never starts, stops, pauses or prunes Core. It waits for Core and serves cached data. |
| `auto` | If Core is running, use it read-only. If not, start and supervise it. |
| `managed` | Always start Core; fail if another instance is already running. |

**Only a Core started by e-lite can be paused, pruned or stopped by e-lite.**
A Core that was already running is never touched: e-lite does not call
`setnetworkactive` or `pruneblockchain` on it.

`prune_mib = 550` is Core's minimum (maximum pruning). `0` means a full node;
`1` means manual pruning. e-lite passes `-prune`, `-rpcport` (taken from
`rpc_url`), `-rpcbind=127.0.0.1`, `-rpcallowip=127.0.0.1`, `-server=1` and
`-daemon=0`, plus `extra_args`. It does not edit `bitcoin.conf` and does not
disable indexes you have enabled.

A Core launched by e-lite is logged to `<data-dir>/bitcoind.log`, tracked in
`<data-dir>/core-child.pid` and stopped on clean shutdown when
`stop_on_exit = true`.

## From-zero pruned IBD (local only)

This is the supported path for “both start from zero”:

1. e-lite starts Core with `-prune=550` (or `1` for manual pruning).
2. e-lite reads raw blocks directly from Core's `blk*.dat` files.
3. Core keeps at least 288 blocks; the lag guard pauses its downloads only when
   the index lag approaches that retention window.
4. e-lite never lets the index fall below `pruneheight`.

The guard uses the real retention (`blocks - pruneheight`), not a fixed 32:
the default `max_core_lag_blocks = 256` is capped by Core's window and resumes
at 75%. It also pauses Core while a `transaction.get` is resolving a block near
the prune boundary.

**Not supported:** a pruned Core that e-lite did not start, or a remote pruned
Core, with an empty index. e-lite fails with instructions instead of risking a
hole:

```
Core is pruned and the index is empty, but e-lite did not start Core; stop Core
and use [core] manage = "managed" (or use a full node/snapshot)
```

## Remote Core (maintenance only)

Core remote is supported for **keeping an existing index up to date**, not for
the first pruned IBD. Use an SSH or VPN tunnel; e-lite speaks plain HTTP to Core:

```toml
[core]
manage = "external"
rpc_url = "http://127.0.0.1:8332"   # forwarded by SSH: ssh -L 8332:127.0.0.1:8332 host
# rpc_user = "..."
# rpc_password = "..."
```

`cookie_file` also works if you copy Core's cookie. For a LAN Core, point
`rpc_url` at its address and use `rpc_user`/`rpc_password`. Do **not** expose
Core's RPC port to the Internet without TLS/VPN.

If the remote Core is pruned and the index is behind `pruneheight`, e-lite
fails and asks for a snapshot or a full node. It never pauses a shared node.

## Configuration

Every value shown is the default. The full file is `config.example.toml`.

```toml
[network]
name = "auto"                 # auto | mainnet | testnet4 | signet | regtest
# data_dir = "/home/USER/.local/share/e-lite"

[core]
manage = "auto"               # external | auto | managed
bitcoind = "bitcoind"
core_datadir = "/home/USER/.bitcoin"
# rpc_url = "http://127.0.0.1:8332"
# rpc_user = ""
# rpc_password = ""
# cookie_file = ""
timeout_seconds = 30
prune_mib = 550               # 550 = max pruning; 0 = full; 1 = manual
extra_args = []
restart = true
stop_on_exit = true
# log_file = ""

[index]
block_source = "auto"         # auto | rpc | files
readers = 0                   # 0 = auto
cache_mib = 0                 # 0 = auto
reorg_window = 288
batch_blocks = 256
follow = true

[pruning]
min_free_gib = 10             # pause Core + indexing below this
max_core_lag_blocks = 256     # capped by Core retention
poll_seconds = 10

[server]
listen = "127.0.0.1:50001"
max_clients = 32

[metrics]
enabled = true
listen = "127.0.0.1:9184"

[rawtx]
enabled = true
cache_mib = 2048
decoys = 3
tor_socks = "127.0.0.1:9050"
allow_clearnet = false
use_p2p = true
use_esplora = true
p2p_v2 = true
esplora_urls = []             # empty = per-network defaults

[silent_payments]
enabled = false

[safety]
allow_mainnet = false
```

Per-network Esplora defaults:
- mainnet: `https://blockstream.info/api`, `https://mempool.space/api` and the
  mempool.space onion endpoint;
- signet: `https://mempool.space/signet/api`;
- testnet4: `https://mempool.space/testnet4/api`;
- regtest: none.

## Commands

| Command | Description |
|---|---|
| `e-lite init` | Validate network/genesis against Core and create the database |
| `e-lite doctor` | Check config, Core, prune, disk, ports and DB integrity |
| `e-lite index` | Index the active chain (`--no-follow` to stop at tip) |
| `e-lite serve` | Serve queries from an indexed database |
| `e-lite run` | Index and serve; waits for Core and auto-initializes (recommended) |
| `e-lite status [--json]` | Health summary (HTTP if running, DB if stopped) |
| `e-lite core status` | Show the Core supervisor state and RPC reachability |
| `e-lite core logs` | Print the last 50 lines of a Core launched by e-lite |
| `e-lite export-snapshot` | Print the tip and canonical digest, or write a backup |
| `e-lite import-snapshot` | Restore a backup (target must be empty) |
| `e-lite version` | Print version |

Useful flags: `--config`, `--network`, `--data-dir`, `--core-rpc`,
`--rpc-user`, `--rpc-password`, `--core-datadir`, `--blocks-dir`,
`--block-source`, `--cookie-file`, `--manage`, `--prune-mib`,
`--min-free-gib`, `--listen`, `--metrics`, `--batch`, `--reorg-window`,
`--allow-mainnet`, `--allow-clearnet`, `--no-rawtx`, `--no-follow`.

## Monitoring

- `GET http://127.0.0.1:9184/status` — JSON with index height, Core height,
  `retention_blocks`, `pruneheight`, `network_active`, disk, clients and rawtx.
- `GET http://127.0.0.1:9184/metrics` — Prometheus text.
- `GET http://127.0.0.1:9184/healthz` — `ok`.
- Logs to stderr; `SIGTERM` stops cleanly at a commit boundary.

States: `WAITING_FOR_CORE`, `INITIALIZING`, `BUILDING_INDEX`, `READY`,
`DEGRADED_RAWTX`, `CORE_UNAVAILABLE`, `PAUSE_DOWNLOAD`, `PRUNING`,
`REORG_RECOVERY`, `FATAL_INTEGRITY`. Queries return an explicit temporary error
while building; they never return a misleading empty result.

## Electrum methods (1.4)

`server.version`, `server.features`, `server.ping`, `server.banner`,
`server.donation_address`, `server.peers.subscribe`,
`blockchain.headers.subscribe`, `blockchain.block.header`,
`blockchain.block.headers`, `blockchain.scripthash.get_history`,
`get_balance`, `listunspent`, `subscribe`/`unsubscribe`,
`blockchain.transaction.get`, `get_merkle`, `id_from_pos`,
`blockchain.scripthash.get_mempool`, `mempool.get_fee_histogram`,
`blockchain.relayfee`, `blockchain.estimatefee`, `blockchain.block.stats`,
`blockchain.transaction.broadcast`.

`blockchain.block.header`/`block.headers` support `cp_height` checkpoint proofs
with a cached header chain. History is capped at 100,000 entries per script.

## Bulk block ingest (local Core)

When Core runs on the same machine, e-lite reads raw blocks directly from
Core's `blk*.dat` files instead of asking RPC for hex. This is the fast path.

```toml
[index]
block_source = "auto"    # auto | rpc | files
readers = 0              # 0 = auto
```

- `auto` uses the block files when they are readable and falls back to RPC.
- `files` requires them; it fails at startup if the directory is unusable.
- `rpc` keeps the RPC-only behaviour.

The reader is **read-only**. It builds a locator sidecar
(`<data-dir>/blockfiles.idx`) with `hash -> (file, offset, size)`, handles Core's
default `-blocksxor` obfuscation (8-byte key in `blocks/xor.dat`, XORed at the
absolute file offset), and verifies every block hash and Merkle root before it is
applied. Core remains the authority for the active chain: heights are always
resolved with `getblockhash`.

On a local regtest the ingest path exceeds 20,000 blocks/s for small blocks.

## Pruned Core and external raw transactions

Once the index exists, e-lite can keep running against a pruned Core:

- History, balance, UTXOs, headers and Merkle proofs come from the local index.
- `transaction.get` resolves raw data in this order:
  1. local Core block (zero external requests),
  2. validated on-disk cache,
  3. **whole-block** fetch from P2P peers and/or Esplora-compatible endpoints,
  4. verify header hash, Merkle root, TXID and witness commitment, then cache.
- External traffic goes through **Tor** (`tor_socks`, stream isolation). Without
  Tor it fails closed unless `allow_clearnet = true`.
- Each external fetch is padded with `decoys` additional blocks, and requests
  never mention a TXID.
- `transaction.get(..., true)` resolves the raw transaction through the same
  resolver and asks Core to decode it, so it also works on a pruned node.

## Mempool, fees and notifications

- The mempool overlay is rebuilt from Core (`getrawmempool` plus batched raw
  transaction fetches), refreshed every 3 s and immediately after each block.
- The overlay is built outside the write lock, so history/balance queries are not
  blocked by a slow Core RPC.
- Notifications use a dirty-set: the indexer marks the scripts touched by each
  confirmed block and the mempool marks additions/removals/height changes, so a
  cycle only recomputes the affected subscriptions. Reorgs force a full refresh.
- `scripthash.get_history` appends mempool entries (`height` 0, or `-1` for
  chained transactions); `get_mempool`, `listunspent` and status subscriptions
  include them too.
- `get_balance` follows Electrum semantics: `confirmed` ignores mempool events
  and `unconfirmed` carries the net delta.
- Reorganisations are handled both ways: a longer Core chain and a shorter one
  (`invalidateblock`) rewind the index to the common ancestor; deep reorgs beyond
  `reorg_window` stop with `NEEDS_RECOVERY`.

## Backup and restore

Stop e-lite first: RocksDB holds an exclusive lock.

```bash
e-lite export-snapshot --config e-lite.toml --output e-lite-backup.tar.gz
# restore into a new, empty data directory (can be another machine)
e-lite import-snapshot --config e-lite.toml --input e-lite-backup.tar.gz \
  --data-dir /var/lib/e-lite/restored
```

The archive contains the database plus a manifest with network, genesis, tip,
txnum and the canonical digest. Import verifies the network and recomputes the
digest; it refuses a non-empty target or a mismatched snapshot. The rawtx cache
is not included (it is rebuildable).

## Starting without Core

The database identity is pinned at `init` (or at the first successful `run`).
If Core is down when e-lite starts, `run` still binds the Electrum and metrics
ports and serves confirmed history/balances from the local index. The Core
cookie is loaded lazily (Core removes it on shutdown and writes a new one on
start), so when Core comes back the indexer re-verifies the chain and resumes
without a restart.

If the database is empty and Core is down, e-lite stays in `WAITING_FOR_CORE`
and retries. With `manage = "auto"`, it starts Core as soon as `bitcoind` is
available.

## Disk expectations

- Core with `-prune=550` is **not** 550 MiB total: the chainstate is still
  ~10–15 GB on mainnet, plus ~550 MiB of blocks and the current block file.
- e-lite stores full history. On mainnet this can be hundreds of GB
  (`tx_occurrences`, `history`, `txids.dat`, UTXO set). This is the cost of
  serving an Electrum server without a full node.
- For production, use separate disks for Core (small, pruned) and the e-lite
  index (large), and keep `min_free_gib` above the actual free space you want
  to preserve.
- The disk guard reads both filesystems **before Core RPC** and the indexer
  checks again while applying blocks. If either disk falls below
  `min_free_gib`, indexing stops and Core downloads are paused; the current
  uncommitted group is discarded and re-indexed after space is freed. A full
  disk must not be allowed to reach Core: if Core prunes past the committed
  index, the gap is unrecoverable and requires a snapshot or a full node.
- A full signet/testnet4 sync needs enough free space for the Core datadir
  (pruned) plus the index. A 2 GiB threshold on a nearly full disk is not
  enough for a multi-GB index: leave room or use a larger disk.
- A from-scratch mainnet index is I/O-bound. For mainnet production, restoring a
  snapshot and then following the tip is the practical path.

## Limitations (v0.1)

- Pruned IBD from scratch is local-only and requires a Core owned by e-lite.
- Remote Core is for maintenance, not for the first pruned IBD.
- The index must be built with a full node, a local pruned Core started by
  e-lite, or restored from a snapshot.
- Raw transactions require the block to still exist in Core or in a configured
  external source; otherwise the error is explicit, never invented data.
- External rawtx requires Tor by default, so a missing Tor daemon degrades
  `transaction.get` for pruned blocks (history/balances still work).
- A deep reorganization beyond `reorg_window` stops with `NEEDS_RECOVERY`.
- No systemd unit, no aarch64 build, no testnet3.

## Build from source

```bash
cargo build --release -p e-lite
cargo test --workspace
```

Requires Rust (stable), a C++ toolchain and libclang (RocksDB bindgen).

## End-to-end tests

```bash
tests/e2e_regtest.sh            # Electrum, mempool, reorg, snapshot
tests/e2e_resilience.sh         # Core outage, offline serve, cookie rotation
tests/e2e_launch_core.sh        # e-lite launches Core from scratch (prune=550)
tests/e2e_wait_core.sh          # e-lite waits for Core, then auto-initializes
tests/e2e_external_pruned.sh    # rejects an external pruned Core safely
tests/e2e_pruned_ibd.sh         # local manual-prune IBD + prune gate

# Signet/testnet4 (real network sync, opt-in; needs >=15/20 GiB free)
E_LITE_NETWORK=signet tests/e2e_networks.sh
E_LITE_NETWORK=testnet4 E_LITE_CORE_DATADIR=/srv/core-t4 tests/e2e_networks.sh
# On a smaller disk, override the preflight at your own risk:
E_LITE_E2E_REQUIRED_FREE_GIB=0 E_LITE_E2E_MIN_FREE_GIB=1 tests/e2e_networks.sh
```

## Reproducible Debian package

```bash
scripts/package.sh          # builds inside debian:12 via docker, writes dist/
```

## License

MIT. See [LICENSE](LICENSE).
