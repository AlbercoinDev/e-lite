# Tor and onion access

e-lite can publish its Electrum listener as a Tor onion service so a wallet can
connect without opening a port, port-forwarding or using clearnet. Tor is
**managed by e-lite**: e-lite runs its own `tor` process with a generated
`torrc`; it does not edit `/etc/tor` and does not require a system Tor daemon.

## What you get

On `e-lite run` or `e-lite serve` with the default configuration:

- a v3 onion address, e.g. `abcdef...xyz.onion`;
- plain Electrum on `<addr>.onion:50001`;
- TLS Electrum on `<addr>.onion:50002` (self-signed certificate by default);
- the same Tor SOCKS proxy used for external rawtx/repair requests.

The address is stable across restarts as long as `<data-dir>/tor/onion` is
preserved. `e-lite onion reset` creates a new one.

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
plain = true                 # publish 50001
tls = true                   # publish 50002
tls_auto_cert = true         # self-signed cert if tls_cert/tls_key are empty
onion_port = 50001
onion_tls_port = 50002
socks_port = 0               # 0 = 9050 if free, else 19050+
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
  --tor-onion-port 50001 --tor-onion-tls-port 50002
```

Use `--no-tor` to disable Tor for one run.

If the Electrum listener binds a wildcard (`0.0.0.0:50001` or `[::]:50001`),
the onion target is automatically the loopback address (`127.0.0.1:50001` or
`[::1]:50001`): the wildcard socket accepts loopback connections, so this is
safe. A specific non-loopback address is refused unless you set a reachable
loopback `target_plain`/`target_tls`.

If `[server] tls_listen` is empty and `[tor] tls = true`, the effective TLS
listener becomes `127.0.0.1:50002`. If that port is busy, e-lite logs a warning
and publishes only the plain onion port.

### TLS certificate

When `server.tls_cert`/`server.tls_key` are empty and `tls_auto_cert = true`,
e-lite generates:

```
<data-dir>/tls/self-signed.crt
<data-dir>/tls/self-signed.key   (0600)
<data-dir>/tls/self-signed.san
```

The SAN covers `localhost`, `127.0.0.1` and the onion address once it exists.
The certificate is regenerated when the address changes; `CertWatcher` reloads
it without restarting. Operator-provided certificates are never overwritten.

Some wallets accept self-signed certificates for `.onion`; others do not. Plain
50001 remains available.

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

- plain: `tcp://<addr>.onion:50001`
- TLS: `ssl://<addr>.onion:50002`

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
- `managed Tor ... exited`: read `<data-dir>/tor/tor.log`; the last lines have
  Tor's configuration error.
- No onion address: check that `[tor] mode = "managed"`, `plain` or `tls` is
  true, and that the data directory is writable.
- Address changed unexpectedly: the onion keys were removed, the data directory
  moved, or `e-lite onion reset` was used.
- `onion reset` refuses: stop e-lite first (the managed Tor PID file is alive).
- TLS port busy: free `127.0.0.1:50002` or set `[tor] tls = false` / a custom
  `[server] tls_listen`.
