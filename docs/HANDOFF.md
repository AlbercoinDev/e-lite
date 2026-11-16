# Session handoff

**Updated:** 2026-11-16 (release of the code and documentation review)
**Latest published release:** `0.1.0-alpha.29.17.0`
**Release:** https://github.com/AlbercoinDev/e-lite/releases/tag/v0.1.0-alpha.29.17.0
**In `main`, not tagged yet:** `0.1.0-alpha.29.18.0` (the owner decides when to tag; see `RELEASING.md`)

This is the short “where we are”. The invariants live in
[`AGENTS.md`](../AGENTS.md) and [`PROJECT_MEMORY.md`](PROJECT_MEMORY.md); the
current behaviour is the code plus `config.example.toml`. If this file
conflicts with a newer commit, the newer commit wins; update this file.
Per-release detail lives in [`CHANGELOG.md`](../CHANGELOG.md); this handoff
keeps only what a new session needs.

## Read order for a new session

1. `AGENTS.md`
2. `docs/README.md`
3. `docs/PROJECT_MEMORY.md`
4. `docs/HANDOFF.md` (this file)
5. `docs/FEATURES.md` and `docs/ROADMAP.md`
6. `docs/AUDIT.md`
7. The specific docs for the task (`CLI`, `INSTALL`, `OPERATIONS`,
   `ARCHITECTURE`, `PRIVACY`, `TUNING`, `ELECTRUM_PROTOCOL`, `GETTING-STARTED`,
   `TESTS`, `RELEASING`).

Suggested prompt:

```text
Trabaja en este repo. Lee AGENTS.md, docs/README.md,
docs/PROJECT_MEMORY.md, docs/HANDOFF.md, docs/AUDIT.md,
docs/FEATURES.md y docs/ROADMAP.md. Continúa desde el estado actual;
no repitas contexto ni rehagas decisiones ya documentadas.
```

## Current state

- **One folder per network**: the index, TLS, Tor and logs live in
  `~/e-lite/<network>` and that network's Core datadir is
  `<data-dir>/bitcoin`. Core and e-lite find these defaults without flags.
  A legacy `~/.local/share/e-lite/<network>` with a RocksDB `CURRENT` is still
  used until it is moved (`OPERATIONS.md`).
- **Pruning**: from-zero pruned IBD uses `prune_mib = 1` (manual pruning); the
  prune gate keeps the cut at or below `index - 288` and evaluates every 5,000
  safe blocks (`min_gain_live_blocks = 144` at the tip). `prune_mib >= 550`
  requires `[pruning] allow_auto_prune = true`. Normal pruning keeps
  `prune_defer_free_gib = 20`; the deferral applies only while Core downloads
  or the index drains.
- **Snapshots are optional**, never required by the from-zero flow.
  `export-snapshot` is per network (manifest format 3, digest, optional
  `--with-core` allowlist); `import-snapshot` is atomic, refuses secrets and
  validates network/genesis/schema/tip/digest plus `--verify[=full]`.
- **Listeners**: plain/TLS/metrics are `55011/55022/55084` (mainnet),
  `55111/55122/55184` (testnet4), `55211/55222/55284` (signet),
  `55311/55322/55384` (regtest). TLS is on by default with a generated local
  CA plus leaf in `<data-dir>/tls/` (wallets trust `ca.crt`); the plain
  listener binds loopback unless `plain_lan`/an explicit `listen` says
  otherwise, and TLS then follows that host. Metrics stay on loopback.
  `[tor] mode = "managed"` publishes the same ports as a v3 onion. There is no
  client authentication: the onion address is the access secret, and the rate
  limiter (200 req/s, burst 5000) is a throttle, not auth.
- **Electrum 1.4–1.7** is negotiated per connection, with 1.6/1.7 methods,
  JSON-RPC batches, notifications and the documented deviations in
  `ELECTRUM_PROTOCOL.md`.
- **Rawtx is local-first** and external only for pruned data: Core/cache, then
  the per-txid path (`privacy.toml` preset `balanced`, the default: Electrum
  batches + Esplora `/tx/:txid/raw` over Tor with decoy wallets, every
  transaction verified against the index), then whole blocks by hash as
  fallback. `use_tor = false` is the explicit direct opt-out (no SOCKS at all;
  the provider sees the IP). The history prefetch is on by default and owes
  decoys. The cache is a 256 MiB verified-transaction cache under
  `<data-dir>/rawtx-cache`; `e-lite cache status|clear` is the only code that
  deletes it and never touches the index, Core, TLS or Tor.
- **Defaults stay conservative**: `scheduler = "overlap"`, Tor-only external
  fetches (`tor_fallback_clearnet = false`), fail-closed public providers,
  plain listener on loopback, manual pruning. `alternate`,
  `adaptive_memtables`, `prefetch_deadline_ms`, `io_priority`,
  `cgroup_io_weight` and `private_broadcast` remain opt-in.
- **Machine plan**: physical cores, cgroup CPU quota and disk class choose
  readers/jobs and Core `-par`/`-rpcthreads`/`-dbcache`; the memory budget is
  fitted to `MemAvailable`, not installed RAM. `doctor` and `/status.machine`
  print the plan.
- **Logs**: `info < debug < trace`, alpha default `debug`, tee'd to
  `<data-dir>/e-lite.log`. The `debug:` lines carry counts, latencies, heights
  and transport, never script hashes, txids or wallet block hashes.
- **CI/release are cached**: `ci` restores `target/` and the registry, cancels
  superseded runs and checks the release profile with `cargo check --release`;
  the real binary is built once in the `release` workflow, smoke-tested and
  published. Warm times are a few minutes.
- `docs/FEATURES.md` is the current-feature source of truth;
  `docs/ROADMAP.md` is future work only, and `docs/AUDIT.md` holds the open
  findings (`docs/audit/FIXED.md` is the fixed history).

## Last change (alpha.29.18.0): the 2026-11-15 review

Full write-up in Spanish: [`ANALISIS.md`](ANALISIS.md).

- **Fixed (AUD-169, P1)**: the reader teardown of a catch-up call could hang
  forever when the call ended early with the prepared window full (shutdown,
  disk pause, alternate scheduler, error). `bin/e-lite/src/reader_pool.rs`
  owns the readers now and always stops them.
- **Refactor, no behaviour change**: `catch_up_limited` (620 lines) is a short
  loop over `preflight`, `ReaderPool`, `CommitGroup`, `flush_group` and
  `account_call`; `lag_guard` and `index_loop` lost their repeated blocks.
- **Refactor, no behaviour change (2026-11-16)**: `tuning.rs` is now
  `bin/e-lite/src/tuning/` (`machine.rs`, `memory.rs`, `controller.rs`,
  `mod.rs`); `/metrics` is a table over the `/status` document. The ranked
  list of further simplifications (optional variants to decide or delete,
  CLI flags that duplicate `--set`, persistent readers, bounded server
  threads) is section 7.10 of `ANALISIS.md`.
- **`AGENTS.md` rewritten (2026-11-16)**: rules and pointers only; defaults
  and ranges live in `docs/`. Keep it that way: a new rule is one or two
  lines plus the document that explains it.
- **Fixed (AUD-170..172)**: client slots released by a drop guard, dead
  connections stop at once, request lines parsed once, bounded error log;
  `:t` Electrum entries only for `.onion`/local servers; `privacy check`
  origin; a failed snapshot export removes its partial archive.
- **Docs**: `AUDIT.md` holds only open findings (AUD-091..125 moved to
  `audit/FIXED.md`), `ROADMAP.md` points at the files that exist after the
  29.17.0 split, and `scripts/check-docs.sh` now fails when a document names a
  source file or an identifier that is no longer there.
- **Panic policy (AUD-173, owner decision 2026-11-16)**: release builds
  unwind; a panic hook contains panics in the wallet-facing threads
  (`el-client`, `el-line`, `el-batch`, `el-fetch`) and aborts for every other
  thread. Shared locks no longer poison.
- **Automatic restart without systemd (AUD-180)**: `run`/`serve`/`index` are
  a watching parent plus the node; an abnormal end is restarted (backoff,
  give-up rule), an exit code or a stop is final, Core is adopted, Tor never
  stays orphaned. `--no-supervise` keeps one process.
- **New open findings**: AUD-174 (threads per connection), AUD-175 (errors classified by
  message text), AUD-176 (snapshot digest cost), AUD-177 (commit holds the
  global lock), AUD-179 (reader pool per call). AUD-178 is closed: the
  owner keeps the lenient negotiation; AUD-140 (online export) is not planned.
- **Listener bind (AUD-181)**: a default port held as the source port of an
  outgoing connection (e-lite's own RPC calls included) no longer fails the
  start; the bind is retried for up to 75 s.

## Previous change (alpha.29.17.0)

Module reorganization and drift guards; no behaviour change. `main.rs` went
from 7,843 to 298 lines (`cli`, `setup`, `pipeline`, `serve`, `diagnostics`,
`tests`) and `el-serve/lib.rs` from 5,388 to 229 (`stats`, `wire`, `tls`,
`client`, `prefetch`, `shared`, `rpc`, `protocol`, `notify`, `server`,
`metrics`, `tests`). Two tests now guard documentation drift:
`config_example_matches_defaults` and `help_flags_are_documented`. Test-only
helpers use `#[cfg(test)]`; the unused `is_rotational` helper was removed. The
`SC2015` shellcheck finding that failed the 29.16.5 `ci` lint job is fixed.

## Previous change (alpha.29.16.5)

Repository and documentation hygiene only; no wallet behaviour change.

- `docs/spec/` (the pre-implementation specification, marked “not started”)
  was removed; the code is the contract and git history keeps the old package.
- `tests/` is organized by purpose: `tests/e2e/`, `tests/bench/`,
  `tests/manual/` and `tests/lib/`, with `tests/README.md`. Scripts resolve the
  repo root and helpers from their own location, so they run from any cwd.
- `HANDOFF.md` is back to a short state document (per-release history stays in
  `CHANGELOG.md`); `ROADMAP.md` lists only pending work; `AUDIT.md` separates
  open findings from the fixed history (`docs/audit/FIXED.md`).
- ADRs moved to `docs/adr/` with an index; benchmarks documented in
  `docs/benchmarks/README.md`; new `docs/TESTS.md` and `docs/RELEASING.md`.
- Documentation corrected against the code: current data paths in
  `README`/`INSTALL`/`GETTING-STARTED`/`OPERATIONS`, `privacy.toml` is one
  shared file (not per network), rawtx is per-txid by default, `CLI.md`
  defaults (`prefetch`, `decoys`, `tor_request_timeout_seconds`) and the
  update command that used undefined `$CORE`/`$DATA`.
- `scripts/check-docs.sh` checks relative links, anchors, version consistency
  and old test paths; CI runs it together with `shellcheck`.
- Version badge and pinned-version examples no longer point at old releases.

## Previous changes (summary)

- **29.16.4**: CI/release cache, `concurrency`, `cargo check --release` per
  push, smoke test before publishing.
- **29.16.3**: `[rawtx] use_tor` (default true) and `--no-rawtx-tor`;
  `/status.rawtx.path`; independent of the onion listener.
- **29.16.2**: shipped Electrum servers for signet/testnet4 (verified).
- **29.16.1**: `TxMode` joins its batcher on drop, `Resolver::tx_idle`, two
  flaky tests fixed.
- **29.16.0**: AUD-167 wallet-path hardening (import secrets, fail-closed
  providers, prune gate, recursive decoys, `server.ping` budget, outpoint cap).
- **29.15.0**: per-txid fetch with decoy wallets, `privacy.toml`, `privacy
  check`.

Full detail: `CHANGELOG.md`.

## Pending measurements

- Mainnet cold wallet load with this release (pruned Core, LAN Sparrow, over
  Tor), comparing `preset = "balanced"` (default) and `"fast"`; decide whether
  the default preset changes.
- AUD-130 (schema v2/v3, index ~260 GiB → ~130-150 GiB): measure per-CF SST
  size and row counts on the owner's node before writing code.
- AUD-129 cold-scan latency on HDD/SATA (prefix filters landed; mainnet
  latency not measured).
- ZFS ARC dynamic manager on the owner's box: warn-only until
  `e-lite arc-setup --apply` (root) grants write access to `zfs_arc_max`.

## Decisions that still bind

- Schema v2 (AUD-130) will be a **from-zero reindex**, no in-place migration.
- Per-txid external fetch (`balanced`) is the default; `max` never sends a
  txid. Decoys are wallets, stable per node, spread over servers.
- `use_tor = false` is opt-in and never the default.
- `scheduler = "overlap"` stays default until a measured Core-solo rate beats
  the ~6.7 blocks/s break-even for `alternate`.
- `prune_mib = 1` for from-zero pruned IBD; snapshots are manual only.
- Public providers fail closed; the shipped Electrum list never stands in for
  a missing or invalid `public-servers.toml`.
- e-lite must not depend on systemd (or any init system): restart after a
  fatal error is built into `run`/`serve`/`index`; boot-time start is left to
  the host.
- Release builds unwind; only the threads in
  `el_model::sync::CONTAINED_THREADS` may fail alone (AUD-173).
- The TLS identity is one CA + leaf per instance (no SNI); a domain
  certificate and `.onion` access are mutually exclusive per instance.

## Open risks / next priorities

P1 (before any public exposure or mainnet production claim):

1. Authentication for non-loopback Electrum. TLS encrypts and the rate
   limiter throttles; neither authenticates. Use firewall/allowlist/VPN/Tor.
2. Core RPC TLS or a hard SSH/VPN requirement with tests.
3. Close or document the residual audit risks (snapshot archive size limits,
   per-client threads, provider trust); see `AUDIT.md`.


P2/P3: the verified list (13 items left) is in `ROADMAP.md`; the short
version: leader-verifies-before-notify, Tor retry by blame, no provider
penalty on a reorg, interruptible pipeline wait, subscription caps,
`get_merkle` tree cache, one snapshot for verbose `transaction.get`,
`privacy.toml` per network, per-network e2e. New in `AUDIT.md`: AUD-174..179.

## Commands

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
scripts/check-docs.sh
tests/run-e2e.sh            # every regtest e2e suite, one build, summary
```

Hot reload:

```bash
pkill -USR1 -o -f 'e-lite run'   # -o: the watching parent, which passes it on
```

## Do not do without explicit approval

- Change the database schema without `schema_version` + migration/test.
- Re-enable automatic pruning by default or weaken the 288/224 safety margins.
- Expose metrics publicly.
- Expose Electrum without TLS or without a firewall/VPN/Tor access policy; TLS
  alone does not authenticate clients.
- Claim client authentication, that rate limiting is access control, or that
  an unimplemented feature (e.g. BIP352) is available.
- Delete an existing index/datadir to “fix” an error before checking
  `NEEDS_RECOVERY`/snapshot options.

## Known working baseline at handoff

See `ANALISIS.md` for what could not be run locally (real networks, real Tor,
the installer on a clean host).

On the `v0.1.0-alpha.29.18.0` tree: `cargo fmt --check`, `clippy -D warnings`,
`cargo test --workspace` (373 tests), `scripts/check-docs.sh`, `shellcheck`
over the shell scripts, and the full local regtest set: `e2e_defaults` (TLS
trust/hostname, loopback plain, `/status.rawtx.path`), `e2e_launch_core`,
`e2e_wait_core`, `e2e_external_pruned`, `e2e_pruned_ibd` (prune + SIGKILL),
`e2e_regtest` (plain + TLS, 1.6/1.7, Sparrow sequence), `e2e_resilience`,
`e2e_recovery`, `e2e_multi_instance` and `e2e_snapshot`. Full signet/testnet4
acceptance is still operator-run; record results in the release notes when
available.
