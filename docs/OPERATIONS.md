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
- Electrum: `127.0.0.1:50001` (no TLS). Status: `127.0.0.1:9184`.
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

Check `127.0.0.1:50001`, no SSL checkbox, and firewall. Do not expose plain
Electrum to the Internet: there is no TLS/authentication yet.
