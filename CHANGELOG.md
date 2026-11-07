# Changelog

## 0.1.0-alpha.29.4.0 — 2026-11-07

### Feature: per-network ports, automatic TLS and isolated instances

- **Per-network listeners**: plain/TLS/metrics default to
  `55011/55022/55084` (mainnet), `55111/55122/55184` (testnet4),
  `55211/55222/55284` (signet) and `55311/55322/55384` (regtest). The plain
  listener binds `0.0.0.0`; the TLS listener copies its host. Explicit
  `--listen`, `--tls-listen` and `--metrics` still win.
- **TLS on by default**: `[server] tls = true`. On first start e-lite generates
  a self-signed certificate in `<data-dir>/tls/` (SANs: `localhost`, `127.0.0.1`,
  `::1`, hostname, onion when known) and logs its path and SHA-256. `--no-tls` /
  `tls = false` disables it. Operator PEMs are never overwritten.
- **TLS independent of Tor**: certificate generation moved to
  `bin/e-lite/src/tls.rs`; a Tor failure no longer disables clearnet TLS.
- **One folder per network**: data defaults to `~/e-lite/<network>` and Core to
  `<data-dir>/bitcoin`, so several instances can run side by side. A single
  legacy Core datadir is detected with a migration warning; two candidates
  (`~/.bitcoin` + `~/e-lite/bitcoin`) are rejected instead of guessed. The old
  `~/.local/share/e-lite/<network>` index keeps being used until moved.
- **Multi-instance**: distinct Electrum, TLS, metrics and onion ports per chain;
  Core cannot share a datadir across chains, so each instance gets its own.
- **`run` serves while Core is down**: with an existing index, `run` now opens
  Electrum in `CORE_UNAVAILABLE` and keeps retrying Core instead of waiting
  before serving. A first run with no identity still waits for Core.
- Tests: per-network defaults, loopback pairing, TLS on/off validation, cert
  generation without Tor, operator cert preservation, plus `tests/e2e_defaults.sh`
  (plain + TLS + metrics with no TLS/Tor configuration) alongside regtest and
  pruned-IBD.
- Docs updated: README, CLI, OPERATIONS, TOR, INSTALL, GETTING-STARTED,
  FEATURES, ARCHITECTURE, PRIVACY and AGENTS.

## 0.1.0-alpha.29.3.7 — 2026-11-07

### Patch: prune locator cleanup, external rawtx guards, disk-first gain

- **Sidecar cleanup**: on open, locators for pruned `blk*.dat` files are dropped
  and the sidecar is rewritten compactly before the scanner starts. After a
  prune, the in-memory locator map is filtered and cached file descriptors are
  closed. This prevents stale entries and deleted files kept open.
- **ReaderTracker only for local heights**: rawtx resolution enters a reader
  guard only when the block is at or above Core's prune height. External-only
  resolutions no longer block pruning. Prefetch also no longer holds a guard.
- **Disk-first gain**: `min_gain_blocks` default lowered from 10,000 to 1,000.
  On mainnet that bounds the retained tail to roughly ~1.5 GiB above the safe
  cut instead of ~15 GiB; Core still prunes whole files, so actual retention
  rounds to file boundaries. `prune_interval_seconds` remains 30 s.
- Tests for locator filtering and sidecar compaction; regtest and pruned-IBD e2e
  pass.
- No schema or wire-format change.

## 0.1.0-alpha.29.3.6 — 2026-11-07

### Patch: durable pruning, confirmed pauses and bounded indexing memory

- **Durability before prune**: e-lite flushes RocksDB/WAL before asking Core to
  delete block files, so the height authorising the cut is durable even when
  bulk commits used `sync=false`.
- **Failed prune retries**: a failed `pruneblockchain` RPC is retried at the same
  safe height after a 30 s backoff instead of waiting for the safe height to
  move. The normal 10k-block gain and interval still apply.
- **Confirmed Core pauses**: pause/resume only update internal state after
  `setnetworkactive` succeeds; a failed RPC is retried instead of being treated
  as done. Lag-guard resumes no longer override a disk pause.
- **Disk hysteresis fixed**: a cold start above the low watermark no longer
  pauses just because it is below the resume watermark.
- **Exact readiness**: `READY`/`FULLY SYNCED` require the exact indexed tip, not
  `height + 1`; prune/resume paths recompute the state instead of forcing READY.
- **Indexing independent of providers**: `run` starts indexing immediately and
  only gates the wallet listeners on provider readiness, so a provider outage
  cannot stop local block progress.
- **Bounded prepared blocks**: an explicit window limits fetched/decoded/prevout
  blocks held in the channel and out-of-order `pending` map.
- **Supervisor cadence**: a long 512-block catch-up yields after the poll
  interval so pruning/disk actions are not delayed.
- **Space-first default**: `prune_defer_free_gib = 0` by default; pruning is
  bounded by the 10k gain and 30 s interval, not by free space.
- **Pruned block files**: dropping the bulk reader cache after a prune releases
  descriptors/handles that could keep deleted files allocated. Disk checks now
  include a custom `blocks_dir`.
- Tests: prune retry, existing chain/index tests, regtest and pruned-IBD e2e.

## 0.1.0-alpha.29.3.5 — 2026-11-07

### Patch: outpoint notifications off the notifier thread, complete protocol matrix

- **Outpoint resolution moved to bounded workers**: `blockchain.outpoint.subscribe`
  status may need Core or an external raw transaction. It now runs in a
  2-worker, queue-256 pool with pending-job retry instead of blocking the
  single notifier thread and delaying every other wallet. Reorg notifications
  are preserved.
- **Removed the temporary live provider test** (`live_esplora_health_cooldown`);
  the provider cooldown remains covered by the deterministic local tests.
- **Electrum protocol matrix**: added `docs/ELECTRUM_PROTOCOL.md` with the
  method/version matrix (1.4–1.4.2, 1.6, 1.7), notification semantics, limits,
  deliberate deviations and performance notes.
- No schema or wire-format change.

## 0.1.0-alpha.29.3.4 — 2026-11-07

### Patch: BIP144 witness requests, BIP141 validation and Electrum protocol audit

- **P2P requests witness blocks**: `getdata` now uses `MSG_WITNESS_BLOCK`
  (`0x40000002`) instead of `MSG_BLOCK`. The previous code advertised
  `NODE_WITNESS` but asked for the legacy serialization, so peers returned
  blocks without witnesses and validation failed with `missing witness reserved
  value`.
- **P2P peer checks**: peers without `NODE_WITNESS` are rejected, `notfound` is
  an immediate error, and the witness commitment is verified inside the P2P
  source before a peer is marked good. A bad peer is cooled instead of being
  retried for every transaction.
- **BIP141 correctness**: the highest-index coinbase commitment output is used,
  and the coinbase witness must contain exactly one 32-byte reserved value.
- **Electrum 1.7 notifications**: scripthash and scriptpubkey subscriptions are
  independent maps, so a client using both receives both notifications.
  Outpoint subscriptions also force a notification on reorg.
- **JSON-RPC parameters**: heights, counts, `cp_height` and `txout_idx` use
  checked conversions instead of truncating `u64` to `u32`.
- **Prefetch**: deduplication is by block hash, failed/queue-full work is not
  marked as done, and computing a subscription status no longer triggers
  external block downloads.
- **Cache**: block reads and `fsync` no longer hold the metadata lock; a lowered
  cache capacity is enforced at open, not only after the next write.
- **testmempoolaccept**: Core's `reject-reason` is mapped to Electrum's
  `reason`.
- **Bounded external work**: a 30 s deadline across external sources and a 45 s
  inflight wait prevent a single rawtx request from hanging for minutes.
- Tests updated for witness-block getdata, witness-service peers, BIP141
  commitment selection/reserved value, and the protocol changes. No schema
  change.

## 0.1.0-alpha.29.3.3 — 2026-11-07

### Patch: wallet-friendly rate limit and disk-minimal defaults

- **Rate limit is now a throttle, not an error**: the same 200 req/s with burst
  5000 applies to every client (loopback, LAN and public). When the bucket is
  empty the request waits for the next token (a few milliseconds at 200 req/s)
  instead of returning `-32000 "rate limit exceeded"`. A Sparrow gap scan no
  longer aborts around address 55; `server.rate_limit = 0` still disables the
  limiter.
- **Disk-minimal defaults for a max-pruned Core**:
  - rawtx cache default lowered from 2048 MiB to 256 MiB;
  - new `rawtx.cache_dir` lets the cache live on tmpfs (for example
    `/dev/shm/e-lite-rawtx`), keeping block data off persistent disk;
  - application log rotation lowered from 64 MiB to 16 MiB;
  - snapshots remain manual/off by default.
- **Disk breakdown** in `/status.disk`: `index_bytes`, `txids_bytes`,
  `cache_bytes`, `cache_dir`, `log_bytes`, `snapshot_bytes` and `total_bytes`.
- Tests: rate-limit unit and protocol tests now assert waiting/success instead
  of rejection. No schema, Electrum protocol or provider behaviour change.

## 0.1.0-alpha.29.3.2 — 2026-11-07

### Patch: never fail a wallet request without trying a provider

- **All-provider cooldown fallback**: when every provider is cooling down,
  e-lite now makes one forced attempt (last good provider first) instead of
  returning an error immediately. A single transient failure no longer turns
  into minutes of failed `transaction.get`/history loads.
- **Last good provider retried sooner**: after one failure it cools down for at
  most 30 s (never-successful providers keep the full cooldown).
- **Bounded SOCKS5 handshake**: the P2P Tor path no longer depends on the
  `socks` crate (which has no connect/read timeout). A minimal SOCKS5 CONNECT
  uses the P2P timeout for connect, auth, request and reply, so a stalled Tor
  SOCKS proxy cannot block a fetch indefinitely.
- **Inflight panic guard**: a panicking external fetch now completes the shared
  inflight entry with an error instead of leaving followers waiting.
- **Diagnostics**: failed `get_history` (scripthash) and failed rawtx
  resolutions are logged with the scripthash/txid, height and block hash.
- Tests: all-cooled providers still get one attempt, a failed provider is
  skipped when another works, SOCKS handshake timeout and password handshake.
- No schema or Electrum protocol changes.

## 0.1.0-alpha.29.3.1 — 2026-11-07

### Patch: stop dead providers and peers from stalling rawtx

- **Direct provider cooldown**: a failed Tor *or direct* request now cools that
  provider for `provider_cooldown_seconds` (default 300 s), and the last
  provider that answered is tried first. Previously a dead clearnet provider
  (for example `mempool.space/signet/api` timing out) cost 15 s on every block;
  the 17–34 s gaps in Sparrow matched that timeout.
- **Shorter rawtx timeout**: `request_timeout_seconds` default is now 8 s
  (was 15). Healthy Esplora providers answer in well under a second.
- **In-flight block dedup**: concurrent `transaction.get` calls and the history
  prefetcher for the same block share one download instead of racing.
- **Concurrency limit**: `max_parallel_fetches` (default 4) bounds external
  block downloads, including decoys.
- **P2P fail-fast**:
  - `p2p_timeout_seconds` (default 8) is separate from Core RPC's timeout and
    budgets v2 + v1 per peer; one block has a total 2× timeout cap.
  - Peers come from Core's current connections first (`getpeerinfo`), then
    addrman; up to three are tried per block.
  - The last peer that answered goes first; failed peers are skipped for the
    provider cooldown instead of being retried for every block.
  - A failed Tor SOCKS connect trips a circuit breaker: non-onion peers go
    direct for the cooldown window (with one warning) instead of retrying Tor
    per peer and per block.
  - Direct connects now use `connect_timeout`; previously the OS timeout could
    add minutes.
- **Cheaper decoys and prefetch**: `decoys` defaults to 1 (was 3), at most two
  decoys run at once, and the prefetch never spawns decoys. The prefetcher uses
  one worker and at most 16 heights per history response.
- `/status` and `/metrics` expose provider/P2P failures, cooling skips,
  decoys in flight and prefetch queued/done.
- Tests: silent P2P peer fails within the short timeout and is not reconnected
  while cooled; concurrent resolvers share one block download; a failed direct
  provider is not retried. Workspace tests and regtest e2e are green.
- No schema or Electrum protocol changes.

## 0.1.0-alpha.29.3 — 2026-11-07

### Patch: P2P witness, faster rawtx and history prefetch

- **P2P now advertises `NODE_WITNESS`** in `version` (v1 and v2). Without it,
  peers strip witnesses from blocks; the block hash/merkle still match, so
  e-lite accepted the bytes but the BIP141 check failed with `missing witness
  reserved value`. This was the cause of stuck `Retrieving transaction` on
  signet.
- **Esplora request timeout** reduced from 60 s to 15 s
  (`rawtx.request_timeout_seconds`) and **per-provider Tor cooldown**
  (`rawtx.provider_cooldown_seconds`, default 120 s): after a failed Tor request
  that provider goes direct instead of waiting a minute per transaction.
- **P2P clearnet fallback** when Tor fails (`tor_fallback_clearnet`), with a
  one-time warning.
- **Readiness probe now fetches the genesis raw block** (`/block/<hash>/raw`)
  and verifies its header hash, not only `/block-height/0`. A provider that
  cannot serve real blocks cannot open the wallet gate.
- **Tor warning fixed**: “Tor unavailable” only when no probe used Tor; partial
  provider fallback is reported separately.
- **History block prefetch**: when a script history is served, e-lite enqueues
  the confirmed heights in a bounded, deduplicated background queue (2 workers)
  so the subsequent `transaction.get` hits the block cache.
- Config: `request_timeout_seconds`, `provider_cooldown_seconds`; docs updated.
- Tests: NODE_WITNESS version payload, raw-block genesis probe (match/mismatch),
  Tor fallback state; existing 1.4/1.6/1.7 e2e stays green.
- No schema or protocol changes.

## 0.1.0-alpha.29.2 — 2026-11-06

### Patch: provider readiness checks the network genesis

The readiness probe accepted any 64-hex `/block-height/0` response, so a
provider serving a different network could pass the gate and let wallets open
against the wrong chain.

- `public_servers::probe` now receives the expected genesis (the indexed
  `NetworkIdentity` in `run`/`serve`; the known chain genesis in `doctor` when
  available).
- A provider whose `/block-height/0` differs is marked unhealthy with
  `genesis mismatch ... (wrong network)` and cannot satisfy `require_ready`.
- Tests: healthy provider with the expected genesis and a provider returning a
  different genesis (gate rejects it). Existing 1.4/1.6/1.7 regtest e2e remains
  green.
- No protocol or schema changes.

## 0.1.0-alpha.29.1 — 2026-11-06

### Patch: provider readiness and clearnet fallback

- **Readiness before wallets**: `run` probes the network's `public-servers.toml`
  entries (`GET /block-height/0`) and only opens the Electrum listeners after at
  least one provider answers. If none does within `ready_timeout_seconds`
  (default 60 s), it exits with instructions instead of letting a wallet receive
  an incomplete history.
- **Tor fallback to clearnet**: `[rawtx] tor_fallback_clearnet = true` (default)
  tries Tor first, then the clearnet endpoint if the Tor proxy is unavailable or
  the Tor request fails. `.onion` endpoints still require Tor. Clearnet fallback
  logs a warning and is visible in `/status`; use `false` for fail-closed.
- **Health surface**: `/status` now includes per-provider `{url, healthy,
  latency_ms, via, error}`, `provider_ready`, `providers_healthy`, `tor_ready`
  and `clearnet_fallback`; `/metrics` adds `e_lite_providers_healthy`.
  `doctor` probes and prints the same table.
- **`serve`** probes once but does not block (offline mode); it warns when no
  provider is ready. `index`/`doctor` never block.
- **Config**: `[public_servers] require_ready`, `probe_timeout_seconds`,
  `ready_timeout_seconds`; `[rawtx] tor_fallback_clearnet`.
- Tests: provider probe (healthy fallback), readiness gate decision, Tor
  fallback state, config parsing; existing 1.4/1.6/1.7 e2e regtest stays green.
- No schema or protocol changes.

## 0.1.0-alpha.29 — 2026-11-06

### Core RPC exposure guard

Core RPC is HTTP only and has no native TLS. e-lite now makes the cleartext
LAN/WAN risk visible and enforceable.

- Startup/commands warn when `rpc_url` is non-loopback: credentials cross the
  link in clear text; use an SSH/VPN tunnel.
- `[core] require_loopback_rpc = true` makes a non-loopback `rpc_url` a fatal
  configuration error with instructions (SSH forward + `127.0.0.1`).
- `doctor` prints the RPC transport (`loopback` or `non-loopback, cleartext`).
- `/status` adds `core_rpc_loopback`.
- Documentation: SSH tunnel recipe, config example, CLI table, privacy notes.
- Roadmap cleanup: remove the completed rate-limit/UTXO-prefetch/Core-RPC rows;
  systemd remains optional.
- No change for the default loopback Core setup.

## 0.1.0-alpha.28 — 2026-11-06

### Per-client Electrum rate limiting

Defends a non-loopback Electrum listener from request bursts without pretending
to authenticate wallets.

- New `[server] rate_limit` (requests/second per client) and
  `[server] rate_burst`. Absent/`None` means auto: **enabled at 50 req/s with a
  300 burst** when any listener is non-loopback, **disabled on loopback**.
  Explicit `0` disables it; any positive value forces it.
- One token bucket per connection; every JSON-RPC line costs one token. When the
  budget is exhausted the server returns `-32000 "rate limit exceeded; slow
  down"` and keeps the connection open. Notifications still get no response.
- The notifier (subscriptions) does not consume the bucket.
- `/status` exposes `rate_limit_rps`, `rate_burst` and `rate_limited`;
  `/metrics` adds `e_lite_rate_limited_total`.
- Docs: `config.example.toml`, `CLI.md`, `FEATURES.md`, `ROADMAP.md` (systemd is
  now explicitly optional, not a release blocker) and `AUDIT.md` (AUD-010/015).
- Tests: token-bucket unit test, a burst over a real socket that is limited but
  keeps the connection alive, and config parsing.

## 0.1.0-alpha.27.2 — 2026-11-06

### Patch: enable the verified mempool.space onion

Verified over Tor 0.4.9.15 on 2026-11-06: the official mempool.space onion
returns the raw block for block 9, the double-SHA256 matches the requested hash,
and the bytes are identical to the clearnet response.

- The mempool.space onion `/api` endpoint is now enabled by default in
  `public-servers.toml`.
- The Blockstream API onion candidate did not resolve over Tor (SOCKS5 cannot
  connect) and stays disabled; enabling it requires a future verification.
- Existing `public-servers.toml` files are never rewritten, so only new/first-run
  installs get the enabled onion; edit the file to add it manually if needed.

## 0.1.0-alpha.27.1 — 2026-11-06

### Patch: 1.7 scriptpubkey notifications

`blockchain.scriptpubkey.subscribe` notifications must use the
`blockchain.scriptpubkey.subscribe` method (the params still carry the
scripthash). The notifier previously sent every notification as
`blockchain.scripthash.subscribe`.

- Scriptpubkey subscribers now receive the correct method name; scripthash
  subscribers keep `blockchain.scripthash.subscribe`.
- The 1.7 e2e now mines after subscribing and asserts both the outpoint and the
  scriptpubkey notification method. It also filters the spend UTXO by the mining
  address so the `spk_hint` check is exercised correctly.

## 0.1.0-alpha.27 — 2026-11-06

### Electrum 1.7 compatibility

The server now negotiates 1.4–1.7. Additive methods are available to every
client; only `server.ping` and `server.features` change shape with the
negotiated version. No schema, index or snapshot changes: 1.7 is resolved from
existing data plus the whole-block resolver.

- **`server.features`**: `protocol_max = "1.7"`; `hash_function` is omitted for
  1.7 clients and kept for 1.4/1.6.
- **`server.ping`**: 1.7 signature `(pong_len=0, data="")` returns
  `{"data": "<pong_len hex chars>"}`; notifications get no response. The length
  is capped at 1 MiB to avoid memory abuse.
- **`blockchain.scriptpubkey.*`**: `get_balance`, `get_history`, `get_mempool`,
  `listunspent`, `subscribe`, `unsubscribe`; history/mempool return dicts, the
  notification still uses the scripthash, and an oversized history returns
  `10001`. Scriptpubkey subscribers are notified on reorg even if the status is
  unchanged.
- **`blockchain.outpoint.*`**: `get_status`, `subscribe`, `unsubscribe`. Live
  UTXOs use the O(1) fast path; spent outputs are resolved from the script
  history and only the candidate raw transactions are fetched. `spk_hint` is
  verified against the live UTXO / mempool output / funding transaction; a
  mismatch returns `{}` instead of inventing a spender. Subscriptions are capped
  at 1,000 per connection.
- **`mempool.recent`**: last 10 mempool entries with `txid`, `fee`, `vsize`.
- **`blockchain.transaction.testmempoolaccept`**: local Core only.
- **`listunspent`** (scripthash and scriptpubkey) now uses blockchain order
  `(height, tx_pos)` for confirmed UTXOs.
- **Privacy/efficiency**: outpoint and scriptpubkey methods add no index writes;
  the e2e asserts `rawtx.fetches == 0` on regtest, i.e. only local data was used.
- Tests: 1.7 features/ping/shapes, scriptpubkey subscribe/unsubscribe, outpoint
  unknown/unspent/mempool/confirmed, outpoint notifications, real
  `testmempoolaccept`, and a 1.6/1.4 regression on the same instance.

## 0.1.0-alpha.26.1 — 2026-11-06

### Patch: public providers live in an editable file

Public Esplora endpoints are no longer compiled into the binary. The effective
list is read from `public-servers.toml` next to `config.toml`; the shipped file
is the single source of truth.

- **First run / missing file**: e-lite copies the shipped template (config dir,
  working directory, next to the binary, or `/usr/share/e-lite/`). Existing
  files are never overwritten. `[public_servers] generate = false` disables
  creation; a missing file means local data only (fail closed).
- **Verified defaults** in the template: Blockstream and mempool.space for
  mainnet, signet and testnet4; the mempool onion is present but commented until
  a real-Tor e2e verifies the `/api` path. `regtest` is empty.
- **Validation**: clearnet must be `https://`; plain `http://` is accepted only
  for `.onion` hosts and still needs Tor. Invalid URLs are warned and skipped.
- **Precedence**: `[rawtx] esplora_urls` (CLI/`--set`) → `public-servers.toml`
  → none. There is no hidden fallback list in code.
- **New `--public-servers <path>`** and `[public_servers]` config
  (`file`, `generate`).
- **`/status`** includes the effective `public_servers` list; **`doctor`** shows
  the resolved file, its source and the endpoints per network.
- **Packaging**: the template ships in the tarball and `.deb`
  (`/usr/share/e-lite/public-servers.toml`); `install.sh` copies it to
  `~/.config/e-lite/` only when absent.
- Tests: first-run creation, no-overwrite, invalid file, generation disabled,
  URL validation and an e2e doctor/status check.

## 0.1.0-alpha.26 — 2026-11-06

### Electrum 1.6 compatibility

The server now negotiates 1.4–1.6 and serves the 1.6 method shapes to clients
that ask for 1.6. 1.4 clients keep the previous responses on the same instance.

- **`blockchain.block.headers`** returns `headers` (an array of hex headers)
  for 1.6 clients; 1.4 clients keep the concatenated `hex` string. `count`,
  `max`, `root` and `branch` are unchanged.
- **`server.version` first** is enforced for 1.6: if another message arrives
  before it, negotiation is capped at 1.4.2 instead of advertising 1.6.
- **`mempool.get_info`** returns `mempoolminfee`, `minrelaytxfee` and
  `incrementalrelayfee` from Core.
- **`blockchain.estimatefee(number, mode)`** accepts `unset`, `economical` or
  `conservative`, passes the mode to Core and caches per `(target, mode)`.
  Invalid modes are `-32602`.
- **`blockchain.transaction.broadcast_package`** wraps Core `submitpackage`.
  The stable response is `{"success": bool, "errors": [{"txid", "error"}]}`;
  `verbose=true` returns Core's dictionary. Requires protocol 1.6.
- Version negotiation still downgrades gaps (`1.4.3`, `1.5`) to `1.4.2` and
  future versions to the newest implemented version below them.
- Tests: 1.6 negotiation and first-message cap, headers shape per version,
  `mempool.get_info`, fee modes and a real parent+child package broadcast in
  `tests/e2e_electrum16.py`.

1.7 remains unimplemented and is not advertised.

## 0.1.0-alpha.25.4 — 2026-11-06

### Patch: negotiate down instead of disconnecting

A client that requested an unimplemented or skipped version (for example `1.5`
or `1.6`) was disconnected even though the server could serve it with `1.4.2`.
The server now picks the newest implemented version at or below the client's
maximum and reports it in the `server.version` result. Only clients that cannot
speak 1.4 at all are refused.

- `ProtocolVersion::highest_available` replaces the strict range intersection;
  the implemented versions are listed in `SERVER_VERSIONS`.
- `1.4.3` (Namecoin-only), `1.5` (skipped) and future `1.6`/`1.7` now negotiate
  to `1.4.2` instead of erroring.
- Inverted ranges (`max < min`) are still `-32602`; a client maximum below
  `1.4` still closes the connection.
- Tests cover the selection matrix, the socket close path and the protocol
  fallback for future versions.

This is a deliberate leniency over the strict “no intersection, close” rule so
wallets asking for a newer protocol keep working; the response tells them the
actual version.

## 0.1.0-alpha.25.3 — 2026-11-06

### Patch: Electrum 1.4 protocol conformance

Finishes the 1.4 surface before starting 1.6/1.7. No schema, config or index
changes; existing wallets keep working and negotiate the same version.

- **`server.version` negotiation**: versions are parsed numerically (`1.4`,
  `1.4.2`, …), the server advertises and returns the negotiated version, only
  the first call on a connection is accepted, extra arguments are tolerated, and
  a client with no common version receives the error and is disconnected.
- **`server.features`** now advertises `protocol_min = "1.4"` and
  `protocol_max = "1.4.2"` (1.4.2 adds `scripthash.unsubscribe`, already
  implemented; 1.4.3 is Namecoin-only and is not advertised).
- **`blockchain.transaction.get(txid, true)`**: confirmed transactions now carry
  `hex`, `blockhash`, `confirmations`, `time`, `blocktime` and
  `in_active_chain`, which Core's `decoderawtransaction` does not provide.
  Mempool responses are normalised with `confirmations: 0`.
- **JSON-RPC notifications**: a request without `id` is executed without
  sending a response (required by 1.7 `server.ping` compatibility and correct
  JSON-RPC behaviour).
- **`blockchain.scripthash.unsubscribe`** returns `true` only if the scripthash
  was subscribed; a second call returns `false`.
- **`server.add_peer`** returns `false` instead of `method not found`.
- **Mempool order**: history, `get_mempool` and the status hash use the
  canonical `(-height, txid)` order, so the status is deterministic before 1.6.
- **Tests**: version negotiation/close, notification handling, unsubscribe,
  verbose transaction decoration, canonical mempool order, and the Sparrow e2e
  now checks the confirmed verbose fields.

## 0.1.0-alpha.25.2 — 2026-11-06

### Patch: onion ports mirror the Electrum listeners

`--listen 0.0.0.0:51001 --tls-listen 0.0.0.0:51002` still published the onion
on the default 50001/50002 because `[tor] onion_port`/`onion_tls_port` had fixed
defaults.

- `onion_port` and `onion_tls_port` now default to **0 = same port as the
  Electrum listener**. The example above publishes `<addr>.onion:51001` and
  `:51002`.
- Explicit `[tor] onion_port`/`onion_tls_port` still override the mirrored
  ports; a collision between the two is rejected.
- The startup log, `torrc`, persisted `tor/state`, `/status` and
  `e-lite onion show` all use the effective ports.
- Tests: listener mirroring (51001/51002), explicit override, and torrc
  rendering.

## 0.1.0-alpha.25.1 — 2026-11-06

### Patch: load the bundled Tor libraries

On Debian 13.7 (and any host without `libevent-2.1.so.7`), the managed Tor
exited with status 127 because the expert bundle's `tor` has no RPATH and the
loader could not find the `libevent`/`libssl`/`libcrypto` files shipped next to
it.

- **`LD_LIBRARY_PATH` for Tor**: e-lite prepends the Tor binary directory to
  the child environment, so the bundled libraries are always found without
  installing system packages. Existing `LD_LIBRARY_PATH` values are preserved.
- **Actionable failure output**: if Tor exits unexpectedly, the last lines of
  `tor.log` (including the loader error) are printed instead of only the exit
  code.
- **`doctor` runs `tor --version`** with the same environment and shows the
  exact error if a library is still missing.
- Tests: unit coverage for the library-path construction; the real-Tor
  integration test and a minimal Debian 12 container check (bundle without
  system libevent) pass with the new environment.

## 0.1.0-alpha.25 — 2026-11-05

### Onion works with wildcard Electrum listeners

Binding Electrum to `0.0.0.0` (or `[::]`) made the onion service fail with
`refusing to publish non-loopback target`. The wildcard socket already accepts
loopback connections, so the onion can safely target `127.0.0.1` (or `[::1]`).

- **Wildcard mapping**: `0.0.0.0:PORT` → `127.0.0.1:PORT` and
  `[::]:PORT` → `[::1]:PORT` for both the plain and TLS onion ports. A specific
  non-loopback address is still refused.
- **Explicit targets**: `[tor] target_plain`/`target_tls` (and
  `--tor-target-plain`/`--tor-target-tls`) override the listener-derived target.
- **Accurate `onion show`**: e-lite persists the effective ports/targets in
  `<data-dir>/tor/state`, so `e-lite onion show` reports the real onion ports
  even when the daemon was started with CLI flags.
- Tests: wildcard/IPv6 mapping, explicit target rendering, and `onion show`
  with a state file.

## 0.1.0-alpha.24 — 2026-11-05

### Adaptive memory and a managed Tor onion service

**Memory (alpha.24)**

- **Hot memory budget.** e-lite derives a ceiling from total RAM (75 %), keeps a
  reserve for the OS/Core/page cache (`max(1024, RAM/8)`), and grows the RocksDB
  block cache in 256 MiB steps while `MemAvailable` and `buff/cache` stay
  healthy. It shrinks 25 % (floor 256 MiB) under pressure, with a two-sample
  hysteresis, and can shrink even a manual `cache_mib` (now a maximum).
- **Larger memtables.** The RocksDB memtable budget is `RAM/8` (was `RAM/16`),
  reducing flush/compaction churn; the WAL cap scales with it.
- **Pre-resume shrink.** When the bulk catch-up is about to resume Core, the
  cache drops to the live budget so Core has room for its block index.
- **Observability.** Startup budget line, per-change log lines, and
  `memory.cache_mib`, `cache_max_mib`, `memtables_mib`, `reserve_mib`,
  `budget_mib`, `cached_mib` in `/status`/`e-lite status`.

**Tor (alpha.24)**

- **Managed Tor by default.** `[tor] mode = "managed"` starts e-lite's own Tor
  process (no system daemon), publishes Electrum as an onion service, and uses
  that Tor's SOCKS for rawtx.
- **Plain + TLS onion ports**: 50001 and 50002. TLS uses an automatically
  generated self-signed certificate (ECDSA P-256, SAN includes the onion
  address) when no operator certificate is configured; `CertWatcher` reloads it.
- **Installer-provisioned Tor bundle.** `install.sh` downloads the official Tor
  expert bundle (pinned version, SHA-256 verified) to
  `<prefix>/lib/e-lite/tor`; falls back to `tor` in `PATH`. `--without-tor`
  skips it. If no binary is available, indexing continues with a warning.
- **`e-lite onion show` / `e-lite onion reset`**: show the address/ports/state;
  reset moves the onion keys to `onion.prev` and the next start generates a new
  address. Reset refuses while the managed Tor is running.
- **Status and security.** `/status` and `e-lite status` show the onion address
  and bootstrap state. A warning documents that the onion has no client
  authentication or rate limiting.

Tests: unit coverage for the memory policy, torrc rendering, onion validation,
self-signed certificate SAN/permissions, fake-Tor lifecycle, real-Tor onion
generation (ignored unless `E_LITE_REAL_TOR` is set), and `onion reset`.

## 0.1.0-alpha.23 — 2026-11-05

### Apply path: overlap UTXO reads with the indexer pipeline

Mainnet bulk indexing was dominated by `apply` (300 ms–2 s per block) while
block reads, decode and prefetch waits stayed near zero. `apply` was doing every
prevout lookup serially in the single apply thread, so random UTXO reads could
not overlap with anything.

- **Speculative prevout prefetch in reader threads.** Each reader now resolves a
  block's prevouts from the committed UTXO set immediately after decoding, while
  the apply thread works on an earlier block. The prefetched set is a superset
  (it ignores group-local creations/deletions); apply still consults the
  in-memory group overlay first and falls back to a point lookup for any miss,
  so correctness does not depend on the speculation. New `prefetch` figure in
  the progress line isolates this cost from `apply`.
- **RocksDB atomic flush** across the seven column families: one flush point and
  fewer, larger L0 files instead of seven independent flushes, which cuts
  compaction write amplification on multi-CF batches.
- **Compaction shape for bulk ingest**: 2 MiB compaction readahead and a 512 MiB
  level-1 base.
- Tests: the oracle-parity suites now drive the production
  `prepare → prefetch → apply_prepared_with_prefetch` path, including blocks
  that spend outputs created earlier in the same uncommitted group.

## 0.1.0-alpha.22 — 2026-11-05

### Faster bulk indexing, readiness and wallet banner

- **Removed the cache-hit metric** (progress/status/metrics/controller). It was
  never reliable; `--cache-mib` and the RocksDB block cache remain.
- **`commit_sync = auto` (default)**: fsyncs `txids.dat` and relaxes the
  RocksDB fsync during bulk/IBD; returns to strict sync when live. A crash can
  only require reindexing the last interval, never corrupt the index.
- **Core bulk scheduling**: `pause_core_during_bulk = true` pauses Core during
  the long catch-up, resumes `core_resume_ahead_blocks` (5,000) before the tip so
  peers reconnect, then applies the relative 20k guard.
- **Prune deferral**: normal pruning is skipped while free space is at or above
  `prune_defer_free_gib` (50 GiB); emergency low-disk pruning remains. Normal
  cadence stays every 10,000 safe blocks.
- **READY / FULLY SYNCED**: one-time log lines, `/status` fields, metrics and
  `e-lite status` output. `READY` means the index reached Core's validated tip;
  `FULLY SYNCED` means Core is out of IBD and the index reached the headers.
- **Wallet banner**: dynamic `server.banner` with built-in INDEXING/READY
  templates and optional `[server] banner_file` (auto-created with
  `[indexing]`/`[ready]` sections and placeholders).
- **Apply micro-optimizations**: output script hashes precomputed in readers,
  txid set instead of all outpoints, reused effect/drain buffers.
- **Benchmark helper**: `scripts/benchmark-index.sh`.
- Tests: commit policy, prune deferral, banner sections/formatting, readiness
  log/banner in regtest e2e.

## 0.1.0-alpha.21 — 2026-11-05

### Test reliability

- **Serialized store tests that use global failpoints.** `reopen_keeps_tip...`
  could race with `failpoint_before_commit...` and fail in CI even though the
  code was correct. A test mutex now makes the suite deterministic. No runtime
  behavior change from alpha.20.

## 0.1.0-alpha.20 — 2026-11-04

### Fixes for alpha.19

- **Bulk lag guard no longer resumes immediately**: while Core was paused, the
  relative window was following the decreasing lag down, so the resume
  threshold dropped below the current lag and Core resumed on the next check.
  The baseline now stays fixed while paused and only follows the lag down while
  Core is downloading.
- **Explicit readers are no longer CPU-capped on hot reload**: `--readers`
  continues to win over the `2 × CPUs` automatic cap.
- **Log path without a parent directory** (`--log-file e-lite.log`) no longer
  fails while creating an empty parent path.
- `PreparedBlock` fields are private so only validated blocks can be applied.

## 0.1.0-alpha.19 — 2026-11-04

### Logging, indexing throughput and pruning cadence

- **Application log**: `run`, `index` and `serve` tee stdout/stderr to
  `<data-dir>/e-lite.log` (same lines as the console), append mode, 64 MiB
  rotation to `e-lite.log.1`, `--log-file`/`--log-max-mib`/`--no-log-file` and
  `[logging] file/max_mib`. Snapshots exclude the log and runtime markers.
- **Prune every 10,000 blocks by default**: `min_gain_blocks` defaults to
  `10000` instead of `288`, so the prune gate is far less intrusive. Low disk
  still triggers an emergency prune.
- **Parallel decode/txid/Merkle**: reader threads now deserialize the block,
  verify its hash and Merkle root, and compute txids; the apply thread only does
  continuity, UTXO work and batch writes. This overlaps CPU-heavy work with the
  previous block's apply/commit.
- **Commit path**: `prune_undo` is skipped when a group wrote no undo records;
  the block hash is computed once; bulk commit interval can grow to 30 s and
  batch to 4096; the controller avoids growing batches when commit already takes
  >75% of the interval.
- **Instrumentation**: progress/status/metrics now expose `decode_ms`,
  `wait_ms`, `cache_hit_known` (`cache ?` when RocksDB does not report the
  counter). Lightweight RocksDB statistics are enabled.
- **Relative bulk lag guard**: `--max-bulk-lag-blocks` now bounds additional lag
  growth from the baseline instead of freezing Core for an entire large
  catch-up; it resumes at half the growth window.
- **CPU-aware readers**: automatic reader counts are capped at `2 × CPUs`
  (explicit `--readers` still wins), avoiding 8 readers on a 2-vCPU machine.
- **Tests**: log file presence/content in e2e, relative bulk-lag window,
  prune default, decode/wait metrics, plus the existing TLS/memory/startup
  assertions.

## 0.1.0-alpha.18 — 2026-11-04

### Startup, shutdown and memory/performance

- **Fixed `/proc/meminfo` parsing** (the colon was left in the value). This was
  the real cause of `memory pressure (0 MiB)`: e-lite always fell back to 1 GiB
  total and 0 MiB available, shrinking the cache to 128 MiB and capping readers
  on machines with plenty of RAM. `doctor`/`/status` now show total, available,
  cgroup limit/usage and reclaimable cache.
- **RocksDB memtable budget is global, not per column family**. The old
  `cache/4` per CF could reserve ~1.3 GiB of write buffers plus the block cache.
  The budget is now `RAM/16` (64 MiB..1 GiB) split across the 7 CFs, and
  background jobs are capped by the CPU count.
- **Ordered startup**: `run` starts/adopts Core, waits for RPC (including block
  index loading) and only then builds the resolver, discovers P2P peers and
  starts mempool, notifications and listeners. `serve` remains the offline mode.
  This removes `p2p discovery failed`, `mempool refresh: Connection refused` and
  transient `Loading block index` noise.
- **Ordered shutdown**: mempool/notification threads observe a shutdown flag and
  are joined before Core is stopped. `Ctrl+C` ends with a local-time
  `stopped at … (uptime …)` line and no post-shutdown errors/tuning lines.
- **Progress**: height and Core now show percentages over Core `headers`, plus
  verification progress during IBD and read/apply/commit/cache metrics. A
  milestone line is logged every `milestone_blocks` (default 10,000) or
  `milestone_seconds` (default 6 h) with interval throughput and disk usage for
  e-lite and Core. Works on all networks.
- **Monotonic Core height** in the progress line (no more 615734 → 613288).
- **Optional bulk lag guard** (`--max-bulk-lag-blocks`, default 0/off): pauses
  Core P2P when a managed pruned Core is in IBD and the index lags more than the
  threshold, resuming at half. Bounds the gap and disk contention.
- **CLI coverage**: `--profile`, `--adaptive/--no-adaptive`, `--cache-mib`,
  `--write-buffer-mib`, `--background-jobs`, `--commit-interval-ms`,
  `--prefetch-blocks`, `--milestone-blocks/seconds`, pruning thresholds,
  `--core-arg`, rawtx/server flags, and `--set section.key=value` for any TOML
  field. See `docs/CLI.md`.
- **Tests**: memory/cgroup parsers, RocksDB budget, milestone timing, CLI
  overrides, TLS, and e2e startup/shutdown log assertions.

### Optional Electrum TLS

- **TLS is available on a separate, opt-in listener** (`[server] tls_listen`,
  `tls_cert`, `tls_key`; CLI `--tls-listen`, `--tls-cert`, `--tls-key`). The
  plain `127.0.0.1:50001` listener is unchanged and remains the default.
- **Any PEM certificate chain works**, including a public CA certificate reused
  from a web server. TLS 1.2+ via rustls; no client authentication.
- **Certificate reload on change**: the listener checks the PEM files every 60 s
  and rebuilds the TLS configuration when they change, so a Let's Encrypt
  renewal does not require restarting e-lite. An invalid PEM keeps the previous
  certificate and logs a warning.
- **Bounded handshake**: 10 s TLS handshake timeout; the existing 1 MiB request
  line cap and 600 s idle timeout still apply.
- **Transport refactor**: plain and TLS connections share one read/write path, so
  scripthash notifications can write to the same TLS connection. Notifications
  are delivered with at most ~250 ms of latency while a client is idle.
- **Observability**: `/status` and `/metrics` expose `tls_listen`,
  `tls_handshakes` and `tls_handshake_failures`.
- **Docs**: certificate setup, Let's Encrypt deploy hook, split-DNS, private-IP
  caveats, renewal and the nginx `stream` alternative in `docs/OPERATIONS.md`.
- **Tests**: TLS handshake + JSON-RPC integration test, certificate reload tests,
  and the regtest e2e now queries both plain and TLS listeners.

TLS encrypts the transport but does **not** authenticate wallets or rate-limit
requests; use a firewall, SSH/VPN or Tor for non-loopback listeners.

## 0.1.0-alpha.16 — 2026-11-03

### Security audit and documentation rewrite

- **Electrum request lines are capped at 1 MiB** before allocation; a client can
  no longer grow a per-connection buffer indefinitely without a newline.
- **`max_clients` is reserved atomically before spawning a client thread**, so a
  burst cannot exceed the configured limit.
- **Snapshot import rejects symlinks, hardlinks and special files**, preventing
  archive extraction outside the target or special-file creation.
- **Esplora endpoints must be http/https**; invalid schemes are skipped.
- **Non-loopback Electrum/metrics listeners log a startup warning** (no TLS or
  authentication is implemented).
- **Project memory added**: `AGENTS.md` and `docs/PROJECT_MEMORY.md` lock the
  one-line install/update UX, Core variant, documentation contract, current/
  future feature sections and audit rules.
- **Documentation regenerated** around `INSTALL`, `ARCHITECTURE`, `OPERATIONS`,
  `PRIVACY`, `FEATURES`, `ROADMAP`, `TUNING`, `AUDIT` and `GETTING-STARTED`.
- **Audit report** added in `docs/AUDIT.md` with fixed findings and residual
  risks (TLS/auth, Core RPC TLS, snapshot archive limits, provider trust).
- **Session handoff** added in `docs/HANDOFF.md` (current state, open risks,
  next priorities) and linked from `AGENTS.md`; new sessions should start there.

## 0.1.0-alpha.15 — 2026-11-03

### Container safety and conservative reader scaling

- **Automatic readers are capped by profile and memory**: 4 (HDD), 8 (SSD),
  16 (NVMe), never 28+ just because a read was fast. The controller now adds
  readers only when read latency is high and the apply stage is not the
  bottleneck.
- **Explicit `readers`, `cache_mib` and `commit_interval_ms` are locked**: the
  controller does not override them, except a critical memory floor.
- **Container-aware memory accounting**: `MemTotal`/`MemAvailable` are min'd
  with the cgroup v2/v1 limit and usage, so LXC/Docker do not over-allocate
  cache or workers.
- **Memory pressure backs off cache first**, keeps a reader floor of 2, and no
  longer collapses to one reader/64 MiB on a transient dip.
- **Docs**: `docs/TUNING.md` now documents the reader caps, locks and LXC
  OOM troubleshooting.

## 0.1.0-alpha.14 — 2026-11-03

### Autotuning, profiles and hot reload

- **Startup profiles**: `auto | hdd | ssd | nvme | manual`, selected from the
  `rotational` hint and RAM. Profiles set readers, commit interval, batch size
  and block-cache size.
- **Event-driven controller**: only changes one knob every 30 s, with hard
  bounds and no action while the disk/prune guards or reorg repair are active.
  It adjusts readers, commit interval/batch and block cache from measured
  read/apply/commit latency, tx/s and cache hit rate.
- **Hot reload with `SIGUSR1`**: readers, cache, commit interval, batch,
  profile, adaptive flag and pruning thresholds are applied to the running
  process. Invalid files keep the previous config; cold fields are reported.
- **Dynamic RocksDB cache**: the shared block cache is retained by `Store` and
  resized with `Cache::set_capacity`; mutable `max_background_jobs` is applied
  at runtime when RocksDB accepts it.
- **Bigger automatic cache/jobs ceilings**: auto cache is no longer capped at
  2 GiB, and background jobs are no longer capped at 4.
- **Instrumentation**: `/status` exposes a `tuning` object with effective
  readers/commit/batch/cache/profile and read/apply/commit/tx/cache EWMA
  metrics; `/metrics` exports the same values.
- **Documentation**: `docs/TUNING.md` and updated README/config examples.
- **CLI help** corrected for the new manual-prune default.

## 0.1.0-alpha.13 — 2026-11-03

### From-zero pruned IBD is manual by default

- **`prune_mib` defaults to `1` (manual pruning).** Core no longer deletes block
  files on its own during IBD. e-lite's prune gate is the only pruning
  authority: it never requests a cut above `committed - 288`, so Core cannot
  prune a block the index still needs. `0` remains a full node.
- **Automatic pruning requires an explicit opt-in.** `prune_mib >= 550` is
  rejected unless `[pruning] allow_auto_prune = true`. This prevents the
  from-zero default from recreating the prune-gap race.
- **Lag guard is manual-mode aware.** It is skipped for `prune_mib = 1`. For
  automatic pruning, `lag_limit` is hard-capped at 224 blocks (64 below Core's
  288-block floor), the default is 192, it resumes at 50 %, and the live check
  runs every 100 ms instead of once per second.
- **Disk backpressure keeps draining.** Below `min_free_gib` Core downloads
  pause but indexing continues, so the prune gate can delete old block files and
  free space. Only below `critical_free_gib` does indexing stop. This removes
  the old all-stop deadlock.
- **Prune requests are cheaper.** `min_gain_blocks` defaults to 288 and a new
  `prune_interval_seconds` (default 30) throttles no-op `pruneblockchain` calls
  that made Core flush during IBD. Low disk bypasses both as an emergency prune.
- **Prune-gap repair.** If automatic pruning still leaves a gap (or an existing
  index is behind an externally pruned Core), the indexer can fetch only the
  missing blocks from the validated rawtx cache, P2P or Esplora sources,
  verify hash/merkle, and continue. `repair_max_gap_blocks` (default 2048)
  bounds the work. No snapshot or full-chain backup is created.
- **Progress lines show live Core/prune height.** The per-call snapshot is no
  longer used as live lag, so a stale `lag 0` cannot hide a prune race.
- **Scripts and docs** use `prune_mib = 1` for from-zero signet/testnet4 runs.
  No snapshot is required or created by the flow.

## 0.1.0-alpha.12 — 2026-11-02

### Download and prune fixes

- **The lag pause is no longer undone by the crash-recovery marker.** The
  `core-paused-by-e-lite` recovery ran on every catch-up call, so a live lag
  pause was resumed on the next iteration, bypassing the 75% hysteresis and the
  reader check. Recovery now only runs when no live lag pause is in progress.
- **Prune requests are throttled by the requested height.** Core prunes at
  block-file granularity and can return the same height for thousands of
  blocks; retrying on every supervisor tick forced Core to flush during IBD.
  The supervisor now records the last requested height and only retries after
  `min_gain` blocks of safe-height progress.
- **Default `max_core_lag_blocks` is 512** (still capped by `blocks -
  pruneheight - 32`). On signet with `prune=550` the retention is thousands of
  blocks, so the old 256 limit paused Core unnecessarily.
- **Progress lines include `core`, `lag` and elapsed seconds**, so the catch-up
  burst rate is interpretable. All-in-one and network tests now use 512.

## 0.1.0-alpha.11 — 2026-11-02

### Autonomous startup and Core lifecycle

- **`run` waits for Core and auto-initializes.** With an empty database it binds
  the ports, reports `WAITING_FOR_CORE` and initializes as soon as Core appears.
  With an existing database it serves cached history while Core is down.
- **`[core] manage = external | auto | managed`.** `auto` uses a running Core
  read-only or starts it; `managed` always starts it. Only a Core started by
  e-lite can be paused, pruned or stopped.
- **Core launcher/supervisor**: `bitcoind`, `core_datadir`, `prune_mib`
  (`550` max pruning, `1` manual, `0` full), `extra_args`, `restart`,
  `stop_on_exit`, `log_file`, PID tracking and adoption after a crash.
- **Local-only pruned IBD.** A pruned Core that e-lite did not start, or a
  remote pruned Core, is rejected for a from-scratch index with instructions;
  e-lite never pauses a node it does not own.
- **Dynamic lag guard** based on `blocks - pruneheight` (default 256, resume at
  75%) instead of a fixed 32. It is reader-aware and does not pause while
  `pruneheight == 0`.
- **Disk guard on both filesystems** (index and Core datadir): below
  `min_free_gib` it pauses Core downloads and indexing, with adaptive
  hysteresis. The check is independent of Core RPC (`disk_tick`) and the
  indexer checks again before Core RPC and during block application; an
  uncommitted group is discarded so the disk is not pushed further. A starting
  warning reports free space below the threshold.
- **Reorg safety:** after a rewind the indexer re-reads the committed tip
  before replaying blocks.
- **From-zero disk requirement:** signet needs ≥15 GiB free and testnet4 ≥20 GiB
  for a full acceptance run; `tests/e2e_networks.sh` refuses to start below
  `E_LITE_E2E_REQUIRED_FREE_GIB` and documents how to override it.
- **Pause marker** so a crash while Core was paused is recovered on restart.
- **Prune height** now records the height Core actually returns, not the
  requested one.
- **Error classification**: transient “pruned data”/Core outages are retried;
  genesis/schema/txids/prune-gap failures are fatal.

### Configuration and networks

- `network = "auto"` detects the chain from Core or an existing database.
- Per-network Esplora defaults (mainnet, signet, testnet4; none on regtest).
- `rpc_user`/`rpc_password` for a remote Core; WAN is documented as HTTP over
  SSH/VPN.
- `parse_url` uses each network's default RPC port and supports IPv6 literals.
- Simplified config: removed `[index] mode`, `[pruning] enabled`,
  `undo_margin_blocks`, `core_recent_blocks_margin`, `min_gain_blocks`,
  `resume_core_lag_blocks`, `max_group_txs`, `commit_interval_ms` and
  `p2p_timeout_seconds`; renamed `bulk_readers`→`readers`,
  `db_cache_mib`→`cache_mib`, `cache_gib`→`cache_mib`.
- Validation: `prune_mib` must be `0`, `1` or `>= 550`; `data_dir` cannot be
  inside the Core datadir; `managed` requires an explicit network and loopback
  RPC.

### Performance and correctness

- The global index lock is no longer held across Core or external network I/O;
  a slow `transaction.get` cannot block index commits.
- Mempool refresh builds the candidate outside the write lock.
- `db_bytes` is cached; header checkpoint proofs use a cached header chain;
  history is capped at 100k entries per script.
- Notifications use a dirty-set: the indexer marks scripts touched by each
  confirmed block and the mempool marks additions/removals/height changes, so
  only affected subscriptions are recomputed. Reorgs force a full refresh.
- RocksDB tuning is automatic (`cache_mib = 0`, `readers = 0`) and open files are
  bounded.
- `family_digests` streams the flat TXID file instead of allocating it whole.

### Bulk block ingest

- **Bulk block ingest**: e-lite can read Core's `blk*.dat` directly
  (`[index] block_source = "auto" | "rpc" | "files"`, `--block-source`,
  `--blocks-dir`). Handles Core's default `-blocksxor` key (`blocks/xor.dat`),
  preallocated files, partial tails and file pruning. A persistent locator
  (`blockfiles.idx`) makes restarts cheap. RPC is always the fallback.
- **Faster ingest**: one batched `getblockhash` per window instead of one RPC per
  block; `readers` parallel fetch/read workers with an ordered writer;
  adaptive group commits (`batch_blocks`, internal TX/time limits); no fixed 1 s
  sleep between windows; TXIDs computed once and reused for the Merkle root.
- **Undo only inside the reorg window**: historical blocks no longer write undo
  records that would be deleted a few commits later. `prune_undo` now removes
  every record below the window in one pass, so the canonical digest is
  independent of the commit batch size.
- **RocksDB tuning**: shared block cache (`cache_mib`), bloom filters, larger
  write buffers and batched prevout reads (`multi_get_cf`).
- **Prune/ingest guard**: with automatic pruning, e-lite pauses Core downloads
  when the index lags more than the dynamic `max_core_lag_blocks` limit, then
  resumes at 75% of that limit.
- `doctor` reports the blocks directory, file count, XOR state and magic.
- Verified: A/B digest parity (RPC vs files), crash/restart parity, existing
  regtest and pruned-IBD E2E.

### Documentation

- New `scripts/e-lite-all-in-one.sh`: one command to install/update
  dependencies, Rust, e-lite and Bitcoin Core; configure; run a regtest quick
  test, the local e2e suite or the full signet/testnet4 acceptance; and write a
  report (`~/e-lite-test/last-report.txt`).
- New `docs/GETTING-STARTED.md`: prerequisites, disk planning, the one-command
  flow, regtest quickstart, full signet/testnet4 acceptance, troubleshooting and
  checklist.
- `docs/benchmarks/F2.md` records the two full-sync attempts that filled the
  disk, the root cause, the fix and the remaining manual acceptance.
- `docs/ADR-020-CORE-LIFECYCLE.md` documents the Core lifecycle and guards.

## 0.1.0-alpha.10 — 2026-11-02

- **Reorg both ways**: when Core's chain is shorter than the index
  (`invalidateblock`, manual rollback), the index now rewinds to the common
  ancestor instead of staying on the orphaned tip. Longer-chain reorgs keep
  using the undo window.
- **Mempool-aware history**: `blockchain.scripthash.get_history` appends
  unconfirmed transactions (`height` 0, or -1 when a parent is unconfirmed).
- **Correct balance semantics**: `confirmed` ignores mempool events and
  `unconfirmed` carries the net delta; previously a mempool spend was subtracted
  twice.
- **Chained mempool height**: `get_mempool`/`get_history` report `-1` for
  transactions with unconfirmed parents.
- **Checkpoint proofs**: `blockchain.block.header` and `blockchain.block.headers`
  accept `cp_height` and return the header merkle `root` plus `branch`.
- E2E additions: transaction merkle proof against the header root, checkpoint
  proof over the whole header chain, sender balance semantics, and a full
  reorg cycle (confirm -> invalidate -> rollback -> re-mine).

## 0.1.0-alpha.9 — 2026-11-02

- **Offline startup**: `e-lite run` now starts while Core is down. The database
  identity is trusted from disk, the Electrum/metrics ports bind, and confirmed
  history/balances are served from the cached index (`CORE_UNAVAILABLE`).
- **Lazy Core cookie**: construction no longer fails when the cookie file is
  missing (Core deletes it on shutdown); the cookie is loaded on the first
  request and refreshed on 401.
- Disk backpressure is scoped to `pruned-ibd`, so a full-core node is never
  paused by e-lite.
- Resilience E2E now restarts e-lite while Core is down and verifies 101 history
  entries, then recovery and SIGTERM exit 0.

## 0.1.0-alpha.8 — 2026-11-02

- **Silent payments opt-in gate**: `[silent_payments] enabled = false` by
  default. The subscribe/unsubscribe methods return an explicit server error
  instead of `unknown method`; enabling them reports that the BIP352 indexer is
  not in this build.
- **ADR-018** documents the protocol shape, the scan-key privacy constraints,
  the prevout-pubkey requirement (schema v2 or on-demand scan) and the future
  implementation/test plan.
- No protocol or schema change for normal wallets.

## 0.1.0-alpha.7 — 2026-11-01

- **Mode B (`pruned-ibd`)**: index while Core runs with `prune=1` (manual).
- **Disk supervisor** with hysteresis: pauses Core downloads
  (`setnetworkactive(false)`) below the low watermark and resumes above the high
  watermark.
- **Safe prune gate**: minimum of committed-tip minus undo margin, earliest
  active rawtx reader minus one, and Core tip minus recent-blocks margin. Never
  prunes while paused, behind, or with an active reader; `min_gain` prevents
  request storms. Only `pruneblockchain` is used; no block files are deleted by
  e-lite.
- Bounded indexing steps so the supervisor can act during long catch-ups.
- Reader tracking around `transaction.get` so pruning respects in-flight rawtx
  resolutions.
- Tests: gate formula, hysteresis, reader blocking, prune gain, and a regtest
  `prune=1 -fastprune` E2E: 2000 blocks indexed, Core pruned to 1530, old block
  unavailable in Core while e-lite still served 2000 history entries.

## 0.1.0-alpha.6 — 2026-11-01

- **BIP324 v2 P2P transport** (encrypted, via the `bip324` crate) with fallback
  to v1 for peers that do not support it. Per-peer Tor isolation is unchanged.
- Correct BIP324 application framing (1/13-byte message type) and short type IDs
  for block/ping/pong/getdata.
- New `[rawtx] p2p_v2` option (default true).
- Tests: v2 mock peer, v1 fallback, and live v2/v1 fetches against Bitcoin Core.

## 0.1.0-alpha.5 — 2026-11-01

- **Clean shutdown**: SIGTERM/SIGINT stop at a commit boundary, flush the pending
  group and exit 0.
- **Core outage resilience**: transient RPC failures set `CORE_UNAVAILABLE`,
  keep serving confirmed reads from the index and retry with backoff (1s→30s).
  Core cookies are reloaded on 401, so a Core restart (which rotates the cookie)
  recovers automatically.
- **Snapshot export/import**: `export-snapshot --output` writes a tar.gz with a
  manifest (network, genesis, tip, txnum, schema, canonical digest);
  `import-snapshot --input` restores into an empty data directory and verifies
  the digest.
- Rate-limited protocol error logging (method + code, no sensitive data).
- New resilience test: Core stop → reads still work → Core restart with rotated
  cookie → recovery → SIGTERM exit 0.
- E2E now also checks snapshot digest equality after restore.

## 0.1.0-alpha.4 — 2026-11-01

- **Mempool overlay** rebuilt from Core: per-script unconfirmed history, balance
  deltas, mempool UTXOs and spent outpoints (including chained transactions),
  refreshed every 3 s and immediately after a new block.
- **Fees**: `blockchain.estimatefee`, `blockchain.relayfee` and
  `mempool.get_fee_histogram`; `estimatefee` never returns `-1`.
- **Broadcast and verbose tx**: `blockchain.transaction.broadcast`,
  `transaction.get(..., true)` and `blockchain.block.stats`.
- **Push notifications**: headers and scripthash status are pushed to subscribed
  clients; status includes mempool entries.
- `scripthash.get_mempool`, unconfirmed balances and `listunspent` now include
  mempool data.
- `run` always follows the tip (`--no-follow` applies only to `index`).
- Per-request line limit (1 MiB) and notification counters in `/status` and
  `/metrics`.
- Tests: Sparrow-like connect sequence, broadcast, mempool overlay, notifications,
  fee fallbacks and block stats.

## 0.1.0-alpha.3 — 2026-11-01

- **Pruned Core support for serving**: the index must already cover the chain;
  `doctor` and the indexer refuse a prune gap with a clear message.
- **Raw transaction resolver** with a strict order: local Core -> validated cache
  -> external sources. Whole blocks only, never TXID lookups.
- **Verification**: block hash/header against the local chain, Merkle root, TXID
  and BIP141 witness commitment. Invalid data is never served or cached.
- **Sources**: Esplora-compatible `/block/:hash/raw` endpoints and a minimal
  Bitcoin P2P client (version/verack, getdata, NODE_NETWORK filtering).
- **Privacy defaults**: Tor SOCKS5h with stream isolation and fail-closed without
  Tor; 3 decoys per external fetch; 10 GiB validated LRU cache; no sensitive
  labels in metrics.
- `transaction.get` serves from the cache on the second access.
- New `[rawtx]` configuration and `rawtx` counters in `/status` and `/metrics`.
- Tests: witness/Merkle verification, cache eviction, source fallback, decoys,
  fail-closed Tor, P2P handshake, prune-gap guard, plus live Esplora and live P2P
  checks against real endpoints.

## 0.1.0-alpha.2 — 2026-11-01

- Fix: the undo-record reader required 85 bytes per spent input instead of the
  81 written by the encoder, which aborted indexing with `impossible spent count`
  once the reorg window started pruning old records (seen on Signet at ~4.6k).
- Pruning no longer decodes old undo records: deleting them at the window
  boundary is idempotent and cannot stop indexing.
- Undo layout sizes are now shared constants for writer and reader; the minimum
  record length check is exact (92 bytes).
- Tests: spent-heavy undo round-trip, minimal record, prune boundary with
  input-heavy records.
- No schema change: existing databases are read as-is.

## 0.1.0-alpha.1 — 2026-11-01

- First public alpha: full-core indexing from Bitcoin Core (RocksDB).
- Electrum 1.4 server on loopback: headers, scripthash history/balance/UTXOs,
  transaction.get, get_merkle, id_from_pos, server.features/version.
- Networks: regtest, signet, testnet4, mainnet (explicit opt-in).
- Reorg handling with a bounded undo window (288 blocks).
- Prometheus /metrics and JSON /status on loopback.
- One-line installer/updater with optional official Bitcoin Core install.
- Debian 12+ package (.deb) built in debian:12; no systemd unit.

### Known limitations
- Confirmed chain only: no mempool, no push notifications, no broadcast.
- `full-core` only (Core prune=0); rawtx requires the block to exist in Core.
