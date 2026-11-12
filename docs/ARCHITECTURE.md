# Architecture and execution modes

e-lite is a local index for Bitcoin Core plus an Electrum 1.4–1.7 server. Core
validates the chain; e-lite reads the active chain, stores history/UTXO/txids,
and answers wallets from that index. It never invents chain data.

## Components

```text
Bitcoin Core (RPC + blocks/)
        |
        |  getblockhash / getblock / blk*.dat
        v
e-lite indexer  --->  RocksDB index (history, UTXO, txids, blocks, undo)
        |                     |
        |                     +--> canonical snapshots (optional)
        |
        +--> Electrum plain 0.0.0.0:<network port> (55011 mainnet, ...)
        +--> Electrum TLS 0.0.0.0:<network TLS port> (55022 mainnet, ...)
        +--> /status and /metrics 127.0.0.1:<network metrics port> (55084, ...)
        +--> rawtx resolver: Core -> validated cache -> P2P/Esplora (Tor-first)
```

The plain and TLS Electrum listeners share the same JSON-RPC dispatcher. TLS
wraps each accepted socket with rustls; the certificate/key PEM files are
reloaded when they change on disk. There is no client authentication.

## Execution modes

| Mode | Command | Core | Block source | Use case |
|---|---|---|---|---|
| All-in-one | `e-lite run` | local managed/auto | files if available, else RPC | normal operation |
| Split | `e-lite index` + `e-lite serve` | local | files/RPC | build separately, serve later |
| Offline serve | `e-lite serve` | unavailable | none | serve an existing index |
| Managed Core | `[core] manage="managed"` | e-lite starts/stops it | local | from-zero pruned IBD |
| External local Core | `manage="external"` | already running | RPC (or read-only files) | operator-managed node |
| LAN Core | `rpc_url="http://LAN:8332"` | external | RPC only | maintenance |
| WAN Core | SSH/VPN tunnel + `rpc_url` | external | RPC only | maintenance; never raw RPC over Internet |
| Full node | `prune_mib=0` | local/remote | files/RPC | no pruning, larger disk |
| Pruned manual | `prune_mib=1` | local owned | files/RPC | from-zero pruned, default |
| Pruned automatic | `prune_mib>=550` + `allow_auto_prune=true` | local owned | files/RPC | advanced/opt-in |

### Local Core

`run` with `manage = "auto"` uses a running Core read-only or starts one. With
`managed`, e-lite requires loopback RPC and owns the process. A Core owned by
e-lite can be paused, pruned or stopped by e-lite; an external Core never is.

From zero:

```bash
e-lite run --network signet --manage managed \
  --core-datadir "$CORE" --data-dir "$DATA" --prune-mib 1
```

The indexer reads `blk*.dat` directly when `block_source=auto|files`, which is
the fast path. It builds a sidecar locator (`blockfiles.idx`) and handles Core's
XOR obfuscation without writing to the Core datadir.

### LAN Core

```toml
[core]
manage = "external"
rpc_url = "http://192.168.1.20:38332"
rpc_user = "..."
rpc_password = "..."
```

- The index uses RPC only; it cannot read remote `blk*.dat`.
- A remote **pruned** Core cannot be used for the first index (no historical
  blocks). Use a full node, a local owned Core, or a snapshot.
- Core RPC is HTTP and has no TLS in e-lite. Use a trusted LAN, firewall, SSH or
  VPN. Do not expose port 8332/38332 to the Internet.

### WAN Core

Use an SSH/VPN tunnel and point `rpc_url` at the local tunnel endpoint:

```bash
ssh -N -L 8332:127.0.0.1:8332 user@host
```

```toml
[core]
manage = "external"
rpc_url = "http://127.0.0.1:8332"
cookie_file = "/path/copied/.cookie" # or rpc_user/rpc_password
```

WAN is for keeping an existing index up to date, not for the first pruned IBD.

### Full vs pruned Core

- **Full** (`prune_mib=0`): no pruning; simplest and safest, needs the chain.
- **Manual pruned** (`prune_mib=1`, default): Core never prunes on its own. The
  prune gate is the only pruning authority and never cuts above
  `index - 288`. This is the supported from-zero pruned mode.
- **Automatic pruned** (`>=550` + explicit `allow_auto_prune`): Core prunes on
  its own; a 224-block lag guard and gap repair protect the index. Not the
  default.

### Reorgs

The index keeps undo for `reorg_window` (default 288). A reorg inside that
window rewinds, replays the new branch and resumes. A deeper reorg or a prune
gap that cannot be repaired from verified sources stops with a clear
`NEEDS_RECOVERY`/integrity error; e-lite never returns invented history.

## What comes from where

| Wallet query | Local index | Core | External |
|---|---|---|---|
| Headers / header subscription | yes | active-chain hashes | no |
| Script history, balance, UTXOs | yes | no | no |
| Mempool history/UTXOs/fees | overlay | `getrawmempool`, estimates | no |
| Merkle and checkpoint proofs | yes + headers | no | no |
| Broadcast | no | `sendrawtransaction` | no |
| Raw tx, unpruned block | no | `blk*.dat` / RPC | no |
| Raw tx, pruned block | optional cache | fails | P2P/Esplora, whole block by hash |
| Prune-gap repair | optional cache | fails | P2P/Esplora, whole block by hash |

Electrum method list and status are in [`FEATURES.md`](FEATURES.md). Privacy and
provider details are in [`PRIVACY.md`](PRIVACY.md).

## Index storage

- One RocksDB database with families: `meta`, `blocks`, `tx_occurrences`,
  `history`, `utxo`, `utxo_by_script`, `undo`.
- Flat `txids.dat` in chain order; commits append/fsync it before atomically
  advancing `committed_tip`.
- Undo records are retained for the reorg window. Catch-up builds undo for
  every block in memory and writes only the records inside the window of the
  committed tip, so historical blocks far below the tip never write undo.
- Snapshots are optional tar.gz exports of the database plus a manifest
  (network, genesis, tip, schema, txnum, canonical digest). They are never
  required for the from-zero flow.

## Adaptive tuning

`[index] profile = auto|hdd|ssd|nvme|manual` selects starting readers, commit
interval, batch and cache. A conservative controller can adjust readers,
commit/batch and cache using measured read/apply/commit latency, tx/s and cache
hit rate. `SIGUSR1` reloads hot fields. See [`TUNING.md`](TUNING.md).

## Failure/state model

States exposed by `/status`: `WAITING_FOR_CORE`, `INITIALIZING`,
`BUILDING_INDEX`, `READY`, `DEGRADED_RAWTX`, `CORE_UNAVAILABLE`,
`PAUSE_DOWNLOAD`, `PRUNING`, `REORG_RECOVERY`, `FATAL_INTEGRITY`.

- While `BUILDING_INDEX`, queries return a temporary “building” error, never a
  misleading empty result.
- If Core is down but an index exists, cached history/balances remain available.
- Disk pressure pauses Core downloads first; indexing continues until a critical
  floor. See [`OPERATIONS.md`](OPERATIONS.md).
