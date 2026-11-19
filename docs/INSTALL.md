# Install and update

e-lite is distributed as a static-ish Linux x86_64 build (Debian 12 baseline),
a `.deb`, and an installer/updater script. The installer never edits
`config.toml`, the index database or Core's datadir.

## Requirements

- Linux Debian 12+/LMDE 6+ (glibc ≥ 2.36), x86_64.
- **RAM, measured after cgroup limits:** mainnet needs an **8 GB class** machine
  (`>= 7500 MiB` usable); testnet4, signet and regtest need a **4 GB class**
  machine (`>= 3800 MiB`). `run`/`index` refuse to start below the minimum and
  print what they detected; `doctor` shows the host and cgroup figures. `serve`
  (existing index only) does not enforce it. `--allow-low-memory` exists for CI
  and regtest but is unsupported for real use.
- **File descriptors:** `ulimit -n` at least ~4096; 16384 recommended. e-lite
  caps RocksDB's `max_open_files` to the process limit and logs the effective
  value, but a low limit makes prevout reads re-open SSTs.
- `curl` and `ca-certificates` for the installer.
- Bitcoin Core 28+ for indexing/serving. `txindex` is not required.
- The e-lite index directory on a filesystem with enough free space. For a
  from-zero **test**, the acceptance harness requires ≥15 GiB on signet and
  ≥20 GiB on testnet4; those are test thresholds, not a capacity guarantee.
  Size a mainnet node from the index (~260 GiB) plus Core's datadir; see
  [`TUNING.md`](TUNING.md).
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
3. atomically replaces the binary (a user install writes a temporary
   `.e-lite.new` next to the target and renames it; `--system` installs the
   `.deb` with `dpkg`, which places the system binary and does not touch a
   per-user install in `~/.local/bin`);
4. downloads the official Tor expert bundle (pinned version, SHA-256 verified)
   to `<prefix>/lib/e-lite/tor`; `--without-tor` skips it. If the download fails
   or `tor` is already in `PATH`, Tor still works through the system binary;
5. optionally installs/updates the latest official Bitcoin Core from
   bitcoincore.org, verifying its published checksums; a failed download or
   checksum leaves Core as it is (the installer warns and continues);
6. leaves configuration, index data and Core's datadir untouched.

The Tor bundle is GPLv3 and is downloaded directly from the Tor Project; it is
not redistributed inside the e-lite release assets.

Default prefix: `~/.local/bin` (`E_LITE_PREFIX` overrides). Add it to `PATH`:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

## Pinned version

Pick any published tag (the leading `v` is not part of `--version`):

```bash
VERSION=0.1.0-alpha.29.19.2          # replace with the tag you want
curl -fsSL "https://github.com/AlbercoinDev/e-lite/releases/download/v$VERSION/install.sh" \
  | sh -s -- --version "$VERSION" --yes
```

With Core:

```bash
VERSION=0.1.0-alpha.29.19.2          # replace with the tag you want
curl -fsSL "https://github.com/AlbercoinDev/e-lite/releases/download/v$VERSION/install.sh" \
  | sh -s -- --version "$VERSION" --with-core --yes
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

`--system` requires root and replaces the system package; it does not update a
per-user install under `~/.local/bin`. Either path leaves configuration, index
and Core's datadir untouched.

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

Wallets connect to port `55022` (TLS, every interface) or, on the same host,
`127.0.0.1:55011` (plain) for mainnet. Plain is not offered on the network
unless `[server] plain_lan = true` / `--plain-lan` is set; testnet4 uses `55111`/`55122`, signet `55211`/`55222` and regtest
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

Disabling TLS alone does not keep the service local: with `tls = false` and no
explicit `[server] listen`, the plain listener moves to `0.0.0.0`. Set
`listen = "127.0.0.1:55011"` as well to stay on loopback. TLS encrypts the
transport but does not authenticate the wallet.

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
add, remove or reorder providers. Esplora entries (`esplora = [...]`) serve
whole blocks by hash and loose transactions by txid; Electrum entries
(`electrum = [...]`, `host:port:s`; plain `host.onion:port:t` only for
`.onion` or local servers) serve loose
transactions in batches. Clearnet Esplora entries must be `https://`;
`.onion` entries use Tor. If the file and
the template are both missing, e-lite uses local data only (fail closed). Set
`[public_servers] generate = false` in `config.toml` to disable automatic
creation.

## Privacy settings

How pruned transactions are fetched is set in `privacy.toml`, next to
`config.toml` (one file shared by all networks on the host). e-lite creates it
from the template on the first run and an update never overwrites it; delete it
and restart to get the template back. `e-lite privacy check` validates it and prints the effective
values. The default (`preset = "balanced"`) fetches loose transactions by
txid through Tor with decoy wallets; `preset = "max"` fetches only whole
blocks. Every setting and its range: [`PRIVACY-TUNING.md`](PRIVACY-TUNING.md).
The installer keeps a reference copy of both templates in
`~/.local/share/e-lite/` (refreshed on every update, never your own files).

## Update safely

```bash
# stop the old process (Ctrl+C is preferred)
pkill -TERM -o -f 'e-lite run'   # -o: the watching parent, which stops the node

curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh | sh

e-lite run --network signet --prune-mib 1
```

Core can stay running; e-lite adopts a Core it started previously. If you use an
external Core, `manage = "external"` and keep its RPC reachable.

> Upgrading from alpha.12: if your config still has `prune_mib = 550`, either
> set `prune_mib = 1` (recommended from zero) or set
> `[pruning] allow_auto_prune = true` intentionally.
