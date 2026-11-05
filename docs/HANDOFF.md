# Session handoff

**Updated:** 2026-11-05
**Latest published release:** `0.1.0-alpha.22`
**Release:** https://github.com/AlbercoinDev/e-lite/releases/tag/v0.1.0-alpha.22
**Main commit at handoff:** `112758c` (`alpha.22: fast bulk defaults, READY status and wallet banner`)

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

## Last audit/fixes (alpha.22)

- **Cache metric removed**; `--cache-mib` and the block cache remain.
- **`commit_sync = auto`**: `txids` in bulk/IBD, `always` live. `txids.dat`
  is always fsynced; startup truncates extras.
- **Core bulk pause + pre-warm**: pause during bulk, resume 5,000 blocks early
  so peers reconnect, then relative 20k guard.
- **Prune deferred above 50 GiB free**; normal cadence every 10k safe blocks;
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

1. Authentication/rate limiting for non-loopback Electrum. TLS encrypts the
   transport but does not restrict clients; document firewall/allowlist/VPN/Tor.
2. Core RPC TLS or a hard SSH/VPN requirement with tests.
3. Verify/close the audit residual risks in `docs/AUDIT.md` (snapshot archive
   limits, per-client threads, provider trust).

Performance/storage (P2, benchmark first):

- UTXO prefetch pipeline.
- HDD sequential/readahead mode.
- History compaction/refs/compression experiments.
- Adaptive controller v2 (queue/compaction signals).

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
(`launch_core`, `pruned_ibd`, `regtest` with plain + TLS Electrum, clean
startup/shutdown/memory assertions, `e-lite.log` presence and milestone checks)
passed on the alpha.22 code. Release artifacts and `SHA256SUMS` for alpha.22
were verified. Full signet/testnet4 acceptance is still operator-run; results
belong in `docs/GETTING-STARTED.md`/release notes when available.
