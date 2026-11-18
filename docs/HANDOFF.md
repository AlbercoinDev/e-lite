# Session handoff

**Updated:** 2026-11-18 (unreleased 29.25.2 work on branch `alpha.29.25.2-stream-budget`)
**Latest published release:** `0.1.0-alpha.29.25.1`
**Release:** https://github.com/AlbercoinDev/e-lite/releases/tag/v0.1.0-alpha.29.25.1

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

## Unreleased (branch `alpha.29.25.2-stream-budget`): real stream budget

- One governor permit per open Tor stream (race attempt, Electrum session,
  Esplora-tx request), held until the connection ends; hedges never wait for
  a stream. Per-txid sessions wait at most 3 s in all and take the stream
  last; local congestion requeues without spending servers (AUD-193, 196).
- Every `unavailable` failure of a confirmed transaction is retried inside
  the wallet deadline and the retry waits out the failure memory (AUD-194).
- Only the local Tor's refusal pauses the governor; 429 is per provider
  (AUD-195). The SOCKS "os error 11" is a circuit timeout.
- Bounded history reads, script hash in the `-32000` error, `e-lite doctor
  history|history-top` (read-only) for the open 1.3M-row script (AUD-198).
- Detailed `debug:` lines (per tx.get, stream, block, session; governor
  summary every 10 s) with an opaque per-process tag instead of hashes.
- Not done yet: the release (version bump, tag, app pin) and a real mainnet
  load to confirm it; see `CHANGELOG.md` "Unreleased".

## Previous change (alpha.29.25.1): Tor stream governor

- `rawtx.tor_streams` (default 8) caps every external stream (blocks, txid
  sessions, decoys) with wallet > prefetch > decoy priority; a refused stream
  (EAGAIN, 429) pauses new streams 0.5-4 s and any success lifts the pause.
- Onion clients (loopback peers) get 1800 s idle / 120 s write timeouts.
- Provider heartbeat keeps circuits warm while the node serves wallets.

## Previous change (alpha.29.25.0): the cheap path first

- `auto` runs two per-txid session attempts (6 s) before any block; the block
  gets 20 s of network and retries for transient failures inside the 45 s
  wallet deadline. `txid_race` (false by default) shortens the txid budget to
  2.5 s for an earlier block start.

## Previous change (alpha.29.24.1): subscribe retry and lock recovery

- `blockchain.scripthash.subscribe` retries once after 50 ms on a transient
  error instead of returning -32000 (Sparrow stores a null transaction).
- Poisoned mempool/index read locks are recovered with a warning.
- `debug: rpc error ... message=...` (redacted) for diagnosis.

## Previous change (alpha.29.24.0): cold wallet loads

- `auto` gives the per-txid path 1.5 s before starting the block; healthy
  loads download no blocks.
- `wallet_deadline_seconds` defaults to 35 s (queue ~27 s + 8 s network);
  a cold burst waits for a slot instead of failing a confirmed tx (AUD-045).
- One retry bypassing the failure memory before an error is returned.

## Previous change (alpha.29.23.0): P2P budget and provider quota

- `rawtx.p2p_blocks_per_peer` (1..4, default 3): per-peer in-flight budget.
- `rawtx.provider_quota` (0 or 1.0..4.0, default 1.5): long-term per-operator
  quota as a multiple of the fair share; over-quota operators rotate behind
  the others.

## Previous change (alpha.29.22.0): configurable history prefetch

- `rawtx.prefetch_history_blocks` (0..4096, default 0 = auto): block mode
  prefetches up to 256 blocks per history, per-txid mode only on demand; an
  explicit value enables it in both modes.
- The prefetch queue is sized from that value (256..4096).

## Previous change (alpha.29.21.0): concurrent race and wallet config

- `rawtx.path` (auto/tx/block), `wallet_deadline_seconds`, `txid_attempt_seconds`,
  `txid_race`, `decoy_sessions`, `sessions_total` and `jitter_ms`, validated,
  with defaults that keep the previous behaviour.
- `auto` races the in-flight block against the per-txid path (250 ms head
  start, one request deadline) and the first verified answer wins.
- Operator dedupe in the candidate set, HTTP/2 for Esplora, and
  `/status.rawtx.block_joins` + `race.wins_txid/wins_block`.

## Previous change (alpha.29.20.0): wallet rawtx budgets and block join

- Session budget 40 s -> 3 s; wallet txid budget 8 s; wallet block budget 20 s
  (12 s queue + 8 s network) instead of 60 s network + 120 s queue.
- A failed session records its real latency, not the whole budget.
- Adaptive batch size per server (50 -> halved down to 2) after a provider
  closes; `electrum.blockstream.info` accepts 20 txids and closes above that.
- A `transaction.get` of a pruned tx starts the block download on demand and
  joins the download already in flight instead of spending the txid budget.
- Decoy wallets use a quarter of the session slots; real sessions go first.

Measured over Tor: a 50-tx Electrum batch answers in 0.3-1.2 s and a 1.9 MB
block downloads at ~581 KiB/s. The old 40/60 s budgets were ~20x the p95.

## Previous change (alpha.29.19.2): closed output

A one-shot command whose stdout is closed early (`e-lite status --json | head
-1`) ends quietly by SIGPIPE instead of a FATAL abort (AUD-192); the daemons
still ignore SIGPIPE.

## Previous change (alpha.29.19.1): cleanup and reorganization

No behaviour change except the
all-in-one `--quick` fix; see `CHANGELOG.md`. Documentation has
one home per topic (`docs/README.md` is the map), the pipeline is split under
`bin/e-lite/src/pipeline/`, and the source layout is in `ARCHITECTURE.md`.

## Pending measurements

- Mainnet cold wallet load with this release (pruned Core, LAN Sparrow, over
  Tor), comparing `preset = "balanced"` (default) and `"fast"`; decide whether
  the default preset changes.
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
(AUD-130, 143, 166, 174, 176, 177, 179).

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
