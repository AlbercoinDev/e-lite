# AGENTS.md — e-lite project memory

Rules for agents and maintainers. This file holds what must not be broken and
where the detail lives; defaults, ranges and history are in `docs/`, not here.

## Start here

1. [`docs/HANDOFF.md`](docs/HANDOFF.md): current state, open risks, next steps.
2. [`docs/README.md`](docs/README.md): the documentation map.
3. `CHANGELOG.md` and the latest git tag: the version. Never trust a version
   number written in a document over the code and the tag.

The durable context is in the repo: do not ask the user to paste history. If
the handoff is stale, update it in the change that makes it stale.

## Product invariants

1. **One-line install/update**, which never touches configuration, databases
   or Core's datadir. Both commands stay literal in `README.md` and
   `docs/INSTALL.md`:
   ```bash
   curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh | sh
   curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh | sh -s -- --with-core --yes
   ```
2. The docs always explain: every way to run e-lite (all-in-one, split,
   offline serve; local, LAN and WAN Core; full and pruned), what is served
   from the local index and what needs external sources, and which public
   servers are used and how privacy is kept.
3. `docs/FEATURES.md` is what exists; `docs/ROADMAP.md` is what does not.
   Never describe an unimplemented feature as available.
4. Every behaviour change updates the docs, `config.example.toml` and
   `CHANGELOG.md` in the same change.
5. Claims about pruning, snapshots, network exposure, privacy or data loss
   need evidence in `docs/AUDIT.md`. Do not guess.

## Owner decisions

- **No dependency on systemd or any init system.** e-lite restarts itself
  (`bin/e-lite/src/supervisor.rs`); nothing in e-lite or its installer creates
  a unit.
- **e-lite never writes the ZFS ARC cap**; it reads the ARC and warns when it
  is uncapped.
- **No online snapshot export**: a backup is taken with the service stopped.
- **Version negotiation stays lenient (AUD-041)**: `client_min` is not
  enforced. Do not make it strict.
- **The three external sources stay** (Electrum by txid, Esplora, P2P).
- **Panic policy (AUD-173)**: release builds unwind; a panic is contained
  only in a wallet-facing thread, everything else aborts and recovers.
- **External transactions go by txid with decoy wallets by default
  (AUD-163)**; `preset = "max"` sends whole blocks by hash only.
- **External fetches are Tor-only and fail closed by default (AUD-128)**.
  Clearnet and `use_tor = false` are explicit opt-ins, never defaults.
- **No authentication on the Electrum listeners**: wallets do not support it.
  Access control is firewall, allowlist, VPN or Tor.
- Work goes to a branch with commits; no push, tag or version bump unless
  asked.

## Rules that must not regress

Each rule names where the detail is. Read it before touching that area.

### Durability and pruning — `docs/TUNING.md`, `docs/OPERATIONS.md`

- Commit order is `txids.dat` fsync → one RocksDB `WriteBatch` → WAL. Never
  authorise a Core prune from a tip that is not durable: the barrier is
  `SyncWAL(true)`, and if it fails there is no prune. A failed prune RPC
  retries the same height.
- The prune gate keeps the cut below `index - 288` and uses the *committed*
  tip. `prune_mib >= 550` needs `allow_auto_prune = true`.
- Undo is built for every block and written only for the reorg window of the
  tip actually committed, whatever forced the commit (AUD-131). A rollback
  deeper than the window is `NEEDS_RECOVERY`, never a silent reindex.
- Do not hold a reader guard for a block below Core's prune height; drop
  locators of deleted `blk*.dat` files. External resolutions never block
  pruning.
- After touching commit, undo or rewind: `tests/e2e/e2e_recovery.sh` and
  `tests/e2e/e2e_pruned_ibd.sh`.

### Indexing pipeline — `bin/e-lite/src/pipeline.rs` and `pipeline/`, `docs/TUNING.md`

- Readers are stopped only through `ReaderPool` (`finish`/drop). A bare stop
  flag plus join hangs when the prepared window is full (AUD-169).
- The supervisor step runs at commit boundaries. Do not add a time deadline
  that drops prepared blocks; `/status.pipeline.discarded` is the signal.
- The bulk lag window is an absolute byte window and never rewrites its
  baseline on resume. A watermark must hold 10 s before changing Core's
  state. A lag resume never overrides a disk pause.
- Pause/resume state follows the result of `setnetworkactive`. An external
  Core is never configured, paused or restarted.
- Take `IbdLock` before the first catch-up call when Core is in IBD.
- Bulk commits are bounded by transactions, blocks, bytes and time; do not
  turn the interval into a per-second timer.
- Start indexing before the provider gate: a provider outage must not stop
  local progress.

### Memory and tuning — `bin/e-lite/src/tuning/`, `docs/TUNING.md`

- The budget is fitted to **available** RAM. An explicit fraction is never
  raised to satisfy floors: `planned_mib + reserve <= effective_mib` is a
  regression test.
- `MemAvailable` already excludes memtables, runtime and table readers: never
  subtract them a second time (it starved the cache to 8 MiB).
- cgroups: walk every ancestor and pair each limit with that level's own
  usage. Never `min(limits) - leaf_usage`.
- Machine defaults come from `MachinePlan` (`tuning/machine.rs`): physical
  cores, not SMT threads. Explicit configuration always wins, and automatic
  changes never override it.
- `el-tune` is the only caller of `maybe_adapt`. The commit controller reads
  only full groups; a transaction-limited or I/O-bound group must not shrink
  the batch. Reader changes need two samples.
- The minimum-RAM check uses total/cgroup memory, not `MemAvailable`.
- Disk accounting excludes a Core datadir nested in the data directory.
- Re-measure with `tests/bench/bench_ibd.sh` after changing prefetch, cache or
  commit. Do not add a persistent index to `apply_block` without a benchmark
  and a migration plan.

### Processes — `docs/ARCHITECTURE.md`, `docs/OPERATIONS.md`

- Only an abnormal end (a signal nobody sent to stop it) is restarted. **An
  exit code is final**: never restart a configuration or integrity error.
- The node dies with its parent, so `kill -9` on the pid a script started
  still crashes the node; the recovery suites rely on it.
- A managed Core is adopted, not restarted. The managed Tor ends with e-lite
  and a Tor left by an older run is stopped at start (AUD-180).
- A new wallet-facing thread is spawned with a name from
  `el_model::sync::CONTAINED_THREADS`, and whatever it holds is released by a
  drop guard. Shared locks in `el-serve`/`el-rawtx` use
  `el_model::sync::Mutex`, not `std::sync::Mutex`. A contained panic logs
  thread and location, never the message.
- Bind listeners with `el_serve::bind_listener`, never a bare
  `TcpListener::bind` (AUD-181).
- A Core started by e-lite gets fitted `-dbcache`/`-par`/`-rpcthreads`, an
  explicit `-prune` and `-listenonion=0`; operator values win.
  `core.network = "private"` fails instead of starting Core on clearnet
  (AUD-136, `core_plan::network_args`).
- One Core datadir per network; two instances never share one.

### Serving wallets — `crates/el-serve/`, `docs/ELECTRUM_PROTOCOL.md`

- Never raise `protocol_max` until every method of that version and the ones
  before is implemented and tested. Protocol 1.6+ needs `server.version`
  first.
- Never answer a wallet early with an error to beat its timeout: Sparrow
  stores an unfetchable placeholder (AUD-045). A timeout is retried.
- A connection runs `transaction.get` lines beside its reader; never go back
  to one line at a time (AUD-160).
- Over-budget requests **wait** for a token; they are not rejected.
- The banner always starts with "e-lite". Never advertise another server's
  name.
- The plain listener binds loopback by default (AUD-147); TLS and the onion
  do not authenticate clients. An explicit `listen` is never rewritten.
- 1.7 scripthash and scriptpubkey subscriptions live in separate maps;
  outpoint status runs in a bounded pool, never in the notifier thread.
- Per-connection bounds stay: `server.ping` budget, outpoint candidates,
  batch size (AUD-167). Checkpoint proofs hash outside the index read lock.

### External data and privacy — `crates/el-rawtx/`, `docs/PRIVACY.md`, `docs/PRIVACY-TUNING.md`

- Nothing external is cached or served before it is verified against the
  index (txid, position, merkle branch to the indexed header). A source that
  fails verification is penalised.
- Scripts, addresses and xpubs never leave. A txid leaves only for a height
  Core has pruned; an unknown prune height keeps the whole-block path.
- Decoy wallets are stable across loads, shuffled with the real ones and
  never cached or served. Do not replace them with random txids.
- Public servers are not compiled in. A missing or invalid
  `public-servers.toml` means local data only: no hidden fallback list.
  Clearnet entries must be HTTPS; a plain Electrum entry only for `.onion` or
  a local server (AUD-171).
- Every download is a race over shuffled endpoints (`Resolver::race` in
  `crates/el-rawtx/src/race.rs`). Never go back to serial sources or to
  preferring the provider that answered last (AUD-135, AUD-160).
- Provider health depends on the failure (`crates/el-rawtx/src/health.rs`,
  AUD-155). Do not cool a provider for minutes after a slow circuit, and never
  cool one that was not contacted.
- One block download has one network budget; the wait for a slot must not eat
  it. Only a wallet failure is remembered, never a prefetch failure. The
  reqwest client timeout is per read: never use `RequestBuilder::timeout`.
- `RawtxError::NoSource` (nothing could be asked) stays distinct from
  `Unavailable` (every source failed).
- P2P must advertise `NODE_WITNESS` **and** request `MSG_WITNESS_BLOCK`, and
  validate the witness commitment before trusting a peer. Peers come from
  Core's outbound list, never inbound.
- Requests do not identify e-lite (AUD-133), and each Esplora request over Tor
  uses its own circuit.
- Logs never print a script hash, txid or the block hash of a wallet request
  (`bin/e-lite/tests/log_redaction.rs`). Debug lines are aggregate counts and latencies.
  INFO lines never change format; extra detail goes to `debug:`/`trace:`.
- Decoys and prefetch run outside the wallet's last download slot. Decoy-only
  work never changes eligibility. A verified result or an actual network
  failure of a real prefetch may update the wallet route's qualification;
  queued or canceled work must not. Prefetch is never correctness.
- External wallet traffic needs a fresh proof per route, transport and
  operation. Probe only public fixtures and do not relax txid operator
  diversity during an outage. Probes share the Tor stream governor.
- The rawtx cache is a transaction cache with a size cap; only
  `e-lite cache` may delete it, through `crates/el-rawtx/src/cachelayout.rs`.
- The readiness gate applies to a pruned Core only and is fail-closed by
  default.

### TLS — `docs/TOR.md`, `docs/OPERATIONS.md`

- One certificate per process, no SNI: one identity per instance.
- The generated identity is a local CA plus a leaf. Wallets trust `ca.crt`.
  Do not "fix" a Java `TrustAnchor` error by disabling verification or by
  marking the leaf as a CA.
- The leaf is re-signed with the same CA when the SAN set changes. Operator
  certificates are never overwritten; a half-configured pair is an error.
- TLS changes stay in sync across `config.example.toml`, `docs/TOR.md` and
  `docs/FEATURES.md`.

### Snapshots — `bin/e-lite/src/snapshot.rs`, `docs/OPERATIONS.md`

- Snapshots are optional and manual; the from-zero flow never needs one.
- Export always excludes the nested Core datadir from the index section
  (AUD-124). The Core section is an allowlist: wallets, keys and
  configuration are never copied (AUD-126).
- Import needs an empty target, validates in a staging directory and renames
  only when every check passed (AUD-141). An archive containing secrets or
  configuration is refused, not silently skipped (AUD-167).
- Cover: `tests/e2e/e2e_snapshot.sh`.

## Quality gates

Before claiming completion:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
scripts/check-docs.sh   # links, versions and source references in the docs
shellcheck install.sh scripts/*.sh tests/*.sh tests/lib/*.sh tests/e2e/*.sh tests/bench/*.sh tests/manual/*.sh
tests/run-e2e.sh        # every regtest suite; `tests/run-e2e.sh <suite>...` for a subset
```

Plus the install/update commands on a clean environment when the installer
changes. Say what was not run.

## Environment facts

- The build needs `build-essential clang libclang-dev pkg-config` (RocksDB
  bindgen), as in the `debian:12` CI container.
- The e2e suites need `bitcoind` in `PATH`. New suites source
  `tests/lib/e2e.sh` and use `tests/lib/e2e.py` for Electrum calls.
- Rust tests build a no-network rawtx config with
  `RawtxConfig::local_only(chain)` plus struct update syntax.
- A benchmark that samples the pid uses `--no-supervise`.

## Code review rule

Audit before rewriting. Open findings and residual risks go in
`docs/AUDIT.md`; once fixed, in `docs/audit/FIXED.md` with severity,
file:line, impact, test and fix commit. Fix P0 (data loss, security, crash)
and P1 (functional, race) before publishing docs that depend on them.
