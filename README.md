# e-lite

**Electrum 1.4 server for Bitcoin Core.** Query the history, balance and UTXOs of
any `scriptPubKey` in the active chain — **without registering wallets**.

`e-lite` indexes the chain from Bitcoin Core and serves Electrum JSON-RPC on
loopback. Core remains the authority for consensus and the active chain; e-lite
never invents data.

> **Status: `v0.1.0-alpha.6` — experimental, no funds.**
> Use on test networks first.

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
`CORE_UNAVAILABLE`, `REORG_RECOVERY`, `FATAL_INTEGRITY`. While building, queries
return an explicit temporary error — never a misleading empty result. When Core
is temporarily unreachable, confirmed history/balances stay available and the
index resumes with backoff when Core returns (the cookie is reloaded if it
rotated).

## Electrum methods (1.4)

`server.version`, `server.features`, `server.ping`, `server.banner`,
`server.donation_address`, `server.peers.subscribe`,
`blockchain.headers.subscribe`, `blockchain.block.header`,
`blockchain.block.headers`, `blockchain.scripthash.get_history`,
`get_balance`, `listunspent`, `subscribe`/`unsubscribe` (status only, no push),
`blockchain.transaction.get`, `get_merkle`, `id_from_pos`.

## Pruned Core and external raw transactions

The index itself must be built from a full node or restored from a snapshot. Once
built, e-lite can keep running against a **pruned** Core:

- History, balance, UTXOs, headers and Merkle proofs come from the local index.
- `transaction.get` resolves raw data in this order:
  1. local Core block (zero external requests),
  2. validated on-disk cache (10 GiB, LRU),
  3. **whole-block** fetch from P2P peers and/or Esplora-compatible endpoints,
  4. verify header hash, Merkle root, TXID and witness commitment, then cache.
- External traffic goes through **Tor** (`tor_socks`, stream isolation). Without
  Tor it fails closed unless `allow_clearnet = true`.
- Each external fetch is padded with `decoys = 3` additional blocks, and requests
  never mention a TXID.
- If no source can supply a block, the client gets an explicit error; history and
  balances remain available (`DEGRADED_RAWTX`).

```toml
[rawtx]
cache_gib = 10
decoys = 3
tor_socks = "127.0.0.1:9050"
allow_clearnet = false
use_p2p = true
use_esplora = true
p2p_peers = []          # empty: discovered from the local Core (getnodeaddresses)
p2p_timeout_seconds = 20
p2p_v2 = true            # BIP324 encrypted transport, fallback to v1
esplora_urls = [
  "http://mempoolhqx4isw62xs7abwphsq7ldayuidyx2v2oethdhhj6mlo2r6ad.onion/api",
  "https://blockstream.info/api",
]
```

Pruning becomes a problem only if Core prunes blocks the index has not reached
yet. `doctor` reports that and refuses to index a gap.

## Mempool, fees and notifications

- The mempool overlay is rebuilt from Core (`getrawmempool` plus batched raw
  transaction fetches), refreshed every 3 s and immediately after each block.
- `scripthash.get_mempool`, unconfirmed balances and `listunspent` include
  unconfirmed transactions; status subscriptions notify clients when they change.
- `blockchain.estimatefee`, `blockchain.relayfee`, `mempool.get_fee_histogram`,
  `blockchain.block.stats`, verbose `transaction.get` and
  `blockchain.transaction.broadcast` are implemented.
- `blockchain.headers.subscribe` and `blockchain.scripthash.subscribe` push
  notifications to connected clients.
- `estimatefee` never returns `-1`: it falls back to `max(mempoolminfee,
  relayfee)` so wallets can still compose transactions.

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

## Clean shutdown

`SIGTERM`/`SIGINT` stop the index at a commit boundary, flush the current group
and exit with code 0. No partial checkpoint is published.

## Limitations (v0.1)

- `blockchain.silentpayments.*` is not implemented (normal wallets are
  unaffected).
- Mode B (`pruned-IBD` building the index while Core syncs with `prune=1`) is
  not implemented; build the index with a full node or restore a snapshot.
- Real Sparrow/Electrum validation on Signet is still pending operator testing;
  the automated suite reproduces Sparrow's connect and wallet flow.
- The index must be built with a full node (or restored from a snapshot); serving
  against a pruned Core is supported.
- Raw transactions require the block to still exist in Core or in a configured
  external source; otherwise the error is explicit, never invented data.
- P2P negotiates **BIP324 v2** and falls back to v1 for peers that do not
  support it.
- External rawtx requires Tor by default, so a missing Tor daemon degrades
  `transaction.get` for pruned blocks (history/balances still work).
- The first mempool load on a large mempool can take a few seconds.
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
