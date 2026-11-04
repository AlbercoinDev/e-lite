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

Do not claim TLS, Electrum 1.7, BIP352 silent payments or external-provider
features that are not implemented. TLS is currently a roadmap item.

## Safety defaults

- `prune_mib = 1` for from-zero pruned IBD; `prune_mib >= 550` needs
  `allow_auto_prune = true`.
- Snapshots are optional, never required by the from-zero flow.
- Electrum is plain TCP and loopback by default.
- Core RPC is HTTP; use SSH/VPN for LAN/WAN.
- Rawtx external lookup is block-based, validated, Tor-first and decoy-enhanced.

## Audit and release

- Findings live in `docs/AUDIT.md`; P0/P1 before documentation claims.
- Required gates: fmt, clippy, workspace tests and relevant e2e.
- Releases must keep the one-line install assets (`install.sh`, `.deb`,
  tarball, `SHA256SUMS`) working.
