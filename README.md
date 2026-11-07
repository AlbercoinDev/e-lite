# e-lite

**Electrum 1.4–1.7 server for Bitcoin Core.** Query history, balances and UTXOs of
any `scriptPubKey` in the active chain without registering wallets. Core remains
the authority for consensus and the active chain; e-lite never invents data.

> **Status: `v0.1.0-alpha.29.3.1` — experimental, no funds.**
> Test on regtest/signet/testnet4 before mainnet.
> Electrum is plain TCP on loopback by default. Optional TLS is available on a
> separate listener; e-lite also runs its own Tor and publishes an onion service
> by default. There is no client authentication: treat the onion address as a
> secret and use a firewall/VPN/access policy where needed.

## Install or update (one line)

```bash
# e-lite only (configuration, database and Core datadir untouched)
curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh | sh
```

```bash
# e-lite + the latest official Bitcoin Core (recommended for a working setup)
curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh | sh -s -- --with-core --yes
```

Both commands install **or update**; they are intentionally one line. More
variants (pinned version, `.deb`, from source) are in
[`docs/INSTALL.md`](docs/INSTALL.md).

## First run from zero

```bash
export PATH="$HOME/.local/bin:$PATH"

CORE="$HOME/.bitcoin-signet"
DATA="$HOME/.local/share/e-lite/signet"

e-lite doctor --network signet --core-datadir "$CORE" --data-dir "$DATA"
e-lite run    --network signet --manage managed \
  --core-datadir "$CORE" --data-dir "$DATA" --prune-mib 1
```

Electrum/Sparrow: `127.0.0.1:50001` (plain). Optional TLS: configure
`[server] tls_listen`/`tls_cert`/`tls_key` and connect to that port with SSL.
`e-lite run`/`serve` also start a managed Tor and show an onion address:

```
[e-lite] Tor onion: <addr>.onion:50001 (plain), :50002 (tls)
```

Use `e-lite onion show` to print it and `e-lite onion reset` to rotate it (stop
e-lite first). `--no-tor` disables Tor for one run. See
[`docs/TOR.md`](docs/TOR.md).
Status and metrics: `http://127.0.0.1:9184/status` and `/metrics`. The same
console output is written to `<data-dir>/e-lite.log`. Wallets see a dynamic
INDEXING/READY banner; customize it with `[server] banner_file`.

All tuning is available without a config file:

```bash
e-lite run --network mainnet --allow-mainnet --profile nvme
e-lite run --set index.profile=ssd --set index.cache_mib=2048
```

See [`docs/CLI.md`](docs/CLI.md) for every flag and `--set` example.

## Documentation

| Document | Contents |
|---|---|
| [`docs/HANDOFF.md`](docs/HANDOFF.md) | Current state, release, open risks, next steps |
| [`docs/INSTALL.md`](docs/INSTALL.md) | Install/update, Core, `.deb`, source, first run |
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | Components, execution modes, local/LAN/WAN Core |
| [`docs/OPERATIONS.md`](docs/OPERATIONS.md) | Pruning, disks, remote Core, monitoring, troubleshooting |
| [`docs/PRIVACY.md`](docs/PRIVACY.md) | Local vs external data, Tor/clearnet, providers |
| [`docs/FEATURES.md`](docs/FEATURES.md) | What exists today, with status |
| [`docs/ROADMAP.md`](docs/ROADMAP.md) | Future features and proposed improvements |
| [`docs/CLI.md`](docs/CLI.md) | All command-line flags, `--set`, tuning examples |
| [`docs/TUNING.md`](docs/TUNING.md) | HDD/SSD/NVMe profiles, adaptive tuning, hot reload |
| [`docs/AUDIT.md`](docs/AUDIT.md) | Code audit findings and residual risks |
| [`docs/GETTING-STARTED.md`](docs/GETTING-STARTED.md) | Acceptance tests from scratch |
| [`docs/PROJECT_MEMORY.md`](docs/PROJECT_MEMORY.md) | Project invariants and documentation rules |

## Networks and requirements

- Networks: `regtest`, `signet`, `testnet4`, `mainnet` (explicit opt-in).
  Testnet3 is not supported.
- Linux Debian 12+/LMDE 6+, x86_64, glibc ≥ 2.36.
- Bitcoin Core 28+; `txindex` is not needed.
- From-zero **pruned** IBD requires a local Core owned by e-lite and read access
  to its `blocks/` directory.
- The index lives in its own directory; e-lite never writes to Core's datadir.

## Security notes

- Electrum listens on `127.0.0.1:50001` by default. Optional TLS on a separate
  listener (`tls_listen`) encrypts the connection, but there is **no client
  authentication**: restrict access with a firewall, SSH/VPN tunnel or Tor. Do not
  expose the plain port publicly.
- Core RPC is HTTP. LAN/WAN use requires SSH/VPN; never expose the RPC port.
- External rawtx lookup is local-first and Tor-first; it requests whole blocks by
  hash and validates them. Public providers live in the editable
  `public-servers.toml` (next to `config.toml`), never in the binary. See
  [`docs/PRIVACY.md`](docs/PRIVACY.md).

## Tests and release

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
tests/e2e_launch_core.sh
tests/e2e_pruned_ibd.sh
tests/e2e_regtest.sh
```

The all-in-one harness is in
[`scripts/e-lite-all-in-one.sh`](scripts/e-lite-all-in-one.sh); it can install,
run and write a report. The installer and artifacts are published in GitHub
Releases with `SHA256SUMS`.

## License

MIT.
