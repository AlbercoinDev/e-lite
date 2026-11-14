# Privacy tuning: how e-lite fetches what a pruned node does not have

This document explains `privacy.toml`: what a public server can learn when
e-lite fetches transactions it does not have locally, how e-lite hides your
wallet, and what each setting changes. The short version is in the comments
of `privacy.toml` itself; `e-lite privacy check` shows the values in effect.

## When anything leaves the node

Only for a confirmed transaction whose block Core has already pruned. Every
other answer (history, balances, UTXOs, headers, merkle proofs, mempool,
transactions in blocks Core still has) comes from the local index and Core.
Requests that do leave the node:

- go **only through Tor** by default (never your IP; `allow_clearnet` is an
  explicit opt-in);
- never contain an address, a script, an xpub or anything the wallet sent to
  e-lite: only a txid (`tx` mode) or a block hash (`block` mode);
- are **verified against the local index** before anything is cached or
  served: the transaction's txid, its position in the indexed block and its
  merkle branch up to the indexed header. A lying server can withhold data,
  never forge it.

## The two modes

| | `tx` (default, `balanced` / `fast`) | `block` (`max`) |
|---|---|---|
| What is requested | loose transactions by txid | whole blocks by hash |
| Sources | public Electrum servers (one JSON-RPC batch per short session) and Esplora `/tx/:txid/raw` | Esplora `/block/:hash/raw` and Bitcoin P2P peers |
| Bytes per transaction | ~0.3-3 KB | ~1.5 MB per block |
| A deep wallet cold over Tor (owner's wallet: 239 transactions in ~144 pruned blocks) | tens of seconds | ~3.5 minutes |
| What a server learns | the txids it is asked for | the block hashes it is asked for |
| Fallback | whole blocks, when every per-txid server failed | none |

Measured over Tor on 2026-11-14 with the code of 29.15.0 and real mainnet
blocks: 480 transactions in **33 s**, and 907 transactions (the owner's
wallet plus three decoy wallets) in **50.6 s**, against ~3.5 minutes for the
owner's load in `block` mode.

## Why decoys must be wallets, not random transactions

A wallet's transactions are connected: one spends the outputs of another,
change goes back to the wallet's own addresses. Random decoy txids form no
such group, so whoever runs the server finds one large connected group among
loose noise, and that group is your wallet. The same weakness exists in
`block` mode with random decoy blocks: the group of transactions that appears
in almost every requested block stands out.

e-lite therefore loads **decoy wallets**:

1. **Look-alike addresses from the index.** For every real address that needs
   external transactions, e-lite picks `decoys.wallets` real addresses from
   its own index whose history has a similar size
   (`size_min_ratio`..`size_max_ratio` of yours) and overlaps the same era
   (`era_margin_blocks` around your heights). The search is local: a keyed
   seek in the index's history table.
2. **Grown like a wallet.** e-lite reads the outputs of each decoy address's
   transactions, in the address's own history order (not in arrival order),
   and adds the addresses they pay to (again from the local index) until the
   look-alike has as many transactions as your address (`decoys.growth`).
   Decoy wallets are connected groups too, and about as large as yours.
3. **Stable across loads.** Every choice depends only on the secret, your
   address and chain data, never on timing. The choice is keyed by a local secret,
   `<data-dir>/decoy.key` (created on the first run, mode 0600, never in a
   snapshot). Reloading the same wallet produces the same decoys. If decoys
   changed on every load, a server could intersect two loads and keep only
   the group that repeats: yours.
4. **Mixed and spread.** Real and decoy transactions travel at the same time,
   shuffled together, in short sessions (`sessions.size` txids each, one Tor
   circuit per session), spread so that no server receives more than
   `sessions.max_share` of any group (yours or a decoy) over the whole load
   (counted per connection, not per batch) and every group is split over at
   least `sessions.min_servers` servers.
5. **Verified, never served.** Decoy transactions are only parsed for their
   outputs. They are never cached and never answered to a wallet.

What a server sees: `k + 1` plausible wallets loading at once. Without other
information, its chance of picking yours is `1 / (k + 1)`.

What this does **not** protect against: an adversary who already knows
something else about you (for example, the moment you broadcast a
transaction, or one of your addresses) can test which of the `k + 1` groups
matches. Several colluding servers see more pieces of each group than one.
Tor hides your IP, not the timing of your loads. If any of that matters for
your threat model, use `preset = "max"`.

## Parameters

Every key is optional in `privacy.toml`; a missing key takes the preset's
value. A value outside its range, or an unknown key, stops e-lite at startup
with the file, the key and the range. `--set privacy.<section>.<key>=<value>`
overrides the file for one run (`e-lite --set privacy.decoys.wallets=5 run`).

| Key | Meaning | balanced | max | fast | Range |
|---|---|---|---|---|---|
| `preset` | starting values | — | — | — | `balanced`, `max`, `fast` |
| `mode` | `tx` or `block` | tx | block | tx | `tx`, `block` |
| `decoys.wallets` | decoy wallets per real wallet (k) | 3 | 3 | 1 | 0..8 |
| `decoys.size_min_ratio` | smallest decoy history vs yours | 0.5 | 0.5 | 0.5 | 0.1..1.0 |
| `decoys.size_max_ratio` | largest decoy history vs yours | 2.0 | 2.0 | 2.0 | 1.0..10.0 (not below min) |
| `decoys.era_margin_blocks` | era overlap margin | 52560 | 52560 | 52560 | 0..500000 |
| `decoys.seek_attempts` | index seeks per decoy address | 64 | 64 | 64 | 8..1024 |
| `decoys.growth` | grow decoys through their outputs | true | true | true | true/false |
| `sessions.size` | txids per Electrum session | 50 | — | 100 | 1..500 |
| `sessions.min_servers` | servers each group is spread over | 3 | — | 2 | 1..16 |
| `sessions.max_share` | largest share of a group per server | 0.34 | — | 0.5 | 0.1..1.0 |
| `sessions.parallel` | sessions open at once | 16 | — | 16 | 1..32 |
| `sessions.batch_window_ms` | gathering window before sending | 20 | — | 20 | 0..1000 |
| `blocks.decoys` | decoy blocks per real block download | `[rawtx] decoys` (1) | same | same | 0..8 |

Notes:

- `sessions.max_share` never goes below `1 / servers available`: with two
  servers configured, each one gets half.
- At most two sessions are open to one server at a time. Measured over Tor:
  32 parallel sessions over 11 servers lost 87 of 480 transactions to refused
  or dropped connections, 16 lost 2. That is why `sessions.parallel` defaults
  to 16 and stops at 32 (also the number of circuits Tor builds at once).
- Servers are ranked by their measured session time (with jitter), so slow
  or failing servers get fewer sessions; the spread still applies.
- Warnings (not errors): `decoys.wallets = 0` and `sessions.min_servers = 1`
  in `tx` mode, and fewer servers configured than `min_servers`.

## Recipes

- **Maximum privacy:** `preset = "max"`. No txid ever leaves the node; decoy
  blocks follow every block download. Slow over Tor for a deep wallet.
- **More hiding, same speed class:** `[decoys] wallets = 5`. Five decoy
  wallets: the chance of picking yours drops to 1/6; requests grow about 1.5x
  over the default.
- **Slow or busy Tor:** `[sessions] size = 40` and `parallel = 12`: fewer
  circuits to build, each one carries more.
- **Few servers:** add servers to `public-servers.toml` (`electrum = [...]`,
  `host:port:s`, onion servers as `host.onion:port:t`). `min_servers` is
  capped by what is configured.
- **No decoys (not recommended):** `[decoys] wallets = 0`. The fastest `tx`
  mode, and servers see your wallet's transactions without cover.

## Where things live

- `privacy.toml`: next to `config.toml` (one per network), or
  `[rawtx] privacy_file`. Created from the template on the first run and
  never overwritten by an update; delete it and restart to get the template
  back. The template ships in the release (`privacy.toml`) and in
  `/usr/share/e-lite/` or `~/.local/share/e-lite/`.
- Electrum servers: `public-servers.toml`, `electrum = [...]` per network. A
  file from before 29.15.0 without that key uses the shipped list (logged);
  `electrum = []` disables Electrum.
- `<data-dir>/decoy.key`: the decoy secret. Deleting it gives new decoys on
  the next load (and one load where a server could intersect old and new).
- `/status.rawtx.tx_mode`: sessions, real and decoy txids sent, verified,
  rejected, fallbacks. `e-lite privacy check [--json]`: effective values and
  where each one comes from.
