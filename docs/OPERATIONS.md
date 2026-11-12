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
- Electrum: `0.0.0.0:55022` (TLS) and loopback-only `127.0.0.1:55011` (plain)
  for mainnet (`plain_lan = true` puts plain on `0.0.0.0` as well);
  testnet4 uses `55111`/`55122`, signet `55211`/`55222`, regtest `55311`/`55322`.
  TLS is on by default and uses a generated self-signed certificate unless
  `[server] tls_cert/tls_key` are set. Status/metrics: `127.0.0.1:55084`
  (`55184`, `55284`, `55384`).
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
remote pruned node; use a full node/local owned Core, or restore a full
snapshot (`export-snapshot --with-core`) that already contains the chain.

### WAN Core

```bash
ssh -N -L 38332:127.0.0.1:38332 user@host
```

```toml
[core]
manage = "external"
rpc_url = "http://127.0.0.1:38332"
```

Maintenance only; never expose Core RPC directly to the Internet. e-lite warns
when `rpc_url` is non-loopback. Set `[core] require_loopback_rpc = true` to make
that a fatal error and force the tunnel:

```toml
[core]
require_loopback_rpc = true
```

## Startup and shutdown

`e-lite run` starts (or adopts) Core, waits for its RPC to answer (including the
`Loading block index…`/`Verifying blocks…` phase), and only then starts peer
discovery, mempool, notifications and the Electrum listeners. This avoids the
old `Connection refused` / `p2p discovery failed` noise. `e-lite serve` is the
offline mode: it serves an existing index even when Core is down, and it never
starts or stops Core.

`Ctrl+C`/`SIGTERM` stops mempool and notification threads, flushes the index and
then stops a Core owned by e-lite. The log ends with a local-time
`stopped at … (uptime …)` line.

Progress is printed after each commit: indexed height and percentage of Core's
`headers`, Core blocks/headers and percentage, lag, throughput and tuning
metrics. Every `[index] milestone_blocks` (default 10,000) or
`milestone_seconds` (default 6 h), whichever comes first, a line reports the
interval throughput and the disk usage of the e-lite index and Core datadir.
This works on every network, including small regtest/signet chains.

## Multiple instances (one per network)

Each network has its own data directory, Core datadir, TLS certificate, onion
key and ports, so four instances can run at once:

```bash
e-lite run --network mainnet  --allow-mainnet
e-lite run --network testnet4
e-lite run --network signet
e-lite run --network regtest
```

| Network | Plain | TLS | Metrics | Data / Core datadir |
|---|---:|---:|---:|---|
| mainnet | 55011 | 55022 | 55084 | `~/e-lite/mainnet` + `/bitcoin` |
| testnet4 | 55111 | 55122 | 55184 | `~/e-lite/testnet4` + `/bitcoin` |
| signet | 55211 | 55222 | 55284 | `~/e-lite/signet` + `/bitcoin` |
| regtest | 55311 | 55322 | 55384 | `~/e-lite/regtest` + `/bitcoin` |

**Bitcoin Core cannot serve two chains from the same datadir at once.** With
`manage = auto`/`managed`, each instance starts its own Core in
`<data-dir>/bitcoin`. To use an external Core per network, set
`[core] core_datadir` and `[core] rpc_url` in each instance (different RPC
ports). Never point two instances at the same Core datadir.

A managed Tor uses a per-network SOCKS port so instances can start together:
9050 on mainnet and 55150/55250/55350 on testnet4/signet/regtest. If the
preferred port is busy, e-lite falls back to 9050 and then 19050-19100. Set
`[tor] socks_port` to pin one explicitly.

Moving an existing installation: stop e-lite, then move the index and the Core
datadir into the new per-network folder (same filesystem, so it is a rename):

```bash
mkdir -p ~/e-lite
mv ~/.local/share/e-lite/signet ~/e-lite/signet        # if it exists
mv ~/e-lite/bitcoin ~/e-lite/signet/bitcoin            # Core signet datadir
```

Two legacy Core candidates (`~/.bitcoin` and `~/e-lite/bitcoin`) are treated as
ambiguous; e-lite then refuses to guess and asks for an explicit
`[core] core_datadir` instead of starting a fresh download.

## Electrum TLS

TLS is **on by default** on a separate listener. Without operator PEMs, e-lite
generates a **local CA** and a server certificate signed by it in
`<data-dir>/tls/`, and prints both fingerprints. Trust **`ca.crt`** in the
wallet, not `self-signed.crt`: the latter is the server chain (leaf + CA).
This is required by Java-based wallets such as Sparrow, which reject a
non-CA certificate used as a `TrustAnchor`. The SANs include `localhost`, the
loopback addresses, the hostname, the local LAN IPs and the onion address.
Disable TLS with `tls = false` or `--no-tls`.

Existing installations migrate automatically on the next start: e-lite creates
the CA, re-signs the leaf and rewrites `self-signed.crt`. Remove the old
trusted server certificate from the wallet and import the new `ca.crt`. Verify
with:

```bash
openssl verify -CAfile <data-dir>/tls/ca.crt <data-dir>/tls/self-signed.crt
```

```toml
[server]
listen = ""                   # empty = 127.0.0.1:<network plain port> (loopback only)
plain_lan = false             # true = plain on 0.0.0.0 as well
tls = true
tls_listen = ""               # empty = 0.0.0.0:<network TLS port>; with an explicit
                              # `listen`, empty = host of listen + TLS port
# tls_cert = "/etc/e-lite/tls/fullchain.pem"   # optional, operator-managed
# tls_key = "/etc/e-lite/tls/privkey.pem"
```

The certificate is validated against the name the wallet uses, not the server
IP:

- a public-domain certificate works when the wallet connects to
  `electrum.example.com:55022`. On a LAN, use split-DNS so that name resolves to
  the LAN address;
- connecting to `192.168.x.x` or `127.0.0.1` with a public-domain certificate
  fails with a name mismatch (public CAs do not issue certificates for private
  IPs);
- loopback wallets can use plain `127.0.0.1:55011` or trust the generated
  certificate for `127.0.0.1:55022`.

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
openssl s_client -connect electrum.example.com:55022 \
  -servername electrum.example.com </dev/null
```

In Sparrow/Electrum use the host and `55022` and enable SSL. There is **no client
authentication**: TLS encrypts the connection but does not restrict who can
connect. Use a firewall, SSH/VPN or Tor in front of any non-loopback listener.

### TLS reverse proxy alternative

If TLS is already terminated by nginx/haproxy/stunnel, leave e-lite plain on
loopback and proxy to it:

```nginx
stream {
    server {
        listen 55022 ssl;
        proxy_pass 127.0.0.1:55011;
        ssl_certificate     /etc/letsencrypt/live/electrum.example.com/fullchain.pem;
        ssl_certificate_key /etc/letsencrypt/live/electrum.example.com/privkey.pem;
    }
}
```

## Tor onion (managed)

`[tor] mode = "managed"` (the default) makes e-lite run its own Tor process and
publish Electrum as an onion service. The address is stable across restarts and
is created automatically on the first `run`/`serve`:

```
[e-lite] Tor onion: <56 chars>.onion:55011 (plain), :55022 (tls)
[e-lite] Tor ready (bootstrapped 100%)
```

- plain: `<addr>.onion:<network plain port>`
- TLS: `<addr>.onion:<network TLS port>`

The TLS port uses an automatically generated self-signed certificate unless
`[server] tls_cert`/`tls_key` are configured; the SAN includes the onion address.
TLS works the same when Tor is off or unavailable; only the onion is lost.

```bash
e-lite onion show  --network mainnet --allow-mainnet
e-lite onion reset --network mainnet --allow-mainnet --yes   # stop e-lite first
```

`reset` moves the onion keys to `<data-dir>/tor/onion.prev`; the next start
generates a new address. `tor/` and `tls/` are excluded from snapshots.

The onion has **no client authentication or rate limiting**. Keep the address
private, use a dedicated instance and a firewall/access policy if needed. See
[`TOR.md`](TOR.md) for installation, the bundled Tor, troubleshooting and the
security model.

## Pruning

| `prune_mib` | Behaviour |
|---|---|
| `0` | full node, no pruning |
| `1` | manual pruning; e-lite is the only pruner (default from zero) |
| `>=550` | Core automatic pruning; requires `allow_auto_prune = true` |

Manual mode: the prune gate uses min(`index - 288`, readers, `tip - 288`) and
throttles requests. By default it evaluates pruning every **5,000** safe blocks
(`min_gain_blocks`) and keeps a 20 GiB free-space margin
(`prune_defer_free_gib = 20`). Low disk still triggers an emergency prune.
Core may return a lower height because it prunes whole files.

During a long bulk catch-up, Core is paused by default
(`pause_core_during_bulk`). It resumes early, `core_resume_ahead_blocks` (5,000)
before the tip, so peers reconnect while e-lite finishes; the relative
`max_bulk_lag_blocks` (20,000) guard bounds additional separation afterwards.

Automatic mode: a 224-block lag guard, fast tip checks and a bounded verified
gap-repair path protect the index. It is opt-in.

Snapshots are optional. A snapshot can restore an index on another host but is
not required by the from-zero flow.

### Backing up and restoring index + Core (one network at a time)

```bash
# Backup (stop e-lite and Core first; the export refuses while Core runs)
e-lite export-snapshot --network mainnet --allow-mainnet \
  --output /backups/full-mainnet-<tip>.tar.gz --with-core

# Restore into an empty data dir and verify the pair
e-lite import-snapshot --network mainnet --allow-mainnet \
  --data-dir ~/e-lite/mainnet --input /backups/full-mainnet-<tip>.tar.gz \
  --verify=full --expect-digest <logical_snapshot_hash from the export>
```

- `--with-core` bundles the network's Core **chain data** (`<data-dir>/bitcoin`)
  under `bitcoin/` in the archive: only `blocks/`, `chainstate/` and
  `indexes/` of that network. Wallets (whatever their name), Core's onion/I2P
  private keys, `bitcoin.conf`, `settings.json`, mempool/peers/cookie/log
  files and other networks' subdirectories are never copied; the export
  prints them as "not copied" so you can back them up separately. A probe Core records tip, prune height and version in the
  manifest; the pair rules are enforced on both sides (`index tip <= Core
  tip`, `prune_height <= index tip - 288` undo window).
- Without `--with-core` the archive is index-only and never contains the
  Core datadir. The index section is stored (gzip level 0: its files are
  already compressed) and the Core section uses level 3; `--compress-level
  0..9` sets both. A SHA-256 per Core file is recorded by default
  (`--no-hash-files` skips it) and checked on import. Archives are manifest
  **format 3**: the manifest is also the first entry, so an archive for
  another network or a newer schema is refused before anything is extracted.
  Format 3 needs e-lite alpha.29.12.0 or later to import; formats 1 and 2
  still import.
- The export prints `logical_snapshot_hash`. Store it **apart from the
  archive** (password manager, notes): the digest inside the archive only
  detects corruption, while `--expect-digest` also detects an archive that
  was replaced or tampered with. Only restore snapshots you made yourself.
- The import extracts into a hidden `.<name>.import-partial` directory next
  to the target and renames it into place only after every check passed, so
  a failed or interrupted import leaves the target untouched; the next
  import removes the leftover.
- `--verify` checks the per-file sizes/hashes; `--verify=full` opens the
  restored Core isolated (`-connect=0 -listen=0`), checks both tips and runs
  a bounded `verifychain(2, 288)` before e-lite is started again.
- Run the command once per network: `mainnet`, `testnet4` and `signet` each
  produce their own archive with their own manifest identity checks.
- A restored instance starts with a new onion address (`tor/` is excluded)
  and no TLS change (`tls/` is excluded too).

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

## Logs

`run`, `index` and `serve` write the same lines shown on screen to
`<data-dir>/e-lite.log` (per network). Rotation keeps `e-lite.log.1` at 16 MiB by
default. Override with `--log-file`, `--log-max-mib` or `--no-log-file`, or:

```toml
[logging]
file = "/var/log/e-lite/e-lite.log"
max_mib = 16
```

Core's own log is separate: `<data-dir>/bitcoind.log` (or `[core] log_file`), and
`e-lite core logs` prints its tail.

`server.banner` uses built-in INDEXING/READY templates. Configure
`[server] banner_file` (or `--banner-file`) to customize the wallet banner; the
file is created with the default `[indexing]`/`[ready]` sections if missing.
The protocol has no banner push, so a connected wallet may need to reconnect.

## Monitoring

```bash
e-lite status
e-lite status --json
e-lite core status
curl -s http://127.0.0.1:55084/status | python3 -m json.tool
curl -s http://127.0.0.1:55084/metrics | grep -E 'e_lite_(lag|tuning|tx_per_sec|commit)'
```

Useful fields: `state`, `ready`, `at_core_tip`, `fully_synced`, `offline`,
`indexed_height`, `core_height`, `lag_blocks`, `retention_blocks`,
`core.pruneheight`, `disk_free_bytes`, `disk_low`, `memory`, `tuning`
(`read_ms_ewma`, `decode_ms_ewma`, `wait_ms_ewma`, `apply_ms_ewma`,
`commit_ms_ewma`, `tx_per_sec`, `blocks_per_sec`).

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

Check `127.0.0.1:55011`, that the SSL checkbox is off for the plain port, and the
firewall. For TLS use the configured `tls_listen` port, enable SSL, and connect
to the certificate's hostname (not a private IP). Do not expose the plain port to
the Internet: it has no TLS or authentication. The TLS listener also has no
client authentication.
