# Project memory

This is the human-readable companion to [`AGENTS.md`](../AGENTS.md). It records
the rules that must survive refactors, releases and documentation rewrites.
For the current state and next priorities, read [`HANDOFF.md`](HANDOFF.md).

## Non-negotiable install/update UX

```bash
# Install or update e-lite only (config/database untouched)
curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh | sh

# Install or update e-lite + the latest official Bitcoin Core
curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh | sh -s -- --with-core --yes
```

Both lines must remain in `README.md` and `docs/INSTALL.md`. The second is the
recommended “it just works” path because e-lite needs Core.

## Documentation contract

Every documentation rewrite must cover:

- execution modes: `run`, `index`+`serve`, offline `serve`, local/LAN/WAN Core,
  full and pruned Core, managed/auto/external Core;
- what comes from the local index and what needs Core or an external source;
- public providers, configurability and privacy controls;
- current features (`docs/FEATURES.md`);
- future features and proposed improvements (`docs/ROADMAP.md`).

Do not claim client authentication, BIP352 silent payments or external-provider
features that are not implemented. Electrum 1.7, TLS on a separate listener and
the per-client rate limiter are implemented; client authentication is not.

## Safety defaults

- `prune_mib = 1` for from-zero pruned IBD; `prune_mib >= 550` needs
  `allow_auto_prune = true`. The prune gate advances in 5,000-safe-block steps
  with a 30 s minimum interval and a 20 GiB free-space deferral by default; low
  disk triggers an emergency prune.
- Snapshots are optional, never required by the from-zero flow.
- Electrum binds per-network ports (55011/55022/55084 mainnet, 55111/55122/
  55184 testnet4, 55211/55222/55284 signet, 55311/55322/55384 regtest): TLS on
  `0.0.0.0`, plain on loopback unless `plain_lan`/an explicit `listen`. TLS is enabled by default with a generated local CA and a server
  certificate signed by it; wallets must trust `ca.crt`, not the leaf. TLS does
  not authenticate clients. A managed Tor publishes an onion (plain +
  TLS) by default; the address is the access secret. The per-connection rate
  limiter delays requests instead of rejecting them, so it is a throttle, not
  access control.
- Core RPC is HTTP; use SSH/VPN for LAN/WAN.
- CPU and disk defaults come from the detected machine: physical cores (not
  logical SMT), cgroup CPU quota, and the data/Core disk class
  (NVMe/SATA/HDD/virtual/network). A 4C/8T SATA/ZFS box gets `readers=4`, `jobs=4`,
  Core `par=3`, `rpcthreads=4`. Explicit config wins.
- The memory budget uses **available** RAM, not installed RAM:
  `effective = min(total, MemAvailable - core_reserve)`. Memtables use a global
  `effective/16` budget (weighted per CF: `utxo` 6, `history`/`utxo_by_script`
  4) inside a ceiling of 75 % of effective, with `clamp(effective/8, 2-4 GiB)`
  reserved for the OS/Core/page cache. The profile cache ceiling is `total/4`
  for local SSD/unknown (2 GiB at 8 GiB, 4 at 16, 8 at 32) and `total/8` for
  HDD/network, capped by the effective room; `cache_mib` is a maximum. The hot
  controller follows
  `MemAvailable`, shrinks below the reserve and grows only with a proportional
  margin and slow prefetch. Memtables are fixed at startup; hot WBM resizing is
  future work. Every knob can be overridden from the CLI or `--set`.
- Bulk commits are bounded by 250k transactions, 512 blocks, raw bytes
  (`clamp(RAM/128, 64-256 MiB)`) or 20-60 s by disk class (minimum 8 blocks/60 s);
  live mode commits every 1.5 s. The prune gate uses the committed tip, so this
  does not risk pruned-block loss. Prune RPCs run in the background with a 120 s
  timeout.
- Core is paused above `max_bulk_lag_gib` (30) of unindexed data and resumed at
  `core_resume_ahead_gib` (7). `scheduler = "overlap"` (default) keeps e-lite
  indexing during Core downloads; `alternate` is opt-in and requires
  `prune_mib = 1` plus `pause_core_during_bulk = true`.
- `run`/`index`/`serve` keep an application log at `<data-dir>/e-lite.log` with
  the same lines as the console.
- Ready state is `READY` at Core's validated tip and `FULLY SYNCED` at the
  network headers; the wallet banner mirrors it.
- Rawtx external lookup is block-based, validated, Tor-first and decoy-enhanced.

## Audit and release

- Findings live in `docs/AUDIT.md`; P0/P1 before documentation claims.
- Required gates: fmt, clippy, workspace tests and relevant e2e.
- Releases must keep the one-line install assets (`install.sh`, `.deb`,
  tarball, `SHA256SUMS`) working.
