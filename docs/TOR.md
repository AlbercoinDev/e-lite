# Tor and onion access

e-lite can publish its Electrum listener as a Tor onion service so a wallet can
connect without opening a port, port-forwarding or using clearnet. Tor is
**managed by e-lite**: e-lite runs its own `tor` process with a generated
`torrc`; it does not edit `/etc/tor` and does not require a system Tor daemon.

## What you get

On `e-lite run` or `e-lite serve` with the default configuration:

- a v3 onion address, e.g. `abcdef...xyz.onion`;
- plain Electrum on the network plain port (`50011`-style defaults below);
- TLS Electrum on the network TLS port (self-signed certificate by default);
- the same Tor SOCKS proxy used for external rawtx/repair requests.

The onion mirrors the active network's ports, so instances do not collide:

| Network | Onion plain | Onion TLS |
|---|---:|---:|
| mainnet | 55011 | 55022 |
| testnet4 | 55111 | 55122 |
| signet | 55211 | 55222 |
| regtest | 55311 | 55322 |

The address is stable across restarts as long as `<data-dir>/tor/onion` is
preserved. `e-lite onion reset` creates a new one.

## Only the Electrum onion is managed

The generated `torrc` contains `SocksPort` and the Electrum `HiddenServicePort`
entries. It deliberately has **no `ControlPort`**, so e-lite cannot publish an
onion for Bitcoin Core's P2P listener. A Core started by e-lite therefore gets
`-listenonion=0`; otherwise Core would default to `-listenonion=1` and retry
`127.0.0.1:9051` forever, logging `Error connecting to address 127.0.0.1:9051`.

- A Core started by e-lite uses the managed Tor for **outbound** connections:
  `[core] network = "auto"` (default) adds `-onion=<SOCKS>` so Core reaches
  onion peers besides clearnet, and `network = "private"` routes everything
  through it (`-proxy`, `-onlynet=onion`, no listening, no DNS). See
  "Bitcoin Core's own network" in [`PRIVACY.md`](PRIVACY.md).
- `/status.tor` reports the effective SOCKS port (`socks_port`), whether it is
  managed by e-lite, and `control_port: 0`. Use that SOCKS address if you set
  Core's `-proxy` yourself (e-lite then leaves Core's network flags alone).
- `e-lite doctor` prints `core onion: disabled by e-lite` for a managed Core.
  If you force `-listenonion=1` in `[core] extra_args`, it warns that the
  managed Tor has no control port.
- An external Core is never changed by e-lite. Add `-listenonion=0` to its
  `bitcoin.conf` if you do not need its onion.
- Supporting Core's own onion would require a per-network control port with
  cookie authentication; it is not implemented and is not required for wallet
  access.

## Installation

The one-line installer downloads the official **Tor expert bundle** from the
Tor Project, verifies its pinned SHA-256 and installs it next to the e-lite
binary:

```
<prefix>/lib/e-lite/tor/tor
```

The default prefix is `~/.local/bin`, so the bundle normally lives in
`~/.local/lib/e-lite/tor/`. A system `.deb` install uses `/usr/lib/e-lite/tor/`.
No `apt`, root or system service is needed for a user install.

```bash
curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh | sh
```

If the download fails (offline install, proxy, unsupported platform), e-lite
falls back to `tor` in `PATH`. If neither exists, indexing still works; the
onion is unavailable and `e-lite doctor` prints an actionable note.

To skip Tor:

```bash
curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh \
  | sh -s -- --without-tor
```

The Tor expert bundle is GPLv3. The installer downloads it directly from the
Tor Project; e-lite does not redistribute the binary in its release assets.

## Configuration

```toml
[tor]
mode = "managed"             # managed | off
plain = true                 # publish the network plain port
tls = true                   # publish the network TLS port
tls_auto_cert = true         # deprecated: use [server] tls_auto_cert
onion_port = 0               # 0 = same as server.listen
onion_tls_port = 0           # 0 = same as server.tls_listen
socks_port = 0               # 0 = per-network (9050 mainnet, 55150/55250/55350),
                             # then 9050, then 19050+
binary = ""                  # "" = installer bundle, then `tor` in PATH
start_timeout_seconds = 120
# data_dir = ""              # default <data-dir>/tor
# target_plain = ""          # default server.listen; 0.0.0.0 maps to 127.0.0.1
# target_tls = ""            # default server.tls_listen; 0.0.0.0 maps to 127.0.0.1
```

CLI equivalents:

```bash
e-lite run --network mainnet --allow-mainnet \
  --tor-mode managed --tor-plain --tor-tls \
  --tor-onion-port 55011 --tor-onion-tls-port 55022
```

Use `--no-tor` to disable Tor for one run.

If the Electrum listener binds a wildcard (`0.0.0.0:55011` or `[::]:55011`),
the onion target is automatically the loopback address (`127.0.0.1:55011` or
`[::1]:55011`): the wildcard socket accepts loopback connections, so this is
safe. A specific non-loopback address is refused unless you set a reachable
loopback `target_plain`/`target_tls`.

`onion_port` and `onion_tls_port` default to `0`, meaning "use the same port as
the Electrum listener". With `--listen 0.0.0.0:51001 --tls-listen 0.0.0.0:51002`
the onion is published as `<addr>.onion:51001` and `<addr>.onion:51002`. Set an
explicit value to publish a different virtual port.

### TLS certificate and wallet trust

TLS does not depend on Tor. On the first `run`/`serve` with TLS enabled and no
operator certificate, e-lite generates a **per-instance local CA** and a server
certificate signed by it:

```
<data-dir>/tls/ca.crt           # local CA: import THIS in the wallet
<data-dir>/tls/ca.key           # CA private key (0600)
<data-dir>/tls/self-signed.crt  # server leaf + CA (the TLS chain)
<data-dir>/tls/self-signed.key  # leaf private key (0600)
<data-dir>/tls/self-signed.san  # regeneration marker
<data-dir>/tls/cert-model       # CA fingerprint marker
```

Java-based wallets (Sparrow) reject a self-signed **server** certificate used as
a trust anchor with `TrustAnchor ... is not a CA certificate`. That is why the
identity is split: trust `ca.crt` in the wallet; the served chain contains the
leaf and the CA, so the wallet can build and validate the path.

The leaf SAN covers `localhost`, `127.0.0.1`, `::1`, the machine hostname, the
non-loopback IPs of the local interfaces and the onion address once it exists.
When the SAN set changes (new LAN address or onion), the leaf is re-signed with
the **same** CA, so the wallet keeps trusting it. A corrupt pair or an old
single-cert installation is migrated on the next start. Files are written
`0600` atomically. Operator-provided certificates are never overwritten; if
`tls_auto_cert = false`, an existing valid pair is still served.

Verify the chain yourself:

```bash
openssl verify -CAfile <data-dir>/tls/ca.crt <data-dir>/tls/self-signed.crt
```

The startup log prints the leaf and CA fingerprints (SHA-256 of the DER
certificate, the value `openssl x509 -fingerprint -sha256` prints, without
colons).

If Tor is unavailable, the clearnet plain and TLS listeners keep working; only
the onion is missing. The same is true in reverse: `[tor] mode = "off"` serves
TLS with the generated certificate.

Some wallets accept self-signed certificates for `.onion`; others do not. Plain
Electrum on the network port remains available.

## Commands

```bash
e-lite onion show  --network mainnet --allow-mainnet
e-lite onion reset --network mainnet --allow-mainnet --yes
```

`show` prints the managed/socks state, the PID, the targets and the address(es).

`reset`:

1. refuses if the managed Tor is still running (stop e-lite first);
2. asks for confirmation (pass `--yes` for scripts);
3. moves `<data-dir>/tor/onion` to `<data-dir>/tor/onion.prev`;
4. removes the auto-generated certificate so it is regenerated with the new SAN;
5. prints the old address. The new address is generated on the next start.

`--purge` deletes the onion keys instead of keeping `onion.prev`.

For tests or a custom location, `E_LITE_TOR=/path/to/tor` overrides the binary
resolution before `[tor] binary` and the installer bundle.

## Connecting a wallet

Use the onion address shown by `e-lite onion show` or `/status`:

- plain: `tcp://<addr>.onion:<network plain port>` (55011 mainnet, 55111 testnet4,
  55211 signet, 55311 regtest)
- TLS: `ssl://<addr>.onion:<network TLS port>` (55022, 55122, 55222, 55322)

The address is case-insensitive and must be copied exactly (56 characters plus
`.onion`).

## Security

- A Tor onion hides the server IP and encrypts the transport, but it is still
  reachable by anyone who knows the address.
- e-lite **does not authenticate clients and does not rate limit** them. Use a
  dedicated instance, keep the address private, monitor clients, and put a
  firewall or access policy in front if the threat model requires it.
- Only loopback Electrum targets are published; e-lite refuses `0.0.0.0` and
  non-loopback targets.
- Onion keys live in `<data-dir>/tor/onion` (0700); protect and back them up if
  the address matters.
- `tor/` and `tls/` are not included in snapshot exports.
- Keep the Tor bundle updated: rerun the installer after e-lite updates.
  `e-lite doctor` shows the binary path and version.

## Troubleshooting

- `tor binary: not found`: install the bundle (`install.sh --with-tor`) or
  install the `tor` package.
- `managed Tor ... exited (exit status: 127)` + `libevent-2.1.so.7: cannot open
  shared object file`: the bundle's `tor` needs its own libraries. alpha.25.1+
  launches it with `LD_LIBRARY_PATH` pointing at them. If you are on an older
  build, export
  `LD_LIBRARY_PATH="$HOME/.local/lib/e-lite/tor${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"`
  or install `libevent-2.1-7t64`. `e-lite doctor` runs `tor --version` with the
  correct environment and prints the exact missing library.
- `managed Tor ... exited`: read `<data-dir>/tor/tor.log`; the last lines have
  Tor's configuration error.
- No onion address: check that `[tor] mode = "managed"`, `plain` or `tls` is
  true, and that the data directory is writable.
- Address changed unexpectedly: the onion keys were removed, the data directory
  moved, or `e-lite onion reset` was used.
- `onion reset` refuses: stop e-lite first (the managed Tor PID file is alive).
- TLS port busy: free the network TLS port, set `[server] tls_listen` to another
  address, or disable TLS for that instance (`[server] tls = false`).
