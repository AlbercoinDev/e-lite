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
  pre-warm peers; normal prune is deferred above 50 GiB free.
- `commit_sync = auto` relaxes the RocksDB fsync in bulk; `txids.dat` is always
  fsynced and live mode is strict.
- Electrum listens on `127.0.0.1:50001` (plain) by default. Optional TLS uses a
  separate listener (`tls_listen`). `[tor] mode = "managed"` runs e-lite's own
  Tor and publishes plain 50001 and TLS 50002 as a v3 onion service; TLS uses an
  auto-generated self-signed certificate when none is configured. Neither
  listener authenticates clients; the onion address is the access secret. Do not
  expose them publicly without a tunnel/firewall/access plan.
- Public servers are not compiled into the binary: they live in the editable
  `public-servers.toml` next to `config.toml`. A first run copies the shipped
  template; a missing file means local data only (fail closed), never a hidden
  fallback list. Only whole blocks are requested by hash, and clearnet entries
  must be HTTPS (`.onion` over Tor is the only plain-HTTP exception).
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
- The memory budget reserves `max(1024 MiB, RAM/8)` for the OS/Core/page cache,
  caps e-lite at 75 % of total RAM, fixes memtables at `RAM/8`, and grows/shrinks
  the block cache in 256 MiB steps under `MemAvailable` + page-cache pressure.
  `cache_mib` is a maximum, not a lock. Before Core resumes downloads after a
  bulk pause, the cache shrinks to the live budget.
- Core RPC is HTTP; LAN/WAN use requires SSH/VPN.
- RocksDB memory is budgeted globally (`RAM/8` for memtables within the memory
  budget, cache starts at `RAM/4` and adapts), background jobs are CPU-capped,
  and every knob is available via CLI/`--set`.
- `run`/`index`/`serve` tee console output to `<data-dir>/e-lite.log`
  (`--log-file`, `--log-max-mib`, `--no-log-file`).
- Raw transactions are local-first. External fallback requests whole blocks by
  hash, validates them, uses Tor by default and never sends wallet txids/scripts.

## TLS certificate model (one cert, no SNI)

- Every process serves a single PEM pair for all clients and both listeners (the
  TLS listener and the published onion TLS port). rustls is built with
  `with_single_cert`; there is no SNI or per-hostname certificate selection
  (`crates/el-serve/src/lib.rs`, `build_tls_config`). A wallet validates the cert
  against the name it connects to: a cert for `example.com` fails for `.onion`
  and the auto self-signed cert fails for any public hostname.
- The self-signed cert (`<data-dir>/tls/self-signed.*`) is generated only when
  both `[server] tls_cert` and `tls_key` are empty (and Tor is managed); its SAN
  is `localhost`, `127.0.0.1` and the onion address. Operator certificates are
  never overwritten or regenerated (`bin/e-lite/src/tor.rs`, `ensure_self_signed`).
- Consequence: a public domain certificate in `[server] tls_cert`/`tls_key`
  disables the auto onion certificate, and `.onion` wallets then get a hostname
  mismatch. Pick one identity per instance: auto self-signed (onion), or a
  domain cert with `[tor] tls = false` / plain-only onion, or an explicit
  `target_tls` design. Do not "fix" one by breaking the other blindly.
- Config validation: `tls_listen` requires both PEM files and must differ from
  `listen`; cert/key without `tls_listen` is an error. Managed Tor auto-enables
  `127.0.0.1:50002` when `tls_listen` is empty and the port is free.
- `CertWatcher` reloads changed PEMs by mtime without restart and keeps the old
  cert if the new pair is invalid (Let's Encrypt renewals need no restart).
- TLS only encrypts; there is no client auth or rate limiting (AUD-010). A
  public domain also needs a reachable bind (`0.0.0.0`/IP, not the default
  loopback) plus a firewall/VPN/allowlist plan.

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
