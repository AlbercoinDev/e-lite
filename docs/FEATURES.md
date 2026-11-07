# Current functionality

Status values: **stable** (tested and supported), **experimental** (works but
not production-hardened), **gate** (explicit error, not implemented), **not
available**.

## Core and ingest

| Feature | Status | Notes |
|---|---|---|
| Networks: regtest, signet, testnet4, mainnet | experimental | mainnet requires explicit opt-in |
| Core `external`, `auto`, `managed` | experimental | only owned Core can be paused/pruned/stopped |
| Local Core, LAN Core, WAN via SSH/VPN | experimental | RPC-only for LAN/WAN; no remote block files |
| Core RPC exposure guard | experimental | warns on non-loopback; `require_loopback_rpc` makes it fatal; no native TLS |
| Block source `files`, `rpc`, `auto` | experimental | files reads `blk*.dat`, XOR aware, read-only |
| From-zero pruned IBD with `prune_mib=1` | experimental | manual prune gate; default 10k safe-block gain |
| Full node `prune_mib=0` | experimental | no pruning |
| Automatic pruning `>=550` + opt-in | experimental | hard lag cap + verified gap repair |
| Parallel decode/txid/Merkle | experimental | readers prepare blocks; apply does UTXO/batch |
| Commit sync policy | experimental | `auto` (txids in bulk, strict live), `always`, `txids` |
| Prune cadence and deferral | experimental | every 10k safe blocks; deferred above 50 GiB |
| Core bulk pause + peer warm-up | experimental | pause during bulk; resume 5k blocks early |
| Disk guard with download/index split | experimental | cgroup/LXC aware |
| Adaptive profiles and hot reload | experimental | `auto/hdd/ssd/nvme/manual`, `SIGUSR1` |
| Global RocksDB memory budget | experimental | `RAM/8` memtables inside a 75 % ceiling; adaptive cache; CPU-capped jobs |
| Reorg recovery within undo window | experimental | deeper reorg fails clearly |
| Snapshots export/import | experimental | optional; not required from zero |

## Index and Electrum 1.4–1.7

| Feature | Status | Notes |
|---|---|---|
| Headers and header subscription | experimental | checkpoint proof support |
| Script history, balance, UTXOs | experimental | history capped at 100k entries/script |
| Scripthash subscribe/unsubscribe | experimental | dirty-set notifications |
| Mempool history/UTXOs/fees | experimental | overlay from Core; fee histogram |
| Merkle proofs, tx id-from-pos | experimental | local index |
| Block stats | experimental | local |
| Broadcast | experimental | via Core |
| Raw transaction get | experimental | local Core -> cache -> P2P/Esplora |
| Electrum 1.4–1.7 negotiation | experimental | negotiates 1.4–1.7 numerically; 1.6 clients get `headers` arrays; 1.7 adds scriptpubkey/outpoint/ping shapes; future/skipped versions fall back to the newest implemented version below |
| Broadcast package | experimental | Core `submitpackage`, stable success/errors result; protocol 1.6 |
| Outpoint status/subscriptions | experimental | protocol 1.7; on-demand history scan, no extra index writes |
| Scriptpubkey methods | experimental | protocol 1.7; scripthash logic with dict results and `10001` |
| TLS Electrum | experimental | opt-in `tls_listen`; PEM cert/key; reload on change; no client auth |
| Rate limiting | experimental | per-client token bucket; auto for non-loopback; no client auth |
| Silent payments BIP352 | **gate** | explicit error; no indexer |

### Electrum methods currently implemented

`server.version`, `server.features`, `server.ping`, `server.banner`,
`server.donation_address`, `server.peers.subscribe`, `server.add_peer`,
`blockchain.headers.subscribe`, `blockchain.block.header`,
`blockchain.block.headers`, `blockchain.scripthash.get_history`,
`blockchain.scripthash.get_balance`, `blockchain.scripthash.listunspent`,
`blockchain.scripthash.subscribe`, `blockchain.scripthash.unsubscribe`,
`blockchain.scripthash.get_mempool`, `mempool.get_fee_histogram`,
`mempool.get_info`, `mempool.recent`, `blockchain.relayfee`, `blockchain.estimatefee`,
`blockchain.block.stats`, `blockchain.transaction.get`,
`blockchain.transaction.get_merkle`, `blockchain.transaction.id_from_pos`,
`blockchain.transaction.broadcast`,
`blockchain.transaction.broadcast_package` (protocol 1.6),
`blockchain.transaction.testmempoolaccept`,
`blockchain.scriptpubkey.*` (protocol 1.7),
`blockchain.outpoint.*` (protocol 1.7),
`blockchain.silentpayments.subscribe`/`unsubscribe` (explicit gate error).

## Privacy and external data

| Feature | Status | Notes |
|---|---|---|
| Local-first rawtx | experimental | Core/cache before external |
| Whole-block external fetch | experimental | by hash, never txid |
| Tor SOCKS5h with clearnet fallback | experimental | Tor first; `tor_fallback_clearnet` (default true) uses clearnet if Tor fails; `allow_clearnet` forces it |
| Managed Tor + onion service | experimental | plain 50001 + TLS 50002; `e-lite onion show/reset` |
| Auto self-signed onion TLS cert | experimental | SAN includes the onion; reloaded on change |
| Decoys and random source order | experimental | `decoys=1` default; max 2 in flight; prefetch spawns none |
| Rawtx cache | experimental | 256 MiB default; `cache_dir` can point at tmpfs to avoid disk |
| Electrum rate limit | experimental | 200 req/s, burst 5000 for every client; over-budget requests wait, never error; `0` disables |
| Block hash/header/Merkle/witness validation | experimental | before serving/caching |
| Configurable Esplora endpoints | experimental | editable `public-servers.toml`; shipped defaults; fail-closed if no provider and `require_ready` |
| Provider readiness gate | experimental | probes `/block-height/0` + genesis raw block before opening Electrum; `require_ready`, latency/state in `/status` |
| Provider health cooldown | experimental | failed Tor/direct provider skipped for `provider_cooldown_seconds` (300 s); last good first and cooled for max 30 s; forced attempt when all are cooling |
| In-flight block dedup | experimental | concurrent tx/prefetch of the same block share one download; `max_parallel_fetches=4` |
| History block prefetch | experimental | background fetch of confirmed history blocks before `transaction.get`; 1 worker, 16 heights per history, bounded queue, no decoys |
| P2P peers auto/config | experimental | `MSG_WITNESS_BLOCK`, `NODE_WITNESS` required, connected peers first, last-good peer first, failed-peer cooldown, total timeout; BIP324 v2 + v1 |
| Prune-gap repair from verified sources | experimental | bounded by `repair_max_gap_blocks` |
| Silent payments privacy | **not available** | gate only |

## Operations and packaging

| Feature | Status | Notes |
|---|---|---|
| `/status`, `/metrics`, `/healthz` | experimental | loopback by default; memory and TLS counters |
| Application log `e-lite.log` | experimental | tee of console; 16 MiB rotation; `--log-file`/`--no-log-file` |
| Disk breakdown | experimental | `/status.disk`: index, txids, rawtx cache, log and snapshot bytes |
| Wallet banner | experimental | built-in INDEXING/READY + template file |
| Adaptive memory budget | experimental | hot cache grow/shrink; 75 % ceiling; reserve |
| READY / FULLY SYNCED status | experimental | log, `/status`, `/metrics`, `e-lite status` |
| Ordered startup/shutdown | experimental | `run` waits for Core; threads joined before Core stop |
| CLI overrides and `--set` | stable | every TOML field; see `docs/CLI.md` |
| Progress percentages and milestones | experimental | height/Core %, 10k blocks or 6 h; disk usage |
| One-line install/update | stable | installer verifies SHA256SUMS; Tor bundle by default |
| Optional official Core install/update | stable | `--with-core` |
| Optional Tor expert bundle | stable | `--with-tor` default; `--without-tor` |
| `.deb`, tarball, release checksums | stable | GitHub Releases |
| All-in-one test harness | experimental | `scripts/e-lite-all-in-one.sh` |
| Systemd unit / container image | **not available** | roadmap |

## Compatibility

| Item | Status |
|---|---|
| Debian 12+/LMDE 6+, x86_64 | supported |
| Bitcoin Core 28+ | supported; tested with 32.x |
| Electrum protocol 1.4–1.7 | supported |
| Electrum/Sparrow wallets | tested on regtest; broader matrix pending |
| ARM64 | not published |
| Windows/macOS | not supported |
