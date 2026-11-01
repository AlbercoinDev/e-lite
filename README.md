# e-lite

**Electrum 1.4 server for Bitcoin Core.** Query the history, balance and UTXOs of
any `scriptPubKey` in the active chain — **without registering wallets**.

`e-lite` indexes the chain from Bitcoin Core and serves Electrum JSON-RPC on
loopback. Core remains the authority for consensus and the active chain; e-lite
never invents data.

> **Status: `v0.1.0-alpha.1` — experimental, no funds.**
> Confirmed chain only: mempool, notifications push and `broadcast` are not
> implemented yet. Use on test networks first.

## Supported networks

`regtest` · `signet` · `testnet4` · `mainnet` (explicit opt-in).
Testnet3 is intentionally not supported.

## Requirements

- Linux **Debian 12+ / LMDE 6+**, x86_64, glibc ≥ 2.36.
- Bitcoin Core (official binary from bitcoincore.org). `txindex` is **not** needed.
- Core must run with `prune=0` in v0.1 (full-core mode).
- The index lives in its own directory; it never writes to the Core datadir.

## Install (one line, also updates)

```bash
curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh | sh
```

The installer needs `curl` and `ca-certificates` (present on standard Debian
desktops). The same command installs or **updates** e-lite (atomic binary replacement,
config and database untouched) and **asks** whether to install/update the latest
official Bitcoin Core. Without a terminal it defaults to *no*.

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

## Quickstart (regtest)

```bash
bitcoind -regtest -datadir=/tmp/e-core -daemon -server -listen=0 -fallbackfee=0.0002
bitcoin-cli -regtest -datadir=/tmp/e-core createwallet demo
ADDR=$(bitcoin-cli -regtest -datadir=/tmp/e-core -rpcwallet=demo getnewaddress)
bitcoin-cli -regtest -datadir=/tmp/e-core -rpcwallet=demo generatetoaddress 101 "$ADDR"

e-lite init  --network regtest --core-datadir /tmp/e-core --data-dir /tmp/e-index
e-lite doctor --network regtest --core-datadir /tmp/e-core --data-dir /tmp/e-index
e-lite run   --network regtest --core-datadir /tmp/e-core --data-dir /tmp/e-index
```

Connect Electrum or Sparrow to `127.0.0.1:50001` (no SSL).

## Signet / testnet4 / mainnet

```bash
bitcoind -signet -datadir=/srv/core-signet           # RPC 38332
bitcoind -chain=testnet4 -datadir=/srv/core-t4       # RPC 48332
bitcoind -datadir=/srv/core-mainnet                  # RPC 8332

e-lite run --network signet   --core-datadir /srv/core-signet --data-dir /srv/e-lite-signet
e-lite run --network testnet4 --core-datadir /srv/core-t4     --data-dir /srv/e-lite-t4
e-lite run --network mainnet  --core-datadir /srv/core-mainnet --data-dir /srv/e-lite-mainnet --allow-mainnet
```

`index` and `serve` open the same RocksDB, so they cannot run at the same time:
use `run` (index + serve in one process), or `index --no-follow` first and then
`serve`.

## Commands

| Command | Description |
|---|---|
| `e-lite init` | Validate network/genesis against Core, create the database |
| `e-lite doctor` | Check config, Core, prune, disk, ports and DB integrity |
| `e-lite index` | Index the active chain (`--no-follow` to stop at tip) |
| `e-lite serve` | Serve queries from an indexed database |
| `e-lite run` | Index and serve in one process (recommended) |
| `e-lite status [--json]` | Health summary (HTTP if running, DB if stopped) |
| `e-lite export-snapshot` | Print tip and canonical logical digest |
| `e-lite version` | Print version |

Useful flags: `--config`, `--network`, `--data-dir`, `--core-rpc`,
`--core-datadir`, `--cookie-file`, `--listen`, `--metrics`, `--batch`,
`--reorg-window`, `--allow-mainnet`, `--no-follow`.

## Monitoring

- `GET http://127.0.0.1:9184/status` — JSON (height, lag, txnum, size, disk, clients).
- `GET http://127.0.0.1:9184/metrics` — Prometheus text.
- `GET http://127.0.0.1:9184/healthz` — `ok`.
- Logs to stderr; `SIGTERM` stops cleanly at a commit boundary.

States reported by the server: `BUILDING_INDEX`, `READY`, `DEGRADED_RAWTX`,
`REORG_RECOVERY`, `FATAL_INTEGRITY`. While building, queries return an explicit
temporary error — never a misleading empty result.

## Electrum methods (1.4)

`server.version`, `server.features`, `server.ping`, `server.banner`,
`server.donation_address`, `server.peers.subscribe`,
`blockchain.headers.subscribe`, `blockchain.block.header`,
`blockchain.block.headers`, `blockchain.scripthash.get_history`,
`get_balance`, `listunspent`, `subscribe`/`unsubscribe` (status only, no push),
`blockchain.transaction.get`, `get_merkle`, `id_from_pos`.

## Limitations (v0.1)

- Confirmed chain only: no mempool overlay, no push notifications, no
  `broadcast`/`estimatefee`.
- `full-core` only (Core `prune=0`).
- Raw transactions require the block to still exist in Core; otherwise the error
  is explicit (`rawtx unavailable`), never invented data.
- Deep reorganizations beyond `reorg_window` stop with `NEEDS_RECOVERY`.
- No systemd unit and no aarch64 build.

## Build from source

```bash
cargo build --release -p e-lite
cargo test --workspace
```

Requires Rust (stable), a C++ toolchain and libclang (RocksDB bindgen).

## Reproducible Debian package

```bash
scripts/package.sh          # builds inside debian:12 via docker, writes dist/
```

## License

MIT. See [LICENSE](LICENSE).
