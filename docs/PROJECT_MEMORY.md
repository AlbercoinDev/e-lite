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

Do not claim client authentication, rate limiting, Electrum 1.7, BIP352 silent
payments or external-provider features that are not implemented. Electrum TLS on
a separate listener is implemented; authentication and rate limiting are not.

## Safety defaults

- `prune_mib = 1` for from-zero pruned IBD; `prune_mib >= 550` needs
  `allow_auto_prune = true`. The prune gate evaluates every 10,000 safe blocks
  by default; low disk triggers an emergency prune.
- Snapshots are optional, never required by the from-zero flow.
- Electrum is plain TCP and loopback by default; TLS is an optional separate
  listener and does not authenticate clients.
- Core RPC is HTTP; use SSH/VPN for LAN/WAN.
- RocksDB memtables use a global `RAM/16` budget; cache and readers scale with
  real RAM, and every knob can be overridden from the CLI or `--set`.
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
