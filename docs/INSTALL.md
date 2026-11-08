# Install and update

e-lite is distributed as a static-ish Linux x86_64 build (Debian 12 baseline),
a `.deb`, and an installer/updater script. The installer never edits
`config.toml`, the index database or Core's datadir.

## Requirements

- Linux Debian 12+/LMDE 6+ (glibc ≥ 2.36), x86_64.
- `curl` and `ca-certificates` for the installer.
- Bitcoin Core 28+ for indexing/serving. `txindex` is not required.
- The e-lite index directory on a filesystem with enough free space:
  signet usually needs ≥15 GiB free for a from-zero test; testnet4 ≥20 GiB.
- For a from-zero pruned IBD: e-lite and Core on the same host, e-lite owning
  the Core process, and read access to Core's `blocks/` directory (including
  `xor.dat`).

## One-line install or update

These two commands are the supported primary UX. Both install **or** update.

```bash
# e-lite only
curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh | sh
```

```bash
# e-lite + latest official Bitcoin Core (recommended)
curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh | sh -s -- --with-core --yes
```

The installer:

1. downloads the latest release tarball or `.deb`;
2. verifies it against the release `SHA256SUMS`;
3. atomically replaces the binary;
4. downloads the official Tor expert bundle (pinned version, SHA-256 verified)
   to `<prefix>/lib/e-lite/tor`; `--without-tor` skips it. If the download fails
   or `tor` is already in `PATH`, Tor still works through the system binary;
5. optionally installs/updates the latest official Bitcoin Core from
   bitcoincore.org, verifying its published checksums;
6. leaves configuration, index data and Core's datadir untouched.

The Tor bundle is GPLv3 and is downloaded directly from the Tor Project; it is
not redistributed inside the e-lite release assets.

Default prefix: `~/.local/bin` (`E_LITE_PREFIX` overrides). Add it to `PATH`:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

## Pinned version

```bash
curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/download/v0.1.0-alpha.29.4.4/install.sh \
  | sh -s -- --version 0.1.0-alpha.29.4.4 --yes
```

With Core:

```bash
curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/download/v0.1.0-alpha.29.4.4/install.sh \
  | sh -s -- --version 0.1.0-alpha.29.4.4 --with-core --yes
```

## System-wide .deb

```bash
curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh \
  | sudo sh -s -- --system --yes
```

Or manually:

```bash
curl -fLO https://github.com/AlbercoinDev/e-lite/releases/latest/download/e-lite_amd64.deb
sudo dpkg -i e-lite_amd64.deb
```

## Only e-lite, use an existing Core

```bash
curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh \
  | sh -s -- --no-core --yes
```

## From source

```bash
sudo apt-get install -y build-essential clang libclang-dev pkg-config \
  curl ca-certificates
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"
cargo build --release -p e-lite
install -m 0755 target/release/e-lite "$HOME/.local/bin/e-lite"
```

## Verify

```bash
e-lite --version
sha256sum -c SHA256SUMS   # inside the release artifact directory
```

## Electrum connection

Wallets connect to `0.0.0.0:55011` (plain) or `0.0.0.0:55022` (TLS) for
mainnet; testnet4 uses `55111`/`55122`, signet `55211`/`55222` and regtest
`55311`/`55322`. TLS is enabled by default and a local CA plus a server
certificate signed by it are generated on first start in `<data-dir>/tls/`; the
startup log prints both fingerprints. Import `ca.crt` into the wallet trust
store (Sparrow/Java rejects a non-CA certificate as a trust anchor). Use `[server] tls = false` or `--no-tls` to disable it, or point
`tls_cert`/`tls_key` at an operator certificate:

```toml
[server]
tls_cert = "/etc/e-lite/tls/fullchain.pem"
tls_key = "/etc/e-lite/tls/privkey.pem"
```

The certificate must match the name the wallet uses (not a private IP). See
[`OPERATIONS.md`](OPERATIONS.md#electrum-tls) for trust, Let's Encrypt reuse,
renewal without restart, split-DNS and the nginx `stream` alternative. To keep
access local, set `[server] listen = "127.0.0.1:55011"`; TLS follows that host.

## Public providers

Public Esplora endpoints are not compiled into the binary. They live in the
editable file next to your config:

```
~/.config/e-lite/public-servers.toml
```

The installer copies the shipped template if the file does not exist; e-lite
also creates it on a first run. Existing files are never overwritten. Edit it to
add, remove or reorder providers; only whole blocks are requested, by hash.
Clearnet entries must be `https://`; `.onion` entries use Tor. If the file and
the template are both missing, e-lite uses local data only (fail closed). Set
`[public_servers] generate = false` in `config.toml` to disable automatic
creation.

## Update safely

```bash
# stop the old process (Ctrl+C is preferred)
kill -TERM "$(pgrep -f 'e-lite run')"

curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh | sh

e-lite run --network signet --manage managed \
  --core-datadir "$CORE" --data-dir "$DATA" --prune-mib 1
```

Core can stay running; e-lite adopts a Core it started previously. If you use an
external Core, `manage = "external"` and keep its RPC reachable.

> Upgrading from alpha.12: if your config still has `prune_mib = 550`, either
> set `prune_mib = 1` (recommended from zero) or set
> `[pruning] allow_auto_prune = true` intentionally.
