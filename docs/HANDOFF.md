# Session handoff

**Updated:** 2026-11-19 (alpha.29.26.4: wallet banner layout)
**Release:** https://github.com/AlbercoinDev/e-lite/releases/tag/v0.1.0-alpha.29.26.4

This is the short “where we are”. The invariants live in
[`AGENTS.md`](../AGENTS.md); the
current behaviour is the code plus `config.example.toml`. If this file
conflicts with a newer commit, the newer commit wins; update this file.
Per-release detail lives in [`CHANGELOG.md`](../CHANGELOG.md); this handoff
keeps only what a new session needs.

## Read order for a new session

1. `AGENTS.md`
2. `docs/README.md`
3. `docs/HANDOFF.md` (this file)
4. `docs/FEATURES.md` and `docs/ROADMAP.md`
5. `docs/AUDIT.md`
6. The specific docs for the task (`CLI`, `INSTALL`, `OPERATIONS`,
   `ARCHITECTURE`, `PRIVACY`, `PRIVACY-TUNING`, `TOR`, `TUNING`,
   `ELECTRUM_PROTOCOL`, `GETTING-STARTED`, `TESTS`, `RELEASING`).

Suggested prompt:

```text
Trabaja en este repo. Lee AGENTS.md, docs/README.md,
docs/HANDOFF.md, docs/AUDIT.md,
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
  `--with-core` allowlist); `import-snapshot` stages the extraction and
  renames it only after the archive, index and manifest checks pass (network,
  genesis, schema, tip, digest, optional `--expect-digest`). `--verify=full`
  runs the bounded `verifychain` on the restored Core *after* that rename, so
  a failure there leaves the directory in place and must not be read as a
  validated restore.
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
- **Defaults stay conservative**: Tor-only external fetches
  (`tor_fallback_clearnet = false`), fail-closed public providers, plain
  listener on loopback, manual pruning. `io_priority` and `private_broadcast`
  remain opt-in.
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

## Recent releases

Per-release detail lives in [`CHANGELOG.md`](../CHANGELOG.md); these lines are
only orientation for a new session.

- **29.26.4** — built-in wallet banner restructured: 50-column READY box with
  `e-lite (Electrum-Lite)` on its own line, a boxless INDEXING banner and the
  closing `Wallet ready to use. Enjoy and be careful.` / `Be patient my
  friend...`. Both start with a blank line so a wallet that prints
  `Server Banner: ` keeps the alignment. No data or configuration change.
- **29.26.3** — UTC timestamps on debug/trace plus richer wallet-recovery
  diagnostics (`conn=`, `wallet block:`, `index_miss` with Core's RPC code);
  no behaviour change. For the next cold Sparrow run, align the UTC timestamps
  with Sparrow's local time; `index_miss` does not mean *unconfirmed*.
- **29.26.2** — external wallet routes need a recent, valid, fast public probe
  per route, transport and operation; `/status.rawtx.qualification`. Expired or
  paused routes cannot requalify with wallet traffic. Behaviour and metrics in
  [OPERATIONS](OPERATIONS.md#external-wallet-route-qualification).
- **29.26.1** — AUD-201: per-txid batches and block downloads alternate at
  every freed Tor stream; a failed per-txid round is retried while the request
  still has time.
- **29.26.0** — wallet recovery rebuilt around shared transaction/block jobs;
  a temporary expiry of a confirmed transaction closes the connection instead
  of a per-item `-32000` (AUD-045); live reorg/catch-up reads wait for the next
  readiness notification.
- **29.20.0–29.25.2** — the Tor stream governor, wallet budgets, configurable
  prefetch, P2P budget, `doctor history`/`history-top` (AUD-198) and the
  SIGPIPE fix; see the CHANGELOG entries.

## Pending measurements

- Mainnet cold wallet load (Sparrow, cold, new wallet, Tor; pruned Core, LAN)
  with 29.26.2/29.26.3, comparing `preset = "balanced"` (default) and `"fast"`;
  decide whether the default preset changes. Use the UTC debug timestamps and
  follow `conn=` and `wallet block:`. Do not clear a live node's cache to
  test. The offline A/B (29.26.1 vs 29.26.2, 239 synthetic transactions) is in
  [benchmarks/provider-qualification.md](benchmarks/provider-qualification.md);
  the mainnet A/B remains pending.
- AUD-198: capture the `doctor history <hash>` output of the 1.3M-row script
  on the owner's node and decide the cause before any repair.
- AUD-199: decide whether the cheap per-txid path needs witness
  authentication (measurement, cache provenance and migration plan first).
- AUD-130 (schema v2/v3, index ~260 GiB → ~130-150 GiB): measure per-CF SST
  size and row counts on the owner's node before writing code.
- AUD-129 cold-scan latency on HDD/SATA (prefix filters landed; mainnet
  latency not measured).

## Decisions that still bind

- Schema v2 (AUD-130) will be a **from-zero reindex**, no in-place migration.
- Per-txid external fetch (`balanced`) is the default; `max` never sends a
  txid. Decoys are wallets, stable per node, spread over servers.
- `use_tor = false` is opt-in and never the default.
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


P2/P3: the verified near-term list is in `ROADMAP.md` (no provider penalty
on a reorg, subscription caps, `privacy.toml` per network, slowloris,
per-network e2e); open findings with their plan are in `AUDIT.md`
(AUD-130, 143, 166, 174, 176, 177, 179, 191, 198, 199).

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

The gates in [`TESTS.md`](TESTS.md) (fmt, clippy, workspace tests,
check-docs, shellcheck and the 11 regtest suites of `tests/run-e2e.sh`) pass on
the latest release; the release notes say what was not run. Full
signet/testnet4 acceptance (`tests/e2e/e2e_networks.sh`), real Tor and the
installer on a clean host are operator-run.
