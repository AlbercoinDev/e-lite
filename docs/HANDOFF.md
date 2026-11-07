# Session handoff

**Updated:** 2026-11-07
**Latest published release:** `0.1.0-alpha.29.4.1`
**Release:** https://github.com/AlbercoinDev/e-lite/releases/tag/v0.1.0-alpha.29.4.1
**Main commit at handoff:** tag `v0.1.0-alpha.29.4.1`

This file is the short “where we are” for a new session. The invariants live in
[`AGENTS.md`](../AGENTS.md) and [`PROJECT_MEMORY.md`](PROJECT_MEMORY.md); the
current behaviour is defined by the code, `config.example.toml` and
`CHANGELOG.md`. If this handoff conflicts with a newer commit, the newer commit
wins; update this file.

## Read order for a new session

1. `AGENTS.md`
2. `docs/README.md`
3. `docs/PROJECT_MEMORY.md`
4. `docs/HANDOFF.md` (this file)
5. `docs/FEATURES.md` and `docs/ROADMAP.md`
6. `docs/AUDIT.md`
7. The specific docs for the task (`CLI`, `INSTALL`, `ARCHITECTURE`, `OPERATIONS`,
   `PRIVACY`, `TUNING`, `ELECTRUM_PROTOCOL`, `GETTING-STARTED`).

Suggested prompt:

```text
Trabaja en este repo. Lee AGENTS.md, docs/README.md,
docs/PROJECT_MEMORY.md, docs/HANDOFF.md, docs/AUDIT.md,
docs/FEATURES.md y docs/ROADMAP.md. Continúa desde el estado actual;
no repitas contexto ni rehagas decisiones ya documentadas.
```

## Current state

- From-zero pruned IBD uses `prune_mib = 1` (manual pruning). Core does not prune
  by itself; the prune gate keeps the cut below `index - 288`.
- `prune_mib >= 550` requires `[pruning] allow_auto_prune = true`.
- Snapshots are optional and are not needed for the from-zero flow.
- Electrum binds per-network defaults: plain/TLS/metrics are
  `55011/55022/55084` (mainnet), `55111/55122/55184` (testnet4),
  `55211/55222/55284` (signet) and `55311/55322/55384` (regtest), on `0.0.0.0`
  for Electrum. TLS is on by default with a generated self-signed certificate
  (`<data-dir>/tls/`) and does not depend on Tor; `tls = false` / `--no-tls`
  disables it. `[tor] mode = "managed"` (default) publishes the same plain/TLS
  ports as a v3 onion. Data defaults to `~/e-lite/<network>` and Core to
  `<data-dir>/bitcoin`, so one instance per chain can run at once. Wildcard
  listeners (`0.0.0.0`/`[::]`) are mapped to loopback onion targets. The
  installer-provisioned Tor bundle is launched with `LD_LIBRARY_PATH` pointing
  at its own libraries. There is no client authentication; treat the onion
  address as a secret. Per-client rate limiting is a throttle, not auth: 200
  req/s with burst 5000 for every client (LAN and public), over-budget requests
  wait for the next token instead of being rejected, and `[server] rate_limit =
  0` disables it. `run` probes the
  network providers and gates the wallet listeners until one answers a real
  genesis block request (`/block-height/0` plus `/block/<genesis>/raw` with a
  header-hash check); Tor failure falls back to clearnet
  (`tor_fallback_clearnet`).
- Electrum 1.4–1.7 is implemented and negotiated per connection: numeric
  `server.version`, verbose `transaction.get` with block context, notification
  suppression, real `scripthash.unsubscribe`, canonical mempool status order,
  1.6 headers arrays, `mempool.get_info`, `estimatefee(mode)`,
  `broadcast_package`, 1.7 `scriptpubkey.*`, `outpoint.*`,
  `testmempoolaccept`, `mempool.recent` and the new `server.ping`. A request for
  an unimplemented/skipped version falls back to the newest implemented version
  at or below it; 1.6+ requires `server.version` as the first message. Outpoints
  are resolved on demand from the existing history/UTXO data; there is no
  persistent spend index or schema change.
- Core RPC is HTTP; LAN/WAN requires SSH/VPN.
- Rawtx resolution is local Core -> validated cache -> P2P/Esplora (whole blocks,
  Tor-first via the managed Tor, decoys, verified).
- Adaptive profiles and `SIGUSR1` hot reload are implemented:
  `auto|hdd|ssd|nvme|manual`, readers capped at 4/8/16, cgroup memory aware,
  explicit readers/commit overrides locked. Every knob is also available via CLI
  and `--set` (see `docs/CLI.md`).
- Memory: reserve `max(1024 MiB, RAM/8)`, ceiling 75 % of RAM, memtables `RAM/8`,
  page-cache floor, cache grows +256 MiB / 30 s and shrinks 25 % under pressure;
  `cache_mib` is a maximum. Background jobs are capped by the CPU count.
- `run` waits for Core RPC before starting P2P/mempool/listeners; `serve` is the
  offline mode. Shutdown joins background threads and stops managed Tor before
  stopping Core.
- `docs/FEATURES.md` is the current-feature source of truth. `docs/TOR.md` covers
  the onion setup, reset and security.
- `docs/ROADMAP.md` contains future features/improvements. Do not present them as
  available.

## Last change (alpha.29.4.1)

- **Undo at the committed tip**: bulk catch-up keeps undo for the last
  `reorg_window + 64` blocks of each batch, not only near Core's tip; a deep
  Core rollback now reports `NEEDS_RECOVERY` explicitly.
- **TLS hardening**: LAN IP SANs, DER SHA-256 fingerprint, atomic `0600` writes,
  key reuse, corrupt-pair regeneration, `tls_auto_cert = false` accepts an
  existing pair, and operator PEM pairs are validated at startup.
- **Validation**: a half-configured `tls_cert`/`tls_key` fails instead of being
  completed silently; legacy Core datadirs require the chain's blocks directory.
- New `tests/e2e_recovery.sh` (SIGKILL, reorg at the tip, write failure);
  regtest, pruned-IBD, defaults, resilience and wait-Core e2e pass.

## Last change (alpha.29.4.0)

- **Per-network ports**: 55011/55022/55084 (mainnet), 55111/55122/55184
  (testnet4), 55211/55222/55284 (signet), 55311/55322/55384 (regtest).
- **TLS on by default** with an auto self-signed certificate that does not
  require Tor; `--no-tls` disables it. Operator PEMs are preserved.
- **Per-network data/Core layout**: `~/e-lite/<network>` and
  `<data-dir>/bitcoin`; a single legacy Core datadir is detected with a
  migration warning, two candidates are rejected as ambiguous.
- **`run` serves offline**: an existing index opens Electrum in
  `CORE_UNAVAILABLE` while the index loop retries Core; first runs still wait.
- New `tests/e2e_defaults.sh`; regtest, pruned-IBD, resilience and wait-Core
  e2e pass. `e2e_networks.sh` needs 15 GiB free and real signet/testnet4
  downloads; it was not completed on the development VM (3.9 GiB free).

## Last change (alpha.29.3.7)

- **Pruned-file locator cleanup**: sidecar locators for missing `blk*.dat` files
  are dropped at open and the sidecar is compacted before the scanner starts;
  after a prune the in-memory map is filtered and file descriptors are closed.
- **ReaderTracker**: rawtx guards are taken only for blocks Core can still serve
  (height >= pruneheight); external-only resolutions and prefetch no longer
  block pruning.
- **Disk-first gain**: `min_gain_blocks` defaults to 1,000 (≈1.5 GiB retained
  tail on mainnet, rounded to whole block files) instead of 10,000.
- Regtest and pruned-IBD e2e pass; no schema/wire change.

## Last change (alpha.29.3.6)

- **Durable prune**: RocksDB/WAL is flushed before the prune cut is authorised;
  failed prune RPCs retry at the same safe height after 30 s.
- **Confirmed pauses**: `setnetworkactive` must succeed before internal
  pause/resume state changes; disk pauses cannot be overridden by lag resumes.
- **Exact readiness**: `READY`/`FULLY SYNCED` require the exact tip; prune and
  resume paths recompute state.
- **Indexing before providers**: `run` starts the indexer immediately and gates
  only the wallet listeners; provider outages no longer stop block progress.
- **Bounded pipeline**: explicit prepared-block window; long catch-ups yield to
  the supervisor after the poll interval.
- **Space-first default**: `prune_defer_free_gib = 0`; 1k gain and 30 s
  interval still bound churn. Bulk reader cache is dropped after prune and disk
  checks include a custom `blocks_dir`.
- Regtest and pruned-IBD e2e pass.

## Last change (alpha.29.3.5)

- **Outpoint notifications** are resolved by a bounded worker pool (2 workers,
  queue 256, pending retry) instead of the notifier thread, so a Core/provider
  lookup cannot delay other wallets. Reorg forcing stays.
- Removed the temporary live provider test; deterministic cooldown tests remain.
- Added `docs/ELECTRUM_PROTOCOL.md` with the complete 1.4–1.7 method/version
  matrix, notification rules, limits, deviations and performance notes.
- No schema or wire-format change.

## Last change (alpha.29.3.4)

- **P2P BIP144 fix**: `getdata` requests `MSG_WITNESS_BLOCK` (0x40000002), peers
  must advertise `NODE_WITNESS`, `notfound` is immediate, and the witness
  commitment is validated inside the P2P source before a peer is marked good.
  This removes the repeated `missing witness reserved value` errors.
- **BIP141**: highest-index coinbase commitment and exactly one 32-byte
  reserved witness item.
- **Electrum 1.7**: scripthash and scriptpubkey subscriptions are separate;
  reorg notifications are forced for scriptpubkey and outpoint subscriptions.
- **Protocol hardening**: checked numeric parameters, `testmempoolaccept`
  `reason`, prefetch dedup by block hash with retry, and no prefetch side effect
  from status computation.
- **Cache/perf**: reads and fsync outside the metadata lock; capacity enforced
  at open. External fetch deadline 30 s; inflight wait 45 s.
- Tests updated; no schema change.

## Last change (alpha.29.3.3)

- **Rate limit uniform and non-blocking**: 200 req/s / burst 5000 for every
  client (no loopback/LAN/public distinction). Over-budget requests wait for
  the next token instead of returning `-32000`; a Sparrow gap scan no longer
  aborts at ~55 addresses. `rate_limit = 0` still disables.
- **Disk-minimal defaults for a max-pruned Core**: rawtx cache 256 MiB (was
  2048), optional `rawtx.cache_dir` for tmpfs (`/dev/shm/e-lite-rawtx`), log
  rotation 16 MiB (was 64). Snapshots remain manual/off by default.
- **Disk breakdown** in `/status.disk`: index, txids, cache, log and snapshot
  bytes, plus the effective cache directory and total.
- No schema or protocol change.

## Last change (alpha.29.3.2)

- **Never fail without a network attempt**: if every provider is cooling down,
  e-lite forces one attempt (last good first). A provider that already answered
  is cooled for at most 30 s after a failure, so one transient error cannot
  disable the only working source for the full 300 s.
- **Bounded SOCKS5 handshake**: P2P Tor uses a minimal SOCKS5 CONNECT with the
  P2P timeout on connect/auth/request/reply; the `socks` crate had no timeouts
  and could block indefinitely on a stalled proxy.
- **Inflight panic guard**: a panicking external fetch completes the shared
  entry with an error instead of leaving followers waiting.
- **Diagnostics**: `get_history` and rawtx failures log the scripthash/txid,
  height and block hash.
- Tests: forced attempt while cooling, failed-provider skip with a healthy
  alternative, SOCKS timeout and password handshake. No schema/protocol change.

## Last change (alpha.29.3.1)

- **Dead providers no longer stall every block**: direct Esplora failures are
  cooled down like Tor failures (`provider_cooldown_seconds`, 300 s), the last
  good provider is tried first, and `request_timeout_seconds` defaults to 8 s.
  This fixes the 17–34 s gaps caused by `mempool.space/signet/api` timing out
  on every block.
- **In-flight dedup and concurrency**: concurrent wallet/prefetch requests for
  one block share a download; `max_parallel_fetches` (4) caps external work.
- **P2P fail-fast**: `p2p_timeout_seconds` (8), current Core connections first,
  last-good peer first, failed-peer cooldown, total 2× timeout per block,
  `connect_timeout`, and a Tor circuit breaker before clearnet fallback.
- **Decoys/prefetch**: `decoys = 1` (max 2 in flight), prefetch has one worker,
  enqueues at most 16 heights and never spawns decoys.
- `/status`/`/metrics` expose P2P failures/skips, decoys in flight and prefetch
  queued/done. No schema or protocol change.

## Last change (alpha.29.3)

- **P2P witness fix**: the `version` message now advertises `NODE_WITNESS`
  (bit 3) in v1 and v2. Without it peers strip witnesses from served blocks;
  the block headers/merkle still look valid but BIP141 validation failed with
  `missing witness reserved value`, leaving wallets on `Retrieving
  transaction`. This was the real cause of the Sparrow stall, not wallet
  corruption.
- **P2P clearnet fallback** when Tor connect fails (`tor_fallback_clearnet`),
  one-time warning; `.onion` peers still require Tor.
- **Fast rawtx**: `rawtx.request_timeout_seconds` (15 s, was hard-coded 60) and
  `rawtx.provider_cooldown_seconds` (120 s) skip a provider's Tor path after a
  failure instead of waiting per transaction.
- **History prefetch**: serving `blockchain.scripthash.get_history` enqueues the
  confirmed heights in a bounded, deduplicated background queue (2 workers), so
  the following `transaction.get` usually hits the block cache. Optimisation
  only, disabled when no external sources exist.
- **Real provider probe**: the readiness gate now also fetches the genesis raw
  block and verifies its header hash; a provider that cannot serve real blocks
  does not open the wallet gate. Tor warning distinguishes total Tor failure
  from per-provider clearnet fallback.
- Tests: `NODE_WITNESS` payload bit, raw-block probe match/mismatch, Tor
  fallback state; workspace tests + regtest e2e green. No schema/protocol
  change.

## Last change (alpha.29.2)

- Readiness probes now validate `/block-height/0` against the **indexed
  network genesis** (`run`/`serve`) or the known chain genesis (`doctor`).
  A provider serving another network is marked unhealthy with a `genesis
  mismatch` error and cannot open the wallet gate.
- Tests cover the matching and mismatching genesis; no protocol/schema change.

## Last change (alpha.29.1)

- **Provider readiness gate**: `run` probes `public-servers.toml` for the active
  network and does not open Electrum until one answers `/block-height/0`
  (`require_ready`, 60 s budget). `serve` probes and warns but stays offline.
  This prevents Sparrow's `UNFETCHABLE_BLOCK_TRANSACTION` NPE caused by a
  history entry whose raw tx cannot be served.
- **Tor → clearnet fallback**: `tor_fallback_clearnet = true` by default; logs
  the IP exposure and shows it in `/status`. `.onion` still requires Tor;
  `false` restores fail-closed.
- `/status`, `/metrics` and `doctor` expose provider health/latency/via.
- Provider endpoints with two clients in `el-rawtx`; no schema/protocol changes.

## Last change (alpha.29)

- **Core RPC exposure guard**: warning on non-loopback `rpc_url`;
  `[core] require_loopback_rpc = true` makes it fatal; `doctor` prints the
  transport and `/status` exposes `core_rpc_loopback`.
- No native Core RPC TLS; the documented path is an SSH/VPN tunnel.
- Roadmap cleanup: rate limiting, Core RPC guard and UTXO prefetch are done;
  systemd stays optional.

## Last change (alpha.28)

- **Per-client rate limiting**: one token bucket per connection; 50 req/s with
  burst 300 on non-loopback listeners, disabled on loopback, `0` disables.
  Exhausted requests get `-32000 "rate limit exceeded; slow down"` and the
  connection stays open.
- `/status` exposes `rate_limit_rps`, `rate_burst`, `rate_limited`; `/metrics`
  adds `e_lite_rate_limited_total`.
- No client authentication: this is defense-in-depth; firewall/allowlist/VPN/Tor
  remain the access control. Systemd is explicitly optional in the roadmap.

## Last change (alpha.27.2)

- Verified the official mempool.space onion over Tor 0.4.9.15: block 9 raw bytes
  match the requested hash and are identical to clearnet. It is now enabled in
  the shipped `public-servers.toml`.
- The Blockstream API onion candidate did not resolve over Tor and remains
  disabled.
- Existing user files are never rewritten; only first-run/new installs get the
  enabled onion.

## Last change (alpha.27.1)

- Scriptpubkey notifications now use `blockchain.scriptpubkey.subscribe` (the
  params still carry the scripthash); scripthash subscribers are unchanged.
- The 1.7 e2e subscribes before mining and asserts both notification methods,
  and selects a UTXO belonging to the mining address so the `spk_hint` path is
  real.

## Last change (alpha.27)

- **Electrum 1.7** (AUD-043): `protocol_max = "1.7"`, `server.features` omits
  `hash_function` for 1.7, and `blockchain.block.headers` keeps the 1.6 array.
- `blockchain.scriptpubkey.*` (dict results, `10001`, reorg notification),
  `blockchain.outpoint.*` (on-demand history scan, `spk_hint` verification,
  1,000 subscriptions/client), `server.ping(pong_len, data)`,
  `mempool.recent`, `blockchain.transaction.testmempoolaccept`.
- No schema/CF/undo/digest change: the apply path and snapshots are untouched.
  The e2e asserts `rawtx.fetches == 0`, i.e. local data only.
- `listunspent` is sorted in blockchain order for both namespaces.

## Last change (alpha.26.1)

- **Public providers are file-based**: `public-servers.toml` next to
  `config.toml` is the only list; there is no compiled-in fallback. A first run
  copies the shipped template, existing files are never overwritten, and a
  missing file means local data only.
- Defaults: Blockstream + mempool.space for mainnet, signet and testnet4;
  mempool onion present but commented pending a real-Tor verification.
- Clearnet URLs must be HTTPS; `.onion` is the only plain-HTTP exception and
  still requires Tor.
- `/status` exposes the effective list; `doctor` prints the resolved file,
  source and endpoints. `--public-servers` and `[public_servers]` override the
  location/generation.
- Packages ship the template; `install.sh` copies it only when absent.

## Last change (alpha.26)

- **Electrum 1.6 compatibility** (AUD-042): `protocol_max = "1.6"`.
  `blockchain.block.headers` returns `headers: []` for 1.6 clients while 1.4
  clients keep `hex`; `server.version` must be the first message for 1.6 (a late
  negotiation is capped at 1.4.2).
- **`mempool.get_info`** exposes `mempoolminfee`, `minrelaytxfee` and
  `incrementalrelayfee`.
- **`blockchain.estimatefee(number, mode)`** passes `unset|economical|
  conservative` to Core and caches per `(target, mode)`.
- **`blockchain.transaction.broadcast_package`** wraps `submitpackage` with the
  stable `{success, errors}` result; `verbose=true` returns Core's object.
- **Tests**: real parent+child package in `tests/e2e_electrum16.py`, 1.6/1.4
  headers on the same instance, first-message cap, fee modes and mempool info.
- 1.7 remains unimplemented and is not advertised.

## Last change (alpha.25.4)

- **Lenient version negotiation** (AUD-041): the server now selects the newest
  implemented version at or below the client's maximum instead of disconnecting
  when the ranges do not intersect. `1.4.3`, `1.5`, `1.6` and `1.7` requests all
  negotiate `1.4.2` today; only a maximum below 1.4 is refused.
- `SERVER_VERSIONS` is the single source of truth; add a version there only
  after all of its methods are implemented and tested.
- Tests: selection matrix (including gaps and future versions), protocol
  fallback and the existing socket-close path for `< 1.4`.

## Last change (alpha.25.3)

- **Electrum 1.4 conformance** (AUD-039): numeric version negotiation
  (`1.4`–`1.4.2`), first `server.version` only, close on incompatible ranges,
  and `server.features` advertises the new range.
- **Verbose transactions**: `blockchain.transaction.get(txid, true)` on a
  confirmed transaction now includes `hex`, `blockhash`, `confirmations`,
  `time`, `blocktime` and `in_active_chain`; mempool responses carry
  `confirmations: 0`.
- **JSON-RPC notifications** are not answered; `scripthash.unsubscribe` returns
  the real boolean; `server.add_peer` returns `false`.
- **Mempool status order** is canonical `(-height, txid)` and deterministic.
- No schema, config or index changes. 1.6/1.7 remain unimplemented and are not
  advertised.

Evidence: `docs/AUDIT.md` (AUD-039), `CHANGELOG.md`, `cargo test --workspace`,
`tests/e2e_regtest.sh` (plain + TLS + Sparrow sequence with confirmed verbose
transaction).

## Last audit/fixes (alpha.25.2)

- **Onion ports mirror the listeners**: `[tor] onion_port`/`onion_tls_port`
  default to 0 = same as `server.listen`/`server.tls_listen`, so
  `--listen 0.0.0.0:51001 --tls-listen 0.0.0.0:51002` publishes `:51001` and
  `:51002` on the onion instead of 50001/50002. Explicit values still win; the
  startup log, torrc, state file, `/status` and `onion show` use the effective
  ports.
- Evidence: unit tests for listener mirroring/override; a manual `run` with
  51001/51002 showed the matching onion ports.

## Last audit/fixes (alpha.25.1)

- **Bundled Tor libraries**: the expert bundle's `tor` has no RPATH; on Debian
  13.7 without `libevent-2.1.so.7` it exited 127. e-lite now prepends the Tor
  binary directory to `LD_LIBRARY_PATH`, preserving any inherited value.
- **Diagnostics**: unexpected Tor exits print the last `tor.log` lines;
  `doctor` runs `tor --version` with the same environment.
- Evidence: unit test for the library path, the real-Tor integration test, and a
  Debian 12 minimal container reproducing the missing-libevent case (before:
  exit 127; after: `Tor version 0.4.9.15`).

## Last audit/fixes (alpha.25)

- **Onion with wildcard listeners**: `--listen 0.0.0.0:50001` /
  `--tls-listen 0.0.0.0:50002` now publish the onion with loopback targets
  (`127.0.0.1`) instead of refusing. `[tor] target_plain`/`target_tls` override
  the target; the effective ports/targets are saved in `<data-dir>/tor/state`
  so `e-lite onion show` is accurate even when the daemon used CLI flags.
- Evidence: unit tests for wildcard/IPv6 mapping and torrc targets; a manual
  `run` with `0.0.0.0` created the onion and `onion show` reported the custom
  ports; fmt/clippy/workspace tests.

## Last audit/fixes (alpha.24)

- **Adaptive memory budget**: 75 % ceiling, `max(1024, RAM/8)` reserve,
  memtables `RAM/8`, page-cache floor, 256 MiB growth / 25 % shrink with
  two-sample hysteresis; `cache_mib` is a maximum and critical pressure can
  lower it; pre-resume shrink when Core is about to download again.
- **Managed Tor + onion**: e-lite runs its own Tor by default, publishes plain
  50001 and TLS 50002 onion ports, auto-generates a self-signed TLS certificate
  with the onion SAN, and exposes `e-lite onion show|reset`.
- **Installer Tor bundle**: downloads the official Tor expert bundle (pinned
  version + SHA-256) into `<prefix>/lib/e-lite/tor`; `--without-tor` skips it and
  `tor` in PATH is the fallback. Indexing continues with a warning if no binary
  exists.
- **Snapshots** exclude `tor/` and `tls/` (onion and private keys are not index
  data); a restored snapshot starts with a new onion address.
- Evidence: `cargo test --workspace` (memory policy, torrc, onion validation,
  self-signed SAN/permissions, fake-Tor lifecycle, reset), the ignored
  `managed_tor_real_binary_creates_onion` with the real bundle, `e2e_regtest`,
  `e2e_pruned_ibd`, `e2e_launch_core`, installer with/without Tor, and a manual
  `run` showing `Tor onion: ...:50001 (plain), :50002 (tls)` + `Tor ready`.
- **Not claimed**: no real mainnet memory benchmark for alpha.24 yet; the
  operator still validates the cache growth and commit behavior on the VM.

## Last audit/fixes (alpha.23)

- **Prevout prefetch in reader threads**: alpha.22 mainnet logs showed `apply`
  growing from ~300 ms to >1.5 s per block while `read`/`decode`/`wait` stayed
  near zero. The single apply thread was doing every UTXO point lookup
  serially. Readers now resolve a speculative prevout superset while the
  previous block is applied; apply still checks the group overlay first and
  falls back to a point lookup, so speculation cannot change the result.
  `prefetch` is a new progress-line figure (not exported to `/status`).
- **RocksDB write amplification**: `atomic_flush` across the 7 CFs (one flush
  point, fewer/larger L0 files), 2 MiB compaction readahead and a 512 MiB L1.
- Evidence: oracle-parity tests now run through
  `prepare → prefetch → apply_prepared_with_prefetch` (including spends of
  outputs created earlier in the same uncommitted group), `cargo test
  --workspace`, `e2e_regtest`, `e2e_pruned_ibd`, `e2e_launch_core`.
- **Not claimed**: no measured mainnet speedup yet. The next real run should
  report `prefetch` vs `apply`; if `prefetch` is high and `apply` low, the disk
  is the limit, not the pipeline.

## Last audit/fixes (alpha.22)

- **Cache metric removed**; `--cache-mib` and the block cache remain.
- **`commit_sync = auto`**: `txids` in bulk/IBD, `always` live. `txids.dat`
  is always fsynced; startup truncates extras.
- **Core bulk pause + pre-warm**: pause during bulk, resume 5,000 blocks early
  so peers reconnect, then relative 20k guard.
- **Prune not deferred by free space by default**; normal cadence every 1k safe blocks;
  emergency low-disk prune remains.
- **READY / FULLY SYNCED** in log, `/status`, `/metrics`, `e-lite status`.
- **Wallet banner**: built-in INDEXING/READY templates plus optional
  `[server] banner_file` with `[indexing]`/`[ready]` and placeholders.
- **Apply micro-opts**: output script hashes precomputed in readers, txid set,
  reused buffers.
- `scripts/benchmark-index.sh` for real A/B runs.

Evidence: `CHANGELOG.md`, `docs/CLI.md`, `docs/TUNING.md`,
`cargo test --workspace`, `tests/e2e_regtest.sh` (banner/READY + log),
`tests/e2e_pruned_ibd.sh`.

## Last audit/fixes (alpha.21)

- **Deterministic store tests**: global failpoint tests are serialized with a
  mutex, fixing the alpha.20 CI flake. No runtime change.

## Last audit/fixes (alpha.20)

- **Bulk lag guard fix (AUD-026)**: the relative baseline was following the lag
  down while Core was paused, so the resume threshold dropped every block and
  Core resumed immediately. The baseline is now frozen while paused; the guard
  holds until the lag reaches half the growth window.
- **Explicit readers survive hot reload**: the `2 × CPUs` cap only applies to
  automatic reader counts.
- **`--log-file` without a parent directory** works.
- **`PreparedBlock` fields are private**; only Merkle/hash-validated prepared
  blocks can be applied.

## Last audit/fixes (alpha.19)

- **`e-lite.log`**: stdout/stderr are teed to `<data-dir>/e-lite.log` (append,
  rotation to `.1`, `--log-file`/`--log-max-mib`/`--no-log-file`). Snapshot
  export skips the log.
- **Prune cadence**: `min_gain_blocks` default 10,000 (emergency low-disk prune
  still bypasses it).
- **Parallel decode/txid/Merkle** in reader threads; apply only does UTXO work
  and batch writes. Block hash is computed once.
- **Commit**: skip `prune_undo` when the group has no undo; bulk interval up to
  30 s and batch up to 4096; controller shrinks the batch when commit dominates
  the interval.
- **Metrics**: `decode_ms`, `wait_ms`; the cache-hit metric added in alpha.19
  was removed in alpha.22 because RocksDB did not report it reliably.
- **Bulk guard is relative**: `--max-bulk-lag-blocks` bounds additional growth
  from the baseline, so a 300k backlog no longer freezes Core for the whole
  catch-up.
- **Readers capped by `2 × CPUs`** unless explicitly overridden.

Evidence: `docs/AUDIT.md` (AUD-022..025), `CHANGELOG.md`, `docs/CLI.md`,
`docs/TUNING.md`, `cargo test --workspace`, `tests/e2e_regtest.sh` (log file +
startup/shutdown), `tests/e2e_pruned_ibd.sh` (prune gate with explicit 288 gain).

## Last audit/fixes (alpha.18)

- **`/proc/meminfo` parser fixed** (the colon was left attached). This was the
  real cause of `memory pressure (0 MiB)` and the 1 GiB/0 MiB fallback on every
  machine; cache/readers were being throttled unnecessarily.
- **Global RocksDB memtable budget** instead of 64 MiB per CF × 3 memtables.
  Jobs capped by CPU count.
- **Ordered startup/shutdown** (AUD-021): no pre-Core P2P/mempool errors, no
  post-shutdown `Connection refused` or tuning lines.
- **Progress**: height/Core percentages over `headers`, verification progress,
  read/apply/commit/cache metrics, start/stop local times, milestones every
  10,000 blocks or 6 h with e-lite/Core disk usage.
- **Opt-in bulk lag guard** (`--max-bulk-lag-blocks`, default 0) for managed
  pruned Core during IBD.
- **TLS listener** (AUD-018): rustls, PEM cert/key, mtime reload, 10 s handshake
  timeout, no client authentication.
- **CLI/`--set`**: every TOML field can be overridden without a config file.

Evidence: `docs/AUDIT.md` (AUD-018..021), `CHANGELOG.md`,
`cargo test --workspace` and `tests/e2e_regtest.sh` (plain + TLS + clean
startup/shutdown + log assertions).

## Open risks / next priorities

P1 (before any public exposure or mainnet production claim):

1. Authentication for non-loopback Electrum. TLS encrypts the transport and
   rate limiting is only a throttle, not access control; document
   firewall/allowlist/VPN/Tor.
2. Core RPC TLS or a hard SSH/VPN requirement with tests.
3. Verify/close the audit residual risks in `docs/AUDIT.md` (snapshot archive
   limits, per-client threads, provider trust).

Performance/storage (P2, benchmark first):

- UTXO prefetch pipeline.
- HDD sequential/readahead mode.
- History compaction/refs/compression experiments.
- Adaptive controller v2 (queue/compaction signals).

Compatibility (P2/P3):

- Electrum 1.7 (outpoint spend index), BIP352 silent payments (schema v2
  needed).
- Systemd unit/container image.
- ARM64/more distros.

## Commands

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
tests/e2e_launch_core.sh
tests/e2e_pruned_ibd.sh
tests/e2e_regtest.sh
```

Hot reload:

```bash
kill -USR1 "$(pgrep -f 'e-lite run')"
```

## Do not do without explicit approval

- Change the database schema without `schema_version` + migration/test.
- Re-enable automatic pruning by default or weaken the 288/224 safety margins.
- Expose metrics publicly.
- Expose Electrum without TLS or without a firewall/VPN/Tor access policy; TLS
  alone does not authenticate clients.
- Claim client authentication, that rate limiting is access control, Electrum
  1.7 or silent payments are implemented.
- Delete an existing index/datadir to “fix” an error before checking
  `NEEDS_RECOVERY`/snapshot options.

## Known working test baseline at handoff

`cargo fmt --check`, clippy, workspace tests and the local regtest e2e
(`launch_core`, `pruned_ibd`, `regtest` with plain + TLS Electrum, clean
startup/shutdown/memory assertions, `e-lite.log` presence, 1.6 and 1.7
negotiation on the same instance, scriptpubkey/outpoint checks, outpoint
notification, real testmempoolaccept, the rate-limit burst test, the strict
Core RPC check and the Sparrow sequence with confirmed verbose transaction)
passed on the alpha.29.4.1 code. Full signet/testnet4 acceptance
is still operator-run; results belong in `docs/GETTING-STARTED.md`/release notes
when available.
