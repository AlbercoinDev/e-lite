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
| Core P2P networks | experimental | `[core] network = auto` (clearnet + onion via the managed Tor, + I2P/CJDNS when available), `private` (only those networks, fail-closed) or `clearnet`; opt-in `private_broadcast` (`-privatebroadcast`, needs Core 30+) |
| ZFS ARC management without root | experimental | `e-lite arc-setup` prints (or applies with `--apply` as root) the group-writable `zfs_arc_max` + systemd oneshot; the installer adds it on ZFS hosts; a failed permission probe is retried every 10 min |
| Core RPC exposure guard | experimental | warns on non-loopback; `require_loopback_rpc` makes it fatal; no native TLS |
| Block source `files`, `rpc`, `auto` | experimental | files reads `blk*.dat`, XOR aware, read-only |
| From-zero pruned IBD with `prune_mib=1` | experimental | manual prune gate; default 5k safe-block gain |
| Full node `prune_mib=0` | experimental | no pruning |
| Automatic pruning `>=550` + opt-in | experimental | hard lag cap + verified gap repair |
| Parallel decode/txid/Merkle | experimental | readers prepare blocks; apply does UTXO/batch |
| Commit sync policy | experimental | `auto` (txids in bulk, strict live), `always`, `txids` |
| Per-network defaults | experimental | plain/TLS/metrics ports and `<data-dir>/bitcoin` per chain; multiple instances |
| Prune cadence and deferral | experimental | every 5k safe blocks; 20 GiB free-space deferral |
| Core bulk pause + peer warm-up | experimental | pause above 30 GiB backlog; resume at 7 GiB; legacy 5k-block fallback |
| Overlap/alternate scheduler | experimental | `overlap` default; `alternate` opt-in (Core and e-lite take turns) |
| Disk guard with download/index split | experimental | cgroup/LXC aware |
| Adaptive profiles and hot reload | experimental | `auto/hdd/ssd/nvme/manual`, `SIGUSR1` |
| Global RocksDB memory budget | experimental | effective `MemAvailable` budget; memtables `effective/16`; opt-in adaptive memtables; adaptive cache; CPU-capped jobs |
| Reorg recovery within undo window | experimental | deeper reorg fails clearly |
| Snapshots export/import | experimental | optional; not required from zero |

## Index and Electrum 1.4–1.7

| Feature | Status | Notes |
|---|---|---|
| Headers and header subscription | experimental | checkpoint proof support |
| Script history, balance, UTXOs | experimental | history capped at 100k entries/script |
| Scripthash subscribe/unsubscribe | experimental | dirty-set notifications |
| Mempool history/UTXOs/fees | experimental | overlay from Core (txid-list polling, only new transactions fetched); fee histogram |
| Merkle proofs, tx id-from-pos | experimental | local index |
| Block stats | experimental | local |
| Broadcast | experimental | via Core |
| Raw transaction get | experimental | local Core -> cache -> P2P/Esplora |
| Electrum 1.4–1.7 negotiation | experimental | negotiates 1.4–1.7 numerically; 1.6 clients get `headers` arrays; 1.7 adds scriptpubkey/outpoint/ping shapes; future/skipped versions fall back to the newest implemented version below |
| Broadcast package | experimental | Core `submitpackage`, stable success/errors result; protocol 1.6 |
| Outpoint status/subscriptions | experimental | protocol 1.7; on-demand history scan, no extra index writes |
| Scriptpubkey methods | experimental | protocol 1.7; scripthash logic with dict results and `10001` |
| TLS Electrum | experimental | on by default (`server.tls`); auto self-signed cert or operator PEM; reload on change; no client auth |
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
| Tor SOCKS5h, fail-closed | experimental | Tor only by default; opt-in `tor_fallback_clearnet` uses clearnet if Tor fails; `allow_clearnet` forces it |
| Managed Tor + onion service | experimental | network plain/TLS ports; `e-lite onion show/reset` |
| Auto self-signed TLS cert | experimental | generated without Tor; SAN includes localhost, hostname and onion; reloaded on change |
| Decoys and random source order | experimental | `decoys=1` default; max 2 in flight; prefetch spawns none |
| Rawtx cache | experimental | 256 MiB default; verified transactions with merkle branch (whole blocks only for prune repair, max 64 MiB); last 8 external blocks in memory; `cache_dir` can point at tmpfs to avoid disk |
| Electrum rate limit | experimental | 200 req/s, burst 5000 for every client; over-budget requests wait, never error; `0` disables |
| Block hash/header/Merkle/witness validation | experimental | before serving/caching |
| Configurable Esplora endpoints | experimental | editable `public-servers.toml`; shipped defaults; fail-closed if no provider and `require_ready` |
| Provider readiness gate | experimental | probes `/block-height/0` + genesis raw block before opening Electrum when Core is pruned (an unpruned Core skips it); `require_ready`, opt-in `open_without_provider` after `ready_timeout_seconds`, 60 s re-probe, latency/state in `/status` |
| Provider health cooldown | experimental | failed Tor/direct provider skipped for `provider_cooldown_seconds` (300 s); random order among healthy providers (none preferred), a provider that answered before is cooled for max 30 s; forced attempt when all are cooling |
| In-flight block dedup | experimental | concurrent tx/prefetch of the same block share one download; `max_parallel_fetches=4` |
| History block prefetch | experimental, off by default (`rawtx.prefetch`) | background fetch of confirmed history blocks before `transaction.get`; 1 worker, 16 heights per history, bounded queue, no decoys |
| P2P peers auto/config | experimental | `MSG_WITNESS_BLOCK`, `NODE_WITNESS` required, random order among discovered peers, failed-peer cooldown, total timeout; BIP324 v2 + v1 |
| Prune-gap repair from verified sources | experimental | bounded by `repair_max_gap_blocks` |
| Silent payments privacy | **not available** | gate only |

## Operations and packaging

| Feature | Status | Notes |
|---|---|---|
| `/status`, `/metrics`, `/healthz` | experimental | loopback by default; memory and TLS counters |
| Application log `e-lite.log` | experimental | tee of console; 16 MiB rotation; `--log-file`/`--no-log-file` |
| Disk breakdown | experimental | `/status.disk`: index, txids, rawtx cache, log and snapshot bytes |
| Wallet banner | experimental | built-in INDEXING/READY + template file |
| Adaptive memory budget | experimental | fitted to available RAM; hot cache ceiling and opt-in hot memtables; 75 % ceiling; reserve |
| READY / FULLY SYNCED status | experimental | log, `/status`, `/metrics`, `e-lite status` |
| Ordered startup/shutdown | experimental | `run` waits for Core; threads joined before Core stop |
| Snapshot export (index only) | experimental | `export-snapshot --output`: manifest format 3 (network/genesis/tip/schema/digest; manifest first, index stored, formats 1-2 still import); the nested Core datadir is excluded |
| Snapshot export (index + Core) | experimental | `--with-core`: probes Core (start + RPC + stop), bundles its chain data only (`blocks/`, `chainstate/`, `indexes/`; wallets, keys and config are never copied) with tip/prune/version in the manifest; refuses while Core runs; SHA-256 per file by default (`--no-hash-files`); gzip index 0 / Core 3 (`--compress-level` sets both) |
| Snapshot import + verification | experimental | `import-snapshot`: empty target, staged extraction renamed into place only after every check, optional `--expect-digest`, digest/tip/genesis validation, Core coherence (`index tip <= Core tip`, 288-block prune margin); `--verify` checks file hashes; `--verify=full` opens the restored Core and runs a bounded `verifychain` |
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
