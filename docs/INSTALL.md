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
curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/download/v0.1.0-alpha.26/install.sh \
  | sh -s -- --version 0.1.0-alpha.26 --yes
```

With Core:

```bash
curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/download/v0.1.0-alpha.26/install.sh \
  | sh -s -- --version 0.1.0-alpha.26 --with-core --yes
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

Wallets connect to `127.0.0.1:50001` by default (plain TCP). TLS is optional and
uses a separate listener:

```toml
[server]
tls_listen = "127.0.0.1:50002"
tls_cert = "/etc/e-lite/tls/fullchain.pem"
tls_key = "/etc/e-lite/tls/privkey.pem"
```

The certificate must match the name the wallet uses (not a private IP). See
[`OPERATIONS.md`](OPERATIONS.md#electrum-tls-optional) for Let's Encrypt reuse,
renewal without restart, split-DNS and the nginx `stream` alternative. No TLS
configuration is needed for a local-only setup.

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
