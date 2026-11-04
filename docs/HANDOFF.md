# Session handoff

**Updated:** 2026-11-04
**Latest published release:** `0.1.0-alpha.18`
**Release:** https://github.com/AlbercoinDev/e-lite/releases/tag/v0.1.0-alpha.18
**Working version:** `0.1.0-alpha.19` (unreleased: log file, parallel decode, prune cadence)
**Main commit at handoff:** `PENDING`

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
   `PRIVACY`, `TUNING`, `GETTING-STARTED`).

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
- Electrum is plain TCP on `127.0.0.1:50001` by default. Optional TLS is
  available on `[server] tls_listen` with `tls_cert`/`tls_key`; the PEM files are
  reloaded when they change. There is no client authentication or rate limiting.
- Core RPC is HTTP; LAN/WAN requires SSH/VPN.
- Rawtx resolution is local Core -> validated cache -> P2P/Esplora (whole blocks,
  Tor-first, decoys, verified).
- Adaptive profiles and `SIGUSR1` hot reload are implemented:
  `auto|hdd|ssd|nvme|manual`, readers capped at 4/8/16, cgroup memory aware,
  explicit overrides locked. Every knob is also available via CLI and `--set`
  (see `docs/CLI.md`).
- RocksDB memory is budgeted globally (`RAM/16`, 64 MiB..1 GiB) across the 7 CFs;
  background jobs are capped by the CPU count.
- `run` waits for Core RPC before starting P2P/mempool/listeners; `serve` is the
  offline mode. Shutdown joins background threads before stopping Core.
- `docs/FEATURES.md` is the current-feature source of truth.
- `docs/ROADMAP.md` contains future features/improvements. Do not present them as
  available.

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
- **Metrics**: `decode_ms`, `wait_ms`, `cache_hit_known` (`?` when unknown);
  RocksDB lightweight statistics enabled.
- **Bulk guard is relative**: `--max-bulk-lag-blocks` bounds additional growth
  from the baseline, so a 300k backlog no longer freezes Core for the whole
  catch-up.
- **Readers capped by `2 × CPUs`** unless explicitly overridden.

Evidence: `CHANGELOG.md`, `docs/CLI.md`, `docs/TUNING.md`,
`cargo test --workspace`, `tests/e2e_regtest.sh` (log file + startup/shutdown),
`tests/e2e_pruned_ibd.sh` (prune gate with explicit 288 gain).

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
startup/shutdown assertions).

## Open risks / next priorities

P1 (before any public exposure or mainnet production claim):

1. Authentication/rate limiting for non-loopback Electrum. TLS encrypts the
   transport but does not restrict clients; document firewall/allowlist/VPN/Tor.
2. Core RPC TLS or a hard SSH/VPN requirement with tests.
3. Verify/close the audit residual risks in `docs/AUDIT.md` (snapshot archive
   limits, per-client threads, provider trust).

Performance/storage (P2, benchmark first):

- Parallel decode/txid/Merkle in reader threads.
- UTXO prefetch pipeline.
- HDD sequential/readahead mode.
- History compaction/refs/compression experiments.

Compatibility (P2/P3):

- Electrum 1.7, BIP352 silent payments (schema v2 needed).
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
- Claim client authentication/rate limiting, Electrum 1.7 or silent payments are
  implemented.
- Delete an existing index/datadir to “fix” an error before checking
  `NEEDS_RECOVERY`/snapshot options.

## Known working test baseline at handoff

`cargo fmt --check`, clippy, workspace tests and the local regtest e2e
(`launch_core`, `pruned_ibd`, `regtest` with plain + TLS Electrum and clean
startup/shutdown/memory assertions) passed on the alpha.18 code. Release
artifacts and `SHA256SUMS` for alpha.18 were verified. Full signet/testnet4
acceptance is still operator-run; results belong in
`docs/GETTING-STARTED.md`/release notes when available.
