# Session handoff

**Updated:** 2026-11-04
**Latest published release:** `0.1.0-alpha.16`
**Release:** https://github.com/AlbercoinDev/e-lite/releases/tag/v0.1.0-alpha.16
**Working version:** `0.1.0-alpha.17` (unreleased: optional Electrum TLS)
**Main commit at handoff:** `e9d4152` (`alpha.17: optional Electrum TLS listener with certificate reload`)

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
7. The specific docs for the task (`INSTALL`, `ARCHITECTURE`, `OPERATIONS`,
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
  explicit overrides locked.
- `docs/FEATURES.md` is the current-feature source of truth.
- `docs/ROADMAP.md` contains future features/improvements. Do not present them as
  available.

## Last audit/fixes (alpha.17)

- **Optional Electrum TLS listener** (`tls_listen`, `tls_cert`, `tls_key`):
  rustls 0.23 with TLS 1.2+, PEM chains (public CA certificates can be reused),
  10 s handshake timeout, same 1 MiB request-line cap.
- **Certificate reload on change**: the TLS listener checks the PEM files every
  60 s and swaps the rustls configuration without restarting the indexer; an
  invalid renewal keeps the previous certificate and logs a warning.
- **Transport refactor**: plain and TLS share one `Wire` path, so scripthash
  notifications can write to the same TLS connection. Idle timeout stays 600 s;
  notification latency is bounded by a 250 ms read poll.
- **`/status` and `/metrics`** expose `tls_listen`, `tls_handshakes` and
  `tls_handshake_failures`.
- **Docs**: TLS setup, Let's Encrypt reuse, split-DNS, permissions, renewal and
  the nginx `stream` alternative are in `docs/OPERATIONS.md`.

Evidence: `docs/AUDIT.md` (AUD-018), `CHANGELOG.md`, `cargo test --workspace`
and `tests/e2e_regtest.sh` (plain + TLS).

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
(`launch_core`, `pruned_ibd`, `regtest` with plain + TLS Electrum) passed on the
alpha.17 code. Full signet/testnet4 acceptance is still operator-run; results
belong in `docs/GETTING-STARTED.md`/release notes when available.
