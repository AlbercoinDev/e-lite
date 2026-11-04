# Operations and troubleshooting

## First run from zero (signet example)

```bash
export PATH="$HOME/.local/bin:$PATH"

CORE="$HOME/.bitcoin-signet"
DATA="$HOME/.local/share/e-lite/signet"
mkdir -p "$CORE" "$DATA"

e-lite doctor --network signet --core-datadir "$CORE" --data-dir "$DATA"

e-lite run --network signet --manage managed \
  --core-datadir "$CORE" --data-dir "$DATA" --prune-mib 1
```

- `run` starts Core if needed, waits for RPC, initializes the DB and indexes.
- `prune_mib=1` is manual pruning: Core never deletes on its own; e-lite’s prune
  gate keeps the cut below `index - 288`.
- Electrum: `127.0.0.1:50001` (plain). Optional TLS listener: configure
  `[server] tls_listen/tls_cert/tls_key`. Status: `127.0.0.1:9184`.
- Stop with Ctrl+C/SIGTERM. With `stop_on_exit=true`, a Core started by e-lite is
  stopped too.

Disk guidance: signet ≥15 GiB free for a from-zero test, testnet4 ≥20 GiB.
Use `--min-free-gib` and a separate index disk for production.

## Modes

### All-in-one with a local Core

```bash
e-lite run --network signet --manage auto \
  --core-datadir "$HOME/.bitcoin" \
  --data-dir "$HOME/.local/share/e-lite/signet"
```

`auto` uses a running Core read-only; otherwise starts it. Use `managed` when
e-lite must own pause/prune/stop authority.

### Split index/serve

```bash
e-lite index --network signet --core-datadir "$CORE" --data-dir "$DATA" --no-follow
e-lite serve --network signet --data-dir "$DATA"
```

`serve` can run while Core is down; cached history/balances remain available.

### External local Core

```toml
[core]
manage = "external"
rpc_url = "http://127.0.0.1:38332"
cookie_file = "/home/USER/.bitcoin/signet/.cookie"
```

Never pauses/prunes/stops a Core it did not start.

### LAN Core

```toml
[core]
manage = "external"
rpc_url = "http://192.168.1.20:38332"
rpc_user = "..."
rpc_password = "..."
```

RPC-only; no remote block files. First pruned IBD is not supported against a
remote pruned node; use a full node/local owned Core/snapshot.

### WAN Core

```bash
ssh -N -L 38332:127.0.0.1:38332 user@host
```

```toml
[core]
manage = "external"
rpc_url = "http://127.0.0.1:38332"
```

Maintenance only; never expose Core RPC directly to the Internet.

## Electrum TLS (optional)

The plain listener (`[server] listen`, default `127.0.0.1:50001`) keeps working.
TLS is a separate, opt-in listener:

```toml
[server]
listen = "127.0.0.1:50001"
tls_listen = "0.0.0.0:50002"
tls_cert = "/etc/e-lite/tls/fullchain.pem"
tls_key = "/etc/e-lite/tls/privkey.pem"
```

The certificate is validated against the name the wallet uses, not the server
IP:

- a public-domain certificate works when the wallet connects to
  `electrum.example.com:50002`. On a LAN, use split-DNS so that name resolves to
  the LAN address;
- connecting to `192.168.x.x` or `127.0.0.1` with a public-domain certificate
  fails with a name mismatch (public CAs do not issue certificates for private
  IPs);
- loopback wallets should keep using plain `127.0.0.1:50001`.

A self-signed or internal-CA certificate also works if the wallet trusts it
(Sparrow uses the Java trust store, so importing a private CA is possible but
more work).

### Reusing a Let's Encrypt certificate

certbot's `privkey.pem` is usually `600 root`. Copy the files to a path e-lite can
read, or grant access explicitly. Example deploy hook:

```bash
certbot renew --deploy-hook 'install -d -m 700 /etc/e-lite/tls && \
  install -m 644 /etc/letsencrypt/live/electrum.example.com/fullchain.pem /etc/e-lite/tls/fullchain.pem && \
  install -m 600 /etc/letsencrypt/live/electrum.example.com/privkey.pem /etc/e-lite/tls/privkey.pem'
```

e-lite checks the PEM files every 60 s and reloads them when they change, so a
renewal does not need a restart. If the new PEM is invalid, it keeps the previous
certificate and logs a warning. Changing `tls_listen` or the configured paths
requires a restart; only the file contents are reloaded automatically.

### Verify and test

```bash
openssl s_client -connect electrum.example.com:50002 \
  -servername electrum.example.com </dev/null
```

In Sparrow/Electrum use the host and `50002` and enable SSL. There is **no client
authentication**: TLS encrypts the connection but does not restrict who can
connect. Use a firewall, SSH/VPN or Tor in front of any non-loopback listener.

### TLS reverse proxy alternative

If TLS is already terminated by nginx/haproxy/stunnel, leave e-lite plain on
loopback and proxy to it:

```nginx
stream {
    server {
        listen 50002 ssl;
        proxy_pass 127.0.0.1:50001;
        ssl_certificate     /etc/letsencrypt/live/electrum.example.com/fullchain.pem;
        ssl_certificate_key /etc/letsencrypt/live/electrum.example.com/privkey.pem;
    }
}
```

## Pruning

| `prune_mib` | Behaviour |
|---|---|
| `0` | full node, no pruning |
| `1` | manual pruning; e-lite is the only pruner (default from zero) |
| `>=550` | Core automatic pruning; requires `allow_auto_prune = true` |

Manual mode: the prune gate uses min(`index - 288`, readers, `tip - 288`) and
throttles requests. Core may return a lower height because it prunes whole files.

Automatic mode: a 224-block lag guard, fast tip checks and a bounded verified
gap-repair path protect the index. It is opt-in.

Snapshots are optional. A snapshot can restore an index on another host but is
not required by the from-zero flow.

## Disks and LXC

- Below `min_free_gib`, Core downloads pause; indexing continues so the prune
  gate can free space.
- Below `critical_free_gib`, indexing also pauses.
- Memory/cgroups control the adaptive cache/readers. In LXC, check:

```bash
cat /sys/fs/cgroup/memory.max /sys/fs/cgroup/memory.current
dmesg -T | grep -i -E 'oom|killed process'
```

If the process is OOM-killed, lower `cache_mib`/`readers` or set
`adaptive = false` while testing.

## Tuning

```toml
[index]
profile = "auto"       # auto | hdd | ssd | nvme | manual
adaptive = true
readers = 0            # explicit value locks adaptive readers
cache_mib = 0
commit_interval_ms = 0
```

Hot reload:

```bash
kill -USR1 "$(pgrep -f 'e-lite run')"
```

Cold fields (`prune_mib`, `block_source`, `reorg_window`, data dir/network)
require restart. Full details in [`TUNING.md`](TUNING.md).

## Monitoring

```bash
e-lite status
e-lite status --json
e-lite core status
curl -s http://127.0.0.1:9184/status | python3 -m json.tool
curl -s http://127.0.0.1:9184/metrics | grep -E 'e_lite_(lag|tuning|tx_per_sec|commit)'
```

Useful fields: `state`, `indexed_height`, `core_height`, `lag_blocks`,
`retention_blocks`, `core.pruneheight`, `disk_free_bytes`, `disk_low`, `tuning`.

## Troubleshooting

### `Killed` / OOM in LXC

Check `dmesg`/`journalctl -k`. Set explicit `readers`/`cache_mib`, or
`adaptive = false`. Alpha.15+ uses cgroup memory accounting and caps automatic
readers at 4/8/16 by profile.

### Index lags behind Core

If Core is in IBD, it can download faster than e-lite indexes. With manual
pruning this is safe: `pruneheight` stays below the index. It is not a gap. Wait
for Core to finish, or improve disk/CPU; see [`TUNING.md`](TUNING.md).

### Prune gap / `NEEDS_RECOVERY`

With `prune_mib=1` this should not happen. If it does (or an external pruned
Core), e-lite can repair a bounded gap from verified cache/P2P/Esplora if a
source is configured. Otherwise use a full node or restore a snapshot.
Do not delete the index first.

### Raw transaction unavailable

The block may be pruned and no external source/Tor is available. History,
balances and UTXOs still work. Configure `[rawtx]` sources/Tor or use a full
node.

### Core unavailable

e-lite keeps serving cached history and retries Core. `e-lite serve` works
offline. `WAITING_FOR_CORE` means no index initialization has happened yet.

### Electrum cannot connect

Check `127.0.0.1:50001`, that the SSL checkbox is off for the plain port, and the
firewall. For TLS use the configured `tls_listen` port, enable SSL, and connect
to the certificate's hostname (not a private IP). Do not expose the plain port to
the Internet: it has no TLS or authentication. The TLS listener also has no
client authentication.
