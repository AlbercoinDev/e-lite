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
   `CHANGELOG.md`.
6. Pruning, snapshots, network exposure, privacy and data-loss questions require
   evidence in `docs/AUDIT.md`; do not guess.

## Current defaults worth remembering

- From-zero pruned IBD uses `prune_mib = 1` (manual pruning). Core never prunes
  on its own; the prune gate keeps the cut below `index - 288` and evaluates
  pruning every 10,000 safe blocks by default (low disk bypasses it).
- `prune_mib >= 550` requires `[pruning] allow_auto_prune = true`.
- Snapshots are optional; they are never required by the from-zero flow.
- Long bulk catch-up pauses Core by default and resumes 5,000 blocks early to
  pre-warm peers; normal prune is deferred above 50 GiB free.
- `commit_sync = auto` relaxes the RocksDB fsync in bulk; `txids.dat` is always
  fsynced and live mode is strict.
- Electrum listens on `127.0.0.1:50001` (plain) by default. Optional TLS uses a
  separate listener (`tls_listen`). Neither listener authenticates clients; do
  not expose them publicly without a tunnel/firewall/access plan.
- Core RPC is HTTP; LAN/WAN use requires SSH/VPN.
- RocksDB memory is budgeted globally (`RAM/16` for memtables, cache `RAM/4`),
  background jobs are CPU-capped, and every knob is available via CLI/`--set`.
- `run`/`index`/`serve` tee console output to `<data-dir>/e-lite.log`
  (`--log-file`, `--log-max-mib`, `--no-log-file`).
- Raw transactions are local-first. External fallback requests whole blocks by
  hash, validates them, uses Tor by default and never sends wallet txids/scripts.

## Quality gates

Before claiming completion:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Plus the relevant integration tests (`tests/e2e_*.sh`) and the install/update
commands on a clean environment.

## Code review rule

Audit before rewriting. Record findings in `docs/AUDIT.md` with severity,
file:line, impact, reproduction/test, status and fix commit. Fix P0
(data-loss/security/crash) and P1 (functional/race) before publishing docs that
depend on the affected behaviour.
