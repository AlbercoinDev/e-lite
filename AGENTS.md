# AGENTS.md — e-lite project memory

This file is the operational memory for agents and maintainers. It is not a
historical design document; the ADRs under `docs/` remain the decision log.

## Start here in a new session

1. Read [`docs/HANDOFF.md`](docs/HANDOFF.md) for the current state, release,
   open risks and next priorities.
2. Read [`docs/README.md`](docs/README.md) for the documentation map.
3. Read [`docs/PROJECT_MEMORY.md`](docs/PROJECT_MEMORY.md) for the human
   companion of the invariants below.
4. Check `CHANGELOG.md` and the latest git tag before assuming a version.

Do not re-ask the user to paste the whole history. The durable context is in the
repo. If the handoff is stale, update it in the same change that changes state.
Never treat a version number in this file as authoritative over the code/tag.

## Product invariants

1. **One-line install/update is mandatory.** The primary command is:
   ```bash
   curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh | sh
   ```
   The same command installs or updates e-lite without touching configuration,
   databases or Core's datadir.
2. **A second one-line variant must install/update the latest official Bitcoin
   Core together with e-lite.** It is the recommended path for a working setup:
   ```bash
   curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh | sh -s -- --with-core --yes
   ```
   Keep both commands in `README.md` and `docs/INSTALL.md`; do not replace them
   with multi-step instructions.
3. Documentation must always explain:
   - how e-lite can run (all-in-one, split index/serve, offline serve, local,
     LAN and WAN Core, full and pruned Core);
   - what is served from the local index and what requires external sources;
   - which public servers are used, whether they are configurable, and how
     privacy is preserved (Tor, clearnet, whole-block requests, decoys,
     validation, cache).
4. Maintain two explicit documentation sections:
   - current functionality (`docs/FEATURES.md`);
   - future functionality and proposed improvements (`docs/ROADMAP.md`).
   Never describe an unimplemented feature as available. Electrum TLS on a
   separate listener is implemented; client authentication and rate limiting are
   not.
5. Every behaviour change updates documentation, `config.example.toml` and
   `CHANGELOG.md`. TLS changes must also stay in sync across
   `config.example.toml`, `docs/TOR.md`, `docs/FEATURES.md` and this file.
6. Pruning, snapshots, network exposure, privacy and data-loss questions require
   evidence in `docs/AUDIT.md`; do not guess.

## Current defaults worth remembering

- From-zero pruned IBD uses `prune_mib = 1` (manual pruning). Core never prunes
  on its own; the prune gate keeps the cut below `index - 288` and evaluates
  pruning every 5,000 safe blocks by default (low disk bypasses it). The
  20 GiB free-space deferral applies only while Core downloads or the index
  drains: once Core is out of IBD and the index matches its tip, the gate
  prunes to the safe maximum once (AUD-122), then every
  `min_gain_live_blocks` (144, ~1 day; 0 = use `min_gain_blocks`).
- `prune_mib >= 550` requires `[pruning] allow_auto_prune = true`.
- Snapshots are optional; they are never required by the from-zero flow.
  `export-snapshot` writes one archive per network (manifest **format 3**:
  network, genesis, index tip, schema, txnum, canonical digest; the manifest
  is also the first tar entry, `e-lite-snapshot.head.json`, so import refuses
  a wrong network/schema before extracting; one gzip member per section, read
  with `MultiGzDecoder`; formats 1-2 still import) and **always
  excludes the nested Core datadir `<data-dir>/bitcoin`** from the index
  section (AUD-124). `--with-core` additionally bundles that network's Core
  datadir under `bitcoin/` after a probe Core (start + RPC + stop) records
  tip, prune height and version; it refuses while any Core is running and on
  a non-loopback RPC URL. The Core section is an **allowlist**: only
  `blocks/`, `chainstate/` and `indexes/` of that network are packed; wallets
  (any name), `onion_v3_private_key`/`i2p_private_key`, `bitcoin.conf`,
  `settings.json`, mempool/peers/cookie/logs and other networks' subdirs are
  never copied and are listed as "not copied" by the export (AUD-126).
  gzip: the index section is stored (level 0, its files are already
  compressed) and Core uses level 3; `--compress-level` sets both. A SHA-256
  per Core file is recorded by default (`--no-hash-files`) and checked on
  import whenever present (AUD-140/143).
  `import-snapshot` needs an empty target, extracts and validates in a
  sibling `.<name>.import-partial` staging dir and renames it into place only
  when every check passed (AUD-141; a leftover staging dir is removed on the
  next import). `--expect-digest <hex>` refuses an archive whose recomputed
  digest is not the one the operator recorded at export (AUD-142). It
  validates network/genesis/
  schema/tip/digest plus the Core rules `index tip <= Core tip` and
  `prune_height <= index tip - 288`; `--verify` checks the per-file SHA-256
  table and `--verify=full` opens the restored Core with `-connect=0`
  `-listen=0`, checks both tips and runs `verifychain(2, 288)`. Cover:
  `tests/e2e_snapshot.sh`.
- Long bulk catch-up pauses Core by default and resumes at 7 GiB of unindexed
  backlog (legacy: 5,000 blocks) to pre-warm peers; normal pruning keeps a
  20 GiB free-space margin by default (`prune_defer_free_gib = 20`) and runs
  every 5k safe blocks / 30 s.
- Never authorise a Core prune from a tip that is not durable: flush RocksDB/WAL
  first. A failed prune RPC must retry the same safe height after backoff.
- Undo is **built** for every applied block (`apply_prepared_deferred_undo`)
  and kept in memory, trimmed to the reorg window as the group grows; at
  commit only the records inside the window of the tip actually committed are
  written (`write_window_undo`), and older ones in the database are deleted
  in the same batch (`prune_undo`). A commit forced by the time limit, the
  transaction limit, a boundary or shutdown must be as rewindable as a full
  batch; the old group-end prediction could commit a tip without undo.
  Never write undo into a bulk batch just to delete it at commit (AUD-131: it
  was ~a third of the IBD write volume). A Core rollback deeper than the
  window is `NEEDS_RECOVERY`, never a silent reindex.
- Crash recovery: SIGKILL during indexing, a write failure and a reorg at the
  indexed tip must all recover through RocksDB/WAL and undo. Covered by
  `tests/e2e_recovery.sh`; run it after touching commit, undo or rewind.
- Real mainnet default baseline (alpha.29.4.3): ~4.2–4.8 blocks/s at ~640k;
  per-block cost dominated by prevout prefetch (100–1500 ms), then decode
  (~25 ms), apply (20–100 ms) and commit (~7–8 ms/block amortised); read
  1–10 ms. The block cache oscillated 6.6 GiB → 256 MiB. Re-measure with
  `tests/bench_ibd.sh` and `/status` stage/commit/rocksdb counters after any
  change to the prefetch, cache or commit path.
- Multi-instance: one Core datadir per network, per-network Electrum/TLS/metrics
  ports and per-network managed-Tor SOCKS ports (9050 mainnet; 55150/55250/
  55350 for testnet4/signet/regtest). `tests/e2e_multi_instance.sh` covers two
  simultaneous instances, distinct certificates and independent shutdown.
  Only one **intensive sync** per user runs at a time: a `flock` in
  `$XDG_RUNTIME_DIR/e-lite/ibd.lock` (or `/tmp/e-lite-<uid>/ibd.lock`) makes a
  second instance **wait** (it keeps serving) and start automatically; the lock
  is taken before a from-zero managed Core starts and released within 1000
  blocks of Core. The kernel releases it on crash. `E_LITE_NO_SYNC_LOCK=1`
  disables it. It does not coordinate separate containers.
- Drop locators for deleted `blk*.dat` files (sidecar compaction at open and
  in-memory filtering after prune) and only hold a ReaderTracker guard when the
  block is still at/above Core's prune height; external resolutions must not
  block pruning. `min_gain_blocks` defaults to 5,000 for the disk-first goal.
- Pause/resume state must reflect `setnetworkactive` success; never let a lag
  resume override a disk pause. Start indexing before the provider gate so a
  provider outage cannot stop local block progress.
- `commit_sync = auto` relaxes the RocksDB fsync in bulk; `txids.dat` is always
  fsynced and live mode is strict.
- The supervisor runs at commit boundaries inside the catch-up pipeline; the
  only teardown left is the 512-block checkpoint. Do not reintroduce a time
  deadline that drops prepared blocks/readers. Disk and shutdown checks stay
  per block. `/status.pipeline.discarded` is the regression signal.
- The bulk lag window must not rewrite its baseline on a normal resume. Doing
  so raised the pause threshold by `max_bulk_lag_blocks/2` per cycle. The
  default is now an absolute byte window: pause at `max_bulk_lag_gib` (30),
  resume at `core_resume_ahead_gib` (7), because a block at 800k is much larger
  than at 100k. `max_bulk_lag_blocks`/`core_resume_ahead_blocks` are legacy and
  apply only when the GiB value is 0. The opt-in
  `E_LITE_LAG_POLICY=drain` uses the last commit cycle's rate; it stays opt-in
  until a mainnet A/B.
- `scheduler = "overlap"` is the default: e-lite keeps indexing while Core
  downloads. `scheduler = "alternate"` is opt-in: Core downloads while e-lite
  indexing is paused, then Core is paused and e-lite drains the byte backlog.
  Serving continues while paused; `alternate` requires `prune_mib = 1` and
  `pause_core_during_bulk = true`. Do not make it the default without a
  measured Core-solo rate above the ~6.7 blocks/s break-even. If the configured
  mode is `alternate` but e-lite cannot pause Core (external/already running),
  it warns and keeps `overlap` instead of advertising an inactive mode.
- The commit controller reads only the last **full** group
  (`/status.tuning.commit_last_*`). Boundary/shutdown partial commits do not
  update the EWMA or the cycle. A `Txs`-limited group must not shrink the
  batch; an I/O-bound group (`db_us > 2 × txids_us`) must not shrink it either.
  Reader changes require two consecutive samples and are driven by `wait_ms`
  (starvation), not by `read_ms` alone; `min_readers` is half the physical
  cores, at least 2.
- The pre-prune durability barrier is `SyncWAL(true)`, not `flush_all`: a
  single `WriteBatch` is atomic across CFs and `txids.dat` is fsynced first.
  Never prune if the barrier fails. `tests/e2e_pruned_ibd.sh` covers
  prune + SIGKILL + restart. `pruneblockchain` then runs in `el-prune` with a
  120 s RPC timeout; the apply thread continues.
- Machine defaults come from `MachinePlan` (`tuning.rs`): physical cores
  (not logical SMT), cgroup CPU quota, data/Core disk class and the memory
  hierarchy. Readers and background jobs are `physical.clamp(2, cap)` by
  data-disk class (readers: NVMe 8, SATA/ZFS/virtual 4, HDD/network 2; jobs:
  NVMe/SATA 8, ZFS/virtual/overlay/unknown 4, HDD/network 2), so a 4C/8T SATA
  host gets `readers=4`, `jobs=4`, Core `par=3`, `rpcthreads=4`. Explicit
  config still wins. `/status.machine` and `doctor`
  print the plan; unknown/virtual disks use the SATA profile.
- Bulk commits are bounded by transactions (250k), blocks (512), raw bytes
  (`clamp(RAM/128, 64-256 MiB)`) and time (20 s NVMe, 30 s SATA/virtual, 45 s
  HDD, 60 s network), with a minimum of 8 blocks unless 60 s elapsed. Live
  mode commits every 1.5 s. This is safe because the prune gate uses the
  committed tip. Do not turn the bulk interval into a per-second timer; it was
  30-40 % of wall time in the 29.6.0 log.
- A prune request is deferred to the top of the next apply iteration after the
  commit/WAL barrier; it never starts with an uncommitted group. RPC socket
  timeout/EAGAIN/reset is retried once (150 ms), not HTTP/RPC errors.
- RocksDB memtables are weighted per CF (`utxo` 6, `history`/`utxo_by_script`
  4) under the shared `WriteBufferManager`; `/status.rocksdb.cf` exposes the
  per-family pressure used to tune them.
- Electrum binds per-network defaults: plain/TLS/metrics are
  `55011/55022/55084` (mainnet), `55111/55122/55184` (testnet4),
  `55211/55222/55284` (signet) and `55311/55322/55384` (regtest). TLS binds
  `0.0.0.0`; the **plain listener binds loopback** by default (same-host
  wallets and the onion target) and reaches the network only with
  `[server] plain_lan = true`/`--plain-lan`, an explicit `listen`, or when TLS
  is disabled (AUD-147). An explicit `listen` is never rewritten and TLS then
  follows its host. Metrics stay on loopback. TLS is **on by default** (`server.tls`)
  with an auto-generated self-signed certificate; TLS is independent of Tor.
  `[tor] mode = "managed"` publishes the same plain/TLS ports as a v3 onion
  service. Neither listener authenticates clients; the onion address is the
  access secret. Do not expose them publicly without a tunnel/firewall plan.
- Electrum rate limiting is a per-connection token bucket, not authentication:
  `rate_limit` defaults to 200 req/s for every client (LAN and public alike),
  `rate_burst` to 5000, and `0` disables it. Over-budget requests **wait for the
  next token**; they must not be rejected, because a wallet's initial scan
  aborts on an error. Never add token/mTLS auth to the Electrum listener:
  wallets do not support it. Access control remains firewall/allowlist/VPN/Tor.
- Public servers are not compiled into the binary: they live in the editable
  `public-servers.toml` next to `config.toml`. A first run copies the shipped
  template; a missing file means local data only (fail closed), never a hidden
  fallback list. Only whole blocks are requested by hash, and clearnet entries
  must be HTTPS (`.onion` over Tor is the only plain-HTTP exception).
- P2P rawtx must advertise `NODE_WITNESS` (bit 3) **and request
  `MSG_WITNESS_BLOCK` (0x40000002) in `getdata`**. Advertising alone is not
  enough: `MSG_BLOCK` returns a block without witnesses, which still matches the
  header hash/merkle root but fails the BIP141 check with `missing witness
  reserved value`. Reject peers that do not advertise `NODE_WITNESS`, and
  validate the witness commitment inside the P2P source before marking a peer
  good. P2P is fail-fast: random order among the discovered peers (no peer is
  preferred, AUD-135), per-peer backoff (30 s doubling to the cooldown; a
  wrong answer gets the full cooldown), at most two attempt budgets per call
  (`p2p_tor_timeout_seconds` 30 s through Tor, `p2p_timeout_seconds` 8 s
  direct). **Only a failure of the local SOCKS proxy** (no TCP, failed
  greeting/auth) pauses P2P over Tor (15 s); a SOCKS reply about one peer
  (refused, unreachable, exit policy) is that peer's failure. Never cool a
  peer that was not contacted, and do not retry v1 after a v2 connection that
  could not be made (AUD-153). Discovery takes Core's **outbound** peers with
  `NODE_NETWORK`+`NODE_WITNESS`, onion first, never inbound (ephemeral port)
  or I2P/CJDNS, and keeps Core's v1/v2 transport as a hint (AUD-154). Never
  let one block spend minutes trying dead peers.
- BIP141: when several coinbase outputs match the commitment pattern, the
  highest output index is the commitment; the coinbase witness must be exactly
  one 32-byte reserved value.
- Electrum 1.7 scripthash and scriptpubkey subscriptions are kept in separate
  maps so a client using both receives both notifications. Outpoint and
  scriptpubkey subscriptions get a forced notification on reorg even if the
  status string is unchanged. Outpoint status resolution runs in a bounded
  worker pool, never in the notifier thread.
- External requests do not identify e-lite: HTTP `User-Agent` is
  `el_rawtx::HTTP_USER_AGENT` and the P2P subversion `P2P_USER_AGENT`
  (AUD-133). Every Esplora request over Tor uses a fresh random SOCKS
  username, so Tor (`IsolateSOCKSAuth`) puts each block on its own circuit
  (AUD-135). Logs never print a script hash, txid or the block hash of a
  wallet request: use `RawtxError::kind()`/`redact_hashes`, and reqwest
  errors only via `without_url()` (AUD-132). P2P peers discovered from Core
  are refreshed every 10 min (60 s while empty) by `el-p2p-peers`; pinned
  `p2p_peers` are never replaced (AUD-137).
- Provider health depends on the failure (`crates/el-rawtx/src/health.rs`,
  AUD-155): timeout/stall/truncated body/5xx back off 5 s doubling to 60 s,
  429/503 honour `Retry-After` (15 s doubling, max 120 s; over Tor the limit
  is per exit), and only a wrong answer gets `provider_cooldown_seconds`
  (300 s). A success clears the streak. Do not go back to cooling a provider
  for minutes after a slow circuit: without the clearnet fallback that left a
  wallet with no source at all. Providers are tried in
  **random order**; never prefer the one that answered last, which sent every
  block of a wallet to a single provider (AUD-135). If every provider is
  cooling down, e-lite forces one attempt on the one whose pause ends first.
  `max_parallel_fetches` (4) bounds total external downloads; concurrent
  requests for the same block share one download. Through Tor an Esplora
  attempt has `tor_connect_timeout_seconds` (20 s) for circuit + connection,
  fails after `tor_stall_timeout_seconds` (25 s) without a header or byte (the
  blocking reqwest client `timeout` is per read, not total; never use
  `RequestBuilder::timeout`) and is capped at `tor_request_timeout_seconds`
  (60 s). One block download has **one** budget, `EXTERNAL_BUDGET` (60 s),
  fixed when its leader registers (queue wait included) and split across
  sources (all but the last leave a third); followers wait
  `EXTERNAL_BUDGET + FOLLOWER_GRACE` (65 s), so they never give up before the
  leader (AUD-156). A block that failed is answered from that failure for
  `FAILURE_MEMORY` (10 s) (AUD-158).
  `RawtxError::NoSource` (nothing could be asked) is distinct from
  `Unavailable` (every source failed); `/status.rawtx.tx_get_failures` counts
  `transaction.get` failures by cause.
- Decoys default to 1 **per real external download, wallet or prefetch**.
  They are owed to one `el-decoys` thread that downloads them one at a time
  (0.25-1.5 s apart) outside the wallet's download slots, in
  `FetchMode::Quiet` (a decoy failure never changes provider/peer health),
  waits while a provider rate-limits, and holds no store handle; at most 32
  are owed (AUD-157). The history prefetch is therefore **on by default**
  (`[rawtx] prefetch`, AUD-159, supersedes AUD-134): it enqueues up to 16
  confirmed history blocks per history response, each with **all** of that
  history's txids in the block (`prefetch_block_txs`: one download caches
  them all), with one worker that never takes the last free download slot;
  blocks already in the transaction cache are not downloaded. It is an
  optimisation, never correctness. Without it a pruned node makes every
  `transaction.get` wait for a whole block over Tor.
- The rawtx cache defaults to 256 MiB under `<data-dir>/rawtx-cache` and is a
  **transaction** cache (AUD-139): wallet requests store the verified
  transaction plus its merkle branch (`tx/<xx>/<txid>.tx`, accounted in 4 KiB
  blocks), re-authenticated against the indexed header on every read; whole
  blocks are kept only for prune-gap repair and legacy entries
  (`cache_split`: a quarter of `cache_mib`, 1..64 MiB). The last 8 verified
  external blocks stay in memory for a wallet's burst. `cache_dir` may point
  at tmpfs to keep data off disk, and the log rotates at 16 MiB. Assume Core
  is pruned to the maximum: do not duplicate block data on disk without a
  disk-pressure cap. Snapshots are manual only. The layout belongs to
  `crates/el-rawtx/src/cachelayout.rs` (`*.blk` at the cache root,
  `tx/<xx>/<txid>.tx`); `e-lite cache status|clear` is the only code allowed to
  delete it and refuses the data directory, a RocksDB `CURRENT`, `index/`,
  `bitcoin/`, `tls/` or `tor/`, so the index, Core's chain data, the TLS
  identity and the onion keys are never touched (a test proves the files stay
  byte-identical). It is safe while serving; only a restart frees the in-memory
  block cache.
- The mempool poller lists txids (`getrawmempool false`) every 3 s and only
  when the list changed clones the overlay and fetches `getmempoolentry` +
  raw transaction for the **new** txids; confirmed prevouts are cached while
  the index tip is unchanged (AUD-138). The notifier sleeps on
  `DirtyScripts::wait` (woken by a commit or a mempool change, at most 2 s)
  and pushes notifications with a 2 s write timeout; a client that cannot
  take one is disconnected (AUD-146). `tests/log_redaction.rs` rejects log
  macros that print script hashes or txids.
- `run` probes the network's providers **in parallel**, each on its own Tor
  circuit, with one request each (the genesis raw block by its known hash,
  header-hash checked; `/block-height/0` first only when the genesis is
  unknown) and does not open Electrum until at least one answers. A Tor
  attempt gets at least `tor_request_timeout_seconds`; the gate opens 2 s
  after the first healthy provider and reports slower ones as "no answer
  yet" (`doctor` waits for all); `require_ready`/`ready_timeout_seconds` control the gate. The gate
  applies only to a **pruned** Core (or one that did not answer): an unpruned
  Core serves every raw transaction locally, so Electrum opens at once. It is
  fail-closed by default because Sparrow persists an unfetchable transaction
  when `transaction.get` fails (AUD-045); `[public_servers]
  open_without_provider = true` opens with local data after
  `ready_timeout_seconds` (AUD-127), and an `el-providers` thread re-probes
  every 60 s until one answers.
  External fetches are **Tor-only and fail-closed by default**
  (`tor_fallback_clearnet = false`, AUD-128): the opt-in fallback uses
  clearnet when Tor is unavailable or a request fails (log + `/status`),
  while `.onion` still needs Tor. Do not turn the default back on: it ties
  the operator IP to the blocks of the wallet.
- JSON-RPC batches are served (max 1000 items, 8 in parallel, ordered when
  the batch has `server.version`/unsubscribe). Sparrow batches only for
  banner names it knows; `[server] advertise_electrumx_compat` (opt-in,
  hot-reloadable) appends "(electrumx-compatible)" to the banner. The banner
  must always **start with "e-lite"**: never advertise another server's name
  (electrs/Fulcrum need a prefix and a fake version), and keep everything
  Sparrow expects from ElectrumX (batches, `unsubscribe`, `server.features`)
  implemented.
- Electrum protocol versions are negotiated per connection. This build serves
  1.4–1.7 (`server.version` returns the negotiated version). A request for a
  future or skipped version falls back to the newest implemented version at or
  below the client maximum; only a client that cannot speak 1.4 is refused.
  Protocol 1.6+ requires `server.version` to be the first message; otherwise the
  connection is capped at 1.4.2. Never raise `protocol_max` until every method
  of that version (and the previous ones) is implemented and tested: a server
  advertising a version MUST support all of its methods.
- Electrum 1.7 `outpoint.*` is resolved on demand from the existing history/UTXO
  data; do not add a persistent spend index to `apply_block` without a benchmark
  and a schema/migration plan. `scriptpubkey.*` reuses the scripthash logic.
- The memory budget is fitted to **available** RAM, not installed RAM:
  `effective = min(total, MemAvailable - core_reserve)`, where `core_reserve`
  is the fitted `-dbcache` when e-lite is about to start Core. Reserve,
  ceiling, runtime, memtables, cache start/floor and page floor derive from
  `effective`. The reserve is `memory_reserve_mib`, else
  `clamp(effective/8, 2048 MiB, 4096 MiB)` (`effective/4` at 2 GiB or less).
  Memtables are `clamp(effective/16, 512 MiB, 2048 MiB)`; the profile cache
  ceiling is `total/4` for NVMe/SATA/unknown/virtual (2 GiB at 8 GiB, 4 at 16,
  8 at 32) and `total/8` for HDD/network, then capped by the effective room
  (`effective - reserve - memtables - runtime`). The ceiling is not an
  allocation.
  An explicit fraction is **never raised** to satisfy floors:
  `planned_mib + reserve <= effective_mib` is a regression test. Memtables use
  one shared `WriteBufferManager`; they are normally fixed at startup. With
  `adaptive_memtables` the controller re-fits WBM + per-CF `write_buffer_size`
  to `effective/16` after a 10-minute persistent target and a 10-minute
  cooldown; cache changes have priority and explicit `write_buffer_mib`
  disables it. A critical-memory one-way shrink (halve, minimum 256 MiB) runs
  when `MemAvailable < 256 MiB`. `cache_mib` is a maximum, not a lock. The hot
  ceiling is `min(profile, MemAvailable - reserve_dyn)`; `MemAvailable`
  already excludes memtables/runtime/table readers, so those budgets must not
  be subtracted a second time (this double-count starved the cache to 8 MiB).
  The reserve is dynamic:
  `reserve_dyn = min(startup_reserve, max(1024 MiB, MemAvailable/2))`. The
  cache shrinks when `cache > ceiling` or `MemAvailable < reserve_dyn`, grows
  in 256 MiB steps only with a `clamp(effective/8, 256 MiB, 2 GiB)` margin and
  **p95 prefetch**, and needs two samples plus a 5-minute reversal dwell.
  Below 1 GiB it acts immediately; below 256 MiB readers are reduced and the
  cache may reach 8 MiB. The page-cache floor is reported but not added to the
  shrink floor (`MemAvailable` already includes reclaimable cache). The
  pre-Core-resume shrink uses `reserve_dyn` and only acts if
  `MemAvailable < reserve_dyn + cache/4`.
  `/status.memory` must show planned memtables, **actual** RocksDB memtable
  bytes and table-reader bytes; table readers live outside the block cache
  and `[index] max_open_files` exists for 8-16 GiB A/B (default 8192).
  `core_reserve` is the fitted or operator `-dbcache` only when e-lite will
  start Core; `run`/`index` probe the RPC port first, an already-running or
  external Core is already in `MemAvailable`, and `serve` reserves nothing.
- The byte pause/resume watermark must persist for 10 s before changing the
  Core state (`reader_pressure` may pause immediately). The block-size average
  is 1/1024, not 1/8. An opt-in `prefetch_deadline_ms` stops chunked prefetch
  after the deadline and leaves missing prevouts to the apply fallback.
- `[core] io_priority` is best-effort `ionice` for a Core e-lite owns;
  `[core] cgroup_io_weight` additionally tries a cgroup v2 `e-lite-core`
  child with `io.weight`. Failure or an unsupported scheduler is a warning,
  never fatal.
- `IbdLock` must be acquired **before** the first `catch_up_limited` call when
  Core is in IBD or the lag exceeds 5,000 blocks; a first 512-block call can
  otherwise run for minutes without the intensive-sync slot. The lock is
  released when the lag falls below 1,000 blocks.
- Minimum RAM (`run`/`index`): mainnet 7500 MiB, other networks 3800 MiB, from
  the effective total after cgroup limits (8 GB / 4 GB classes). The minimum is
  checked against total/cgroup, not current `MemAvailable`, so a temporary
  memory peak does not reject a valid 8 GB machine; the dynamic budget uses
  `MemAvailable` and reduces cache/memtables instead. The check runs before
  certificates, RocksDB or Core; `serve` is exempt. `--allow-low-memory` /
  `E_LITE_ALLOW_LOW_MEMORY=1` is CI-only.
- cgroups are resolved from `/proc/self/cgroup` + `/proc/self/mountinfo` and
  every ancestor is walked. Each limit is paired with that level's own usage;
  never `min(limits) - leaf_usage`. `max`/the v1 sentinel mean unlimited, and
  `doctor` distinguishes limited, unlimited and not detected.
- `max_open_files` is capped to `RLIMIT_NOFILE - reserve`, handles
  `RLIM_INFINITY`, never pads back above the limit, and is shown with the soft
  limit in `doctor`/`/status.file_descriptors`; a cap below 2048 warns and a
  limit leaving RocksDB < 20 descriptors is a startup error.
- A Core started by e-lite gets a fitted `-dbcache`
  (`min(total/8, MemAvailable/2, 4096)`, <= `RAM/3`), `-par` (physical cores -
  1, 1..8), `-rpcthreads` (physical cores, 4..16), `-listenonion=0` and an
  explicit `-prune` (including `-prune=0`). Core's own
  onion is disabled because the managed `torrc` has no `ControlPort`; only the
  Electrum onion is published. Operator values win for `-dbcache`, `-par`,
  `-rpcthreads` and `-listenonion`; `-datadir/-chain/-server/-daemon/
  -daemonwait/-rpcbind/-rpcport/-rpcallowip/-prune` in `extra_args` are
  rejected. An external Core is never configured, paused or restarted. Core's
  shutdown grace is 120 s.
- Core P2P networks (AUD-136, `core_plan::network_args`): `[core] network =
  "auto"` adds `-onion=<Tor SOCKS>` (+ `-i2psam`/`-i2pacceptincoming=0` when a
  SAM router answers, `-cjdnsreachable` with `cjdns = true`) and keeps
  clearnet; `"private"` adds `-proxy`, `-onlynet=onion|i2p|cjdns`, `-listen=0`,
  `-dnsseed=0`, `-dns=0`, `-discover=0`, `-natpmp=0` and **fails instead of
  starting Core on clearnet** when no private network exists (the supervisor
  never starts Core with a rejected plan); `"clearnet"` adds nothing.
  `private_broadcast` adds `-privatebroadcast=1` only if `bitcoind -help` has
  it and Tor or I2P is available; it stays opt-in because Core marks it
  experimental and the transaction reaches the mempool only when relayed
  back. Operator `-proxy/-onion/-onlynet/-i2psam/-connect/-privatebroadcast`
  win. The Tor is the managed one (`tor.socks_port`, else the network default
  port) or a reachable `rawtx.tor_socks`.
- Core RPC is HTTP; LAN/WAN use requires SSH/VPN. e-lite warns on a non-loopback
  `rpc_url`; `[core] require_loopback_rpc = true` makes it fatal. There is no
  native Core RPC TLS.
- RocksDB memory is budgeted globally through the shared `WriteBufferManager`
  and the fitted budget above; background jobs are CPU-capped, and every knob is
  available via CLI/`--set`. Index/bloom filter memory is reported separately
  as `table_readers_bytes`. Per-CF properties (pending compaction, memtables,
  SST size, table readers) are summed; DB-wide properties (`num-running-
  compactions`, shared block cache) are read once. `/status.rocksdb.
  properties_ok=false` means a zero is unknown, not empty.
- `run`/`index`/`serve` tee console output to `<data-dir>/e-lite.log`
  (`--log-file`, `--log-max-mib`, `--no-log-file`). `[logging] level` /
  `--log-level` is `info < debug < trace` (alpha default `debug`, stable will
  be `info`), hot-reloadable; INFO lines never change format, extra detail
  prints as `[e-lite] debug:`/`trace:`. The `debug:` wallet lines (`wallet:`,
  `wallet session:`, `tx.get failed:`) carry counts, latencies and heights
  only. A session summary prints only after >= 3 `get_history`/`transaction.get`
  in 60 s (pings do not count); sub-millisecond latencies print in µs. The same
  level prints one startup block (served banner and `advertise_electrumx_compat`,
  cache inventory from the in-memory state - never a directory walk - and the
  provider limits), the client name/version and protocol per connection, a
  one-time `first batch, N items`, and one `rawtx summary` at shutdown. Session
  and close lines carry batches (lines/items/max) plus the cache hits/puts/
  external-fetch and `tx.get` failure deltas of the session or window, which is
  how one log proves whether the wallet batched, whether the run was cold or
  warm and where the time went. Keep it aggregate (two relaxed counters per
  line, never per item) and never print hashes, scripts or txids.
- The height line prints `phase X b/s` (average since the phase changed),
  `recent X b/s` (60 s window over full commit groups) and `eta` =
  `lag / recent`. `recent` is the honest current rate.
- Dynamic ZFS ARC (`[index] zfs_arc_dynamic = true`): the cap grows to
  `zfs_arc_max_mib` (0 = half of RAM) while Core is in IBD or the index
  drains, and drops to `zfs_arc_min_mib` (1024) once fully synced, with a 5 s
  tick and 5 min dwell. It writes `zfs_arc_max` only when it can; without
  permission it stays warn-only and retries the permission probe every 10 min.
  `e-lite arc-setup` prints (and `--apply` as root runs) the group-writable
  parameter plus the `e-lite-arc-cap.service` oneshot; `install.sh` adds it on
  ZFS hosts.
- The adaptive controller is its own thread (`el-tune`): a 5 s tick with a 30 s
  minimum between changes, and one `adapt_gate` mutex covering the check and
  the mutation. It is the only caller of `maybe_adapt`. `commit/blk` is
  measured from the blocks actually committed; `commit_share` divides by the
  real start-to-start commit cycle, not the configured interval. Cache growth
  requires measured prefetch pressure, not just free RAM. Automatic changes
  never override an explicit `batch_blocks`.
- Disk accounting excludes a Core datadir nested inside the data directory;
  otherwise `disk.index_bytes` and the benchmark double-count Core.
- Raw transactions are local-first. External fallback requests whole blocks by
  hash, validates them, uses Tor by default and never sends wallet txids/scripts.

## TLS certificate model (one cert, no SNI)

- Every process serves a single PEM pair for all clients and both listeners (the
  TLS listener and the published onion TLS port). rustls is built with
  `with_single_cert`; there is no SNI or per-hostname certificate selection
  (`crates/el-serve/src/lib.rs`, `build_tls_config`). A wallet validates the cert
  against the name it connects to: a cert for `example.com` fails for `.onion`
  and the auto self-signed cert fails for any public hostname.
- The generated TLS identity is a **local CA plus a server leaf**, not a single
  self-signed leaf (`bin/e-lite/src/tls.rs`). Files under `<data-dir>/tls/`:
  `ca.crt`/`ca.key` (CA, `CA:TRUE`, key 0600), `self-signed.crt` (leaf followed
  by the CA, the TLS chain), `self-signed.key` (leaf key), `self-signed.san` and
  `cert-model`. It does not require Tor. SANs on the leaf: `localhost`,
  `127.0.0.1`, `::1`, the hostname, the non-loopback IPs of the local
  interfaces and the onion address once known.
- **Java/Sparrow compatibility**: Java rejects a non-CA certificate used as a
  `TrustAnchor` (`TrustAnchor ... is not a CA certificate`). The wallet must
  trust `ca.crt`, not the leaf. Do not "fix" the error by disabling TLS
  verification or by marking the leaf as a CA.
- The leaf is re-signed with the same CA when the SAN set changes (LAN IP,
  hostname, onion), so wallets keep trusting the CA across restarts and onion
  rotation. The leaf key is reused; a corrupt pair or a pre-CA installation is
  migrated on the next start (new CA, chain rewritten). A CA change requires a
  new trust import.
- `tls_auto_cert = false` may still serve an existing valid pair; it warns if
  `ca.crt` is missing. Operator certificates are never overwritten. Files are
  written `0600` through a temporary file and an atomic rename. The startup log
  prints the leaf fingerprint and the CA path/fingerprint; fingerprints are the
  SHA-256 of the DER certificate (what `openssl x509 -fingerprint -sha256`
  prints), not of the PEM text.
- Consequence: a public domain certificate in `[server] tls_cert`/`tls_key`
  disables the auto onion certificate, and `.onion` wallets then get a hostname
  mismatch. Pick one identity per instance: auto self-signed (onion), or a
  domain cert with `[tor] tls = false` / plain-only onion, or an explicit
  `target_tls` design. Do not "fix" one by breaking the other blindly.
- Config validation: `tls_listen` must differ from `listen`; `tls_cert` and
  `tls_key` must be set together; `server.tls = false` forbids the TLS fields.
  Default cert/key paths are filled only when **neither** was configured, so a
  half pair fails instead of being completed with a mismatched key. Empty
  `listen`/`tls_listen`/`metrics.listen` are filled from the network in
  `Config::apply_network_defaults` before validation. A legacy Core datadir is
  detected only when the chain's blocks directory exists, not from a bare
  `bitcoin.conf`.
- `CertWatcher` reloads changed PEMs by mtime without restart and keeps the old
  cert if the new pair is invalid (Let's Encrypt renewals need no restart).
- TLS only encrypts; there is no client auth and rate limiting is only a
  throttle (AUD-010). TLS binds `0.0.0.0` by default; restrict with a
  firewall/VPN/allowlist, or set `listen = "127.0.0.1:..."` (TLS follows it).
- Data layout: `~/e-lite/<network>` holds the index, TLS, Tor and logs, and
  `<data-dir>/bitcoin` is that network's Core datadir. Two instances must never
  share a Core datadir (Core cannot serve two chains from one at once). The
  legacy `~/.local/share/e-lite/<network>` and a single legacy Core datadir are
  detected; `~/.bitcoin` plus `~/e-lite/bitcoin` together are ambiguous and
  require an explicit `[core] core_datadir`.

## Quality gates

Before claiming completion:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Focused checks when touching a subsystem:

```bash
cargo test -p el-serve tls   # TLS config, watcher reload
cargo test -p e-lite tls     # TLS config validation, Tor auto-cert
```

Plus the relevant integration tests (`tests/e2e_*.sh`) and the install/update
commands on a clean environment.

Environment facts:

- Build needs `build-essential clang libclang-dev pkg-config` (RocksDB bindgen),
  matching the `debian:12` CI container.
- Each `tests/e2e_*.sh` needs `bitcoind` in `PATH` and a debug binary first
  (`cargo build -p e-lite`); they create their own regtest datadir and ports.

## Code review rule

Audit before rewriting. Record findings in `docs/AUDIT.md` with severity,
file:line, impact, reproduction/test, status and fix commit. Fix P0
(data-loss/security/crash) and P1 (functional/race) before publishing docs that
depend on the affected behaviour.
