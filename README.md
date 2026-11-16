# e-lite

**Electrum 1.4–1.7 server for Bitcoin Core.** Query history, balances and UTXOs of
any `scriptPubKey` in the active chain without registering wallets. Core remains
the authority for consensus and the active chain; e-lite never invents data.

> **Status: `v0.1.0-alpha.29.18.0` — experimental, no funds.**
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

e-lite doctor --network signet
e-lite run    --network signet --prune-mib 1
```

One self-contained folder per network: the index, TLS, Tor and logs live in
`~/e-lite/<network>`, and that network's Core datadir is
`<data-dir>/bitcoin`. No `--data-dir`/`--core-datadir` is needed for the
defaults; use them only for a custom layout or an external Core.

Electrum/Sparrow: `0.0.0.0:55022` TLS on the network and `127.0.0.1:55011`
plain on this host only for mainnet (`--plain-lan` also offers plain on the LAN)
(`55111`/`55122` testnet4, `55211`/`55222` signet, `55311`/`55322` regtest).
TLS is on by default: on first start e-lite generates a local CA and a server
certificate signed by it in `<data-dir>/tls/`, and prints both fingerprints.
Import **`ca.crt`** into the wallet trust store (Java/Sparrow rejects a
self-signed server certificate as a trust anchor). Alternatively configure
`[server] tls_cert`/`tls_key` with a public-CA certificate.
`tls = false` (or `--no-tls`) disables TLS. `e-lite run`/`serve` also start a
managed Tor and show an onion address:

```
[e-lite] Tor onion: <addr>.onion:55011 (plain), :55022 (tls)
```

Use `e-lite onion show` to print it and `e-lite onion reset` to rotate it (stop
e-lite first). `--no-tor` disables Tor for one run. See
[`docs/TOR.md`](docs/TOR.md).
Status and metrics: `http://127.0.0.1:55084/status` and `/metrics` (one port
per network: `55184`, `55284`, `55384`). The same
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
| [`docs/HANDOFF.md`](docs/HANDOFF.md) | Current state, open risks, next steps |
| [`docs/INSTALL.md`](docs/INSTALL.md) | Install/update, Core, `.deb`, source, first run |
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | Components, execution modes, local/LAN/WAN Core |
| [`docs/OPERATIONS.md`](docs/OPERATIONS.md) | Pruning, disks, remote Core, monitoring, troubleshooting |
| [`docs/PRIVACY.md`](docs/PRIVACY.md) | Local vs external data, Tor/clearnet, providers |
| [`docs/PRIVACY-TUNING.md`](docs/PRIVACY-TUNING.md) | `privacy.toml`: per-txid mode, decoy wallets, presets and ranges |
| [`docs/FEATURES.md`](docs/FEATURES.md) | What exists today, with status |
| [`docs/ROADMAP.md`](docs/ROADMAP.md) | Future features and proposed improvements |
| [`docs/CLI.md`](docs/CLI.md) | All command-line flags, `--set`, tuning examples |
| [`docs/TUNING.md`](docs/TUNING.md) | HDD/SSD/NVMe profiles, adaptive tuning, hot reload |
| [`docs/ELECTRUM_PROTOCOL.md`](docs/ELECTRUM_PROTOCOL.md) | Methods, versions, notifications and limits |
| [`docs/TOR.md`](docs/TOR.md) | Managed Tor, onion access, TLS certificate and reset |
| [`docs/GETTING-STARTED.md`](docs/GETTING-STARTED.md) | Acceptance tests from scratch |
| [`docs/TESTS.md`](docs/TESTS.md) | Test suites, prerequisites, CI vs local |
| [`docs/RELEASING.md`](docs/RELEASING.md) | Version bump, tag, workflows and artifacts |
| [`docs/AUDIT.md`](docs/AUDIT.md) | Open findings and residual risks |
| [`docs/PROJECT_MEMORY.md`](docs/PROJECT_MEMORY.md) | Project invariants and documentation rules |

Architecture decisions are in [`docs/adr/`](docs/adr/README.md); dated
measurements in [`docs/benchmarks/`](docs/benchmarks/README.md).

## Networks and requirements

- Networks: `regtest`, `signet`, `testnet4`, `mainnet` (explicit opt-in).
  Testnet3 is not supported.
- Linux Debian 12+/LMDE 6+, x86_64, glibc ≥ 2.36.
- Bitcoin Core 28+; `txindex` is not needed.
- From-zero **pruned** IBD requires a local Core owned by e-lite and read access
  to its `blocks/` directory.
- The index lives in its own directory; e-lite never writes to Core's datadir.

## Security notes

- Electrum listens on `0.0.0.0:55022` (TLS) and, on this host only,
  `127.0.0.1:55011` (plain) by default for mainnet; `[server] plain_lan = true`
  offers plain on the LAN too. TLS encrypts the connection but there is **no client
  authentication**: restrict access with a firewall, SSH/VPN tunnel or Tor. If
  you only want local access, set `[server] listen = "127.0.0.1:55011"` (the TLS
  listener follows the same host). Each network has its own ports, so several
  instances can run side by side.
- Core RPC is HTTP. LAN/WAN use requires SSH/VPN; never expose the RPC port.
- External rawtx lookup is local-first and Tor-first by default. Pruned
  transactions are fetched by txid with decoy wallets (`privacy.toml` preset
  `balanced`), or whole blocks only with `preset = "max"`; every transaction is
  verified against the index. `[rawtx] use_tor = false` is an explicit opt-out
  that exposes this machine's IP to providers and peers. Public providers live
  in the editable `public-servers.toml` (next to `config.toml`), never in the
  binary. See [`docs/PRIVACY.md`](docs/PRIVACY.md).

## Tests and release

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
scripts/check-docs.sh
tests/e2e/e2e_defaults.sh
tests/e2e/e2e_launch_core.sh
tests/e2e/e2e_pruned_ibd.sh
tests/e2e/e2e_regtest.sh
```

See [`docs/TESTS.md`](docs/TESTS.md) for every suite and
[`docs/RELEASING.md`](docs/RELEASING.md) for the tag/publish flow. The
all-in-one harness is in
[`scripts/e-lite-all-in-one.sh`](scripts/e-lite-all-in-one.sh). The installer and
artifacts are published in GitHub Releases with `SHA256SUMS`.

## License

MIT.
