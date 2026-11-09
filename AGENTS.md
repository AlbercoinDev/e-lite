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
  pruning every 10,000 safe blocks by default (low disk bypasses it).
- `prune_mib >= 550` requires `[pruning] allow_auto_prune = true`.
- Snapshots are optional; they are never required by the from-zero flow.
- Long bulk catch-up pauses Core by default and resumes 5,000 blocks early to
  pre-warm peers; normal pruning is not deferred by free space by default
  (`prune_defer_free_gib = 0`), only by the 1k-block gain and 30 s interval.
- Never authorise a Core prune from a tip that is not durable: flush RocksDB/WAL
  first. A failed prune RPC must retry the same safe height after backoff.
- Undo is generated for **every** applied block and pruned at commit time from
  the tip that is actually committed (`undo_cutoff`). A commit forced by the
  time limit, the transaction limit or the supervisor deadline must be as
  rewindable as a full batch; the old group-end prediction could commit a tip
  without undo. Pruning must also delete undo added by the same `WriteBatch`
  (`group_undo_heights`), which a DB-only scan cannot see. A Core rollback
  deeper than the window is `NEEDS_RECOVERY`, never a silent reindex.
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
  block pruning. `min_gain_blocks` defaults to 1,000 for the disk-first goal.
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
  opt-in `E_LITE_LAG_POLICY=drain` uses the last commit cycle's rate; it stays
  opt-in until a mainnet A/B.
- The pre-prune durability barrier is `SyncWAL(true)`, not `flush_all`: a
  single `WriteBatch` is atomic across CFs and `txids.dat` is fsynced first.
  Never prune if the barrier fails. `tests/e2e_pruned_ibd.sh` covers
  prune + SIGKILL + restart. `pruneblockchain` then runs in `el-prune` with a
  120 s RPC timeout; the apply thread continues.
- Machine defaults come from `MachinePlan` (`tuning.rs`): physical cores
  (not logical SMT), cgroup CPU quota, data/Core disk class and the memory
  hierarchy. A 4C/8T machine gets `readers=4`, `jobs=2`, Core `par=3`,
  `rpcthreads=4`. Explicit config still wins. `/status.machine` and `doctor`
  print the plan; unknown/virtual disks use the SATA profile.
- Bulk commits are bounded by transactions (250k), blocks (512) and time
  (20 s NVMe, 30 s SATA/virtual, 45 s HDD, 60 s network), with a minimum of 8
  blocks unless 60 s elapsed. Live mode commits every 1.5 s. This is safe
  because the prune gate uses the committed tip. Do not turn the bulk interval
  into a per-second timer; it was 30-40 % of wall time in the 29.6.0 log.
- RocksDB memtables are weighted per CF (`utxo` 6, `history`/`utxo_by_script`
  4) under the shared `WriteBufferManager`; `/status.rocksdb.cf` exposes the
  per-family pressure used to tune them.
- Electrum binds per-network defaults: plain/TLS/metrics are
  `55011/55022/55084` (mainnet), `55111/55122/55184` (testnet4),
  `55211/55222/55284` (signet) and `55311/55322/55384` (regtest), on `0.0.0.0`
  for Electrum and loopback for metrics. TLS is **on by default** (`server.tls`)
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
  good. P2P is fail-fast: connected peers first, last-good peer first, per-peer
  timeout/failed-peer cooldown, total 2× timeout, and a Tor circuit breaker
  before clearnet fallback. Never let one block spend minutes trying dead peers.
- BIP141: when several coinbase outputs match the commitment pattern, the
  highest output index is the commitment; the coinbase witness must be exactly
  one 32-byte reserved value.
- Electrum 1.7 scripthash and scriptpubkey subscriptions are kept in separate
  maps so a client using both receives both notifications. Outpoint and
  scriptpubkey subscriptions get a forced notification on reorg even if the
  status string is unchanged. Outpoint status resolution runs in a bounded
  worker pool, never in the notifier thread.
- Provider health is sticky: a failed Tor **or direct** request cools that
  provider for `provider_cooldown_seconds` (300 s), and the last successful
  provider is tried first. A provider that already answered is cooled for at
  most 30 s after a failure; if every provider is cooling down, e-lite forces
  one attempt rather than failing the wallet request. `max_parallel_fetches`
  (4) bounds total external downloads; concurrent requests for the same block
  share one download.
- Decoys default to 1, at most 2 are in flight, and the prefetch never spawns
  decoys. The prefetcher enqueues up to 16 confirmed history heights per
  history response with one worker; it is an optimisation, never correctness.
- The rawtx cache defaults to 256 MiB under `<data-dir>/rawtx-cache`; `cache_dir`
  may point at tmpfs to keep blocks off disk, and the log rotates at 16 MiB.
  Assume Core is pruned to the maximum: do not duplicate block data on disk
  without a disk-pressure cap. Snapshots are manual only.
- `run` probes the network's providers (`/block-height/0` plus the genesis raw
  block with a header-hash check) and does not open Electrum until at least one
  answers; `require_ready`/`ready_timeout_seconds` control the gate.
  `tor_fallback_clearnet = true` uses clearnet when Tor is unavailable or a
  request fails (log + `/status`), while `.onion` still needs Tor; set it to
  `false` for fail-closed.
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
- The memory budget reserves `memory_reserve_mib`, else `clamp(RAM/8, 512 MiB,
  2048 MiB)` (`RAM/4` at 2 GiB or less), for the OS/Core/page cache, and fits
  runtime + memtables + cache inside `min(RAM*memory_fraction, RAM-reserve)`.
  An explicit fraction is **never raised** to satisfy floors:
  `planned_mib <= ceiling_mib` is a regression test. Memtables use one shared
  `WriteBufferManager`; per-CF arithmetic no longer multiplies the budget.
  `cache_mib` is a maximum, not a lock, and the block cache still moves in
  256 MiB steps under `MemAvailable`. Below 256 MiB available the soft floor is
  bypassed, readers are reduced and the cache may reach 8 MiB. Before Core
  resumes downloads after a bulk pause, the cache shrinks to the live budget.
- Minimum RAM (`run`/`index`): mainnet 7500 MiB, other networks 3800 MiB, from
  the effective total after cgroup limits. The check runs before certificates,
  RocksDB or Core; `serve` is exempt. `--allow-low-memory` /
  `E_LITE_ALLOW_LOW_MEMORY=1` is CI-only.
- cgroups are resolved from `/proc/self/cgroup` + `/proc/self/mountinfo` and
  every ancestor is walked. Each limit is paired with that level's own usage;
  never `min(limits) - leaf_usage`. `max`/the v1 sentinel mean unlimited, and
  `doctor` distinguishes limited, unlimited and not detected.
- `max_open_files` is capped to `RLIMIT_NOFILE - reserve`, handles
  `RLIM_INFINITY`, never pads back above the limit, and is shown with the soft
  limit in `doctor`/`/status.file_descriptors`; a cap below 2048 warns and a
  limit leaving RocksDB < 20 descriptors is a startup error.
- A Core started by e-lite gets a fitted `-dbcache` (`clamp(RAM/8, 450, 4096)`,
  <= `RAM/3`), `-par` (physical cores - 1, 1..8), `-rpcthreads` (physical
  cores, 4..16), `-listenonion=0` and an explicit `-prune` (including
  `-prune=0`). Core's own
  onion is disabled because the managed `torrc` has no `ControlPort`; only the
  Electrum onion is published. Operator values win for `-dbcache`, `-par`,
  `-rpcthreads` and `-listenonion`; `-datadir/-chain/-server/-daemon/
  -daemonwait/-rpcbind/-rpcport/-rpcallowip/-prune` in `extra_args` are
  rejected. An external Core is never configured, paused or restarted. Core's
  shutdown grace is 120 s.
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
  (`--log-file`, `--log-max-mib`, `--no-log-file`).
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
  throttle (AUD-010). The default bind is `0.0.0.0`; restrict with a
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
