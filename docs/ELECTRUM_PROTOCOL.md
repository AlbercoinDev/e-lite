# Electrum protocol matrix

This page is the implementation matrix for the Electrum server surface. It is
checked against the [Electrum protocol methods](https://electrum-protocol.readthedocs.io/en/latest/protocol-methods.html)
and the Bitcoin P2P rules used to obtain raw transactions (BIP141/BIP144).

## Version negotiation

| Version | Status | Notes |
|---|---|---|
| 1.4 / 1.4.1 / 1.4.2 | implemented | default range; `server.version` must be the first message only for 1.6+ |
| 1.4.3 | not advertised | Namecoin-only change |
| 1.5 | not advertised | skipped by the protocol |
| 1.6 | implemented | `headers` array, `mempool.get_info`, `estimatefee(mode)`, `broadcast_package`, first-message rule |
| 1.7 | implemented | `scriptpubkey.*`, `outpoint.*`, `server.ping`, `mempool.recent`, `testmempoolaccept`; `hash_function` removed from features |

Requests for a future or skipped version fall back to the newest implemented
version at or below the client maximum. Only a client that cannot speak 1.4 is
refused. The negotiated version is returned by `server.version`.

## Server methods

| Method | 1.4 | 1.6 | 1.7 | Notes |
|---|---:|---:|---:|---|
| `server.version` | yes | yes | yes | numeric negotiation; second call on a connection is rejected |
| `server.banner` | yes | yes | yes | file/template banner |
| `server.donation_address` | yes | yes | yes | empty string |
| `server.peers.subscribe` | yes | yes | yes | empty list (no peer discovery) |
| `server.add_peer` | yes | yes | yes | always `false` |
| `server.features` | yes | yes | yes | 1.7 omits `hash_function` |
| `server.ping` | yes | yes | yes | 1.7 `pong_len`/`data`; notifications are answered only when they have an id |

## Blockchain and mempool methods

| Method | 1.4 | 1.6 | 1.7 | Notes |
|---|---:|---:|---:|---|
| `blockchain.headers.subscribe` | yes | yes | yes | tip header + notifications |
| `blockchain.block.header` | yes | yes | yes | checkpoint proof when `cp_height != 0` |
| `blockchain.block.headers` | yes | yes | yes | 1.6+ returns `headers` array; max 2016 |
| `blockchain.estimatefee` | yes | yes | yes | 1.6 `mode`; never returns `-1` (Sparrow compatibility) |
| `blockchain.relayfee` | yes | yes | yes | Core `relayfee` |
| `blockchain.scripthash.get_history` | yes | yes | yes | confirmed + mempool, canonical order |
| `blockchain.scripthash.get_balance` | yes | yes | yes | confirmed + mempool delta |
| `blockchain.scripthash.listunspent` | yes | yes | yes | blockchain order, mempool at end |
| `blockchain.scripthash.subscribe` | yes | yes | yes | status `sha256`; reorg-safe |
| `blockchain.scripthash.unsubscribe` | 1.4.2 | yes | yes | boolean result |
| `blockchain.scripthash.get_mempool` | yes | yes | yes | mempool-only history |
| `blockchain.transaction.get` | yes | yes | yes | raw or verbose; confirmed path uses the verified resolver |
| `blockchain.transaction.get_merkle` | yes | yes | yes | position + branch |
| `blockchain.transaction.id_from_pos` | yes | yes | yes | optional merkle proof |
| `blockchain.transaction.broadcast` | yes | yes | yes | Core `sendrawtransaction` |
| `blockchain.transaction.broadcast_package` | no | yes | yes | stable `{success, errors}` shape |
| `blockchain.transaction.testmempoolaccept` | no | no | yes | maps Core `reject-reason` to `reason` |
| `mempool.get_fee_histogram` | yes | yes | yes | `[fee, vsize]` pairs |
| `mempool.get_info` | no | yes | yes | `mempoolminfee`, `minrelaytxfee`, `incrementalrelayfee` |
| `mempool.recent` | no | no | yes | last 10 mempool txs |
| `blockchain.scriptpubkey.get_history` | no | no | yes | `{history: [...]}` |
| `blockchain.scriptpubkey.get_balance` | no | no | yes | confirmed/unconfirmed |
| `blockchain.scriptpubkey.listunspent` | no | no | yes | `{utxos: [...]}` |
| `blockchain.scriptpubkey.get_mempool` | no | no | yes | `{history: [...]}` |
| `blockchain.scriptpubkey.subscribe` | no | no | yes | status notifications; separate from scripthash |
| `blockchain.scriptpubkey.unsubscribe` | no | no | yes | boolean result |
| `blockchain.outpoint.get_status` | no | no | yes | funder/spender status |
| `blockchain.outpoint.subscribe` | no | no | yes | notifications; resolved by bounded workers |
| `blockchain.outpoint.unsubscribe` | no | no | yes | boolean result |
| `blockchain.block.stats` | ext. | ext. | ext. | non-standard extension (Cormorant-style), not in the Electrum spec: Core `getblockstats(height)` passed through; fails for heights a pruned Core no longer has |

`blockchain.silentpayments.subscribe/unsubscribe` exists only as an explicit
opt-in error until a BIP352 indexer is wired; it is not advertised as available.

## Notifications

- Header notifications are sent only to clients that subscribed.
- Scripthash and scriptpubkey subscriptions are stored independently. A client
  that subscribes through both receives both notification methods for the same
  script.
- Scriptpubkey and outpoint subscriptions receive a notification on a reorg even
  when their status object is unchanged, as required for reorg correctness.
- Outpoint status may require Core or an external raw transaction. It is
  computed by a bounded worker pool (queue 256, 2 workers), never in the
  notifier thread. A full queue keeps the job pending and retries on the next
  notifier pass.
- JSON-RPC notifications (requests without `id`) are never answered.

## Limits and behaviour

| Item | Value |
|---|---|
| Max history entries per script | 100,000 (more is `-32000` naming the script hash; `e-lite doctor history <hash>` counts its rows) |
| Max outpoint subscriptions per client | 1,000 |
| Max headers per request | 2,016 |
| Max clients | 32 (configurable) |
| Rate limit | 200 req/s, burst 5000, soft wait (never an error) |
| Request line limit | 1 MiB |
| Client idle timeout | 600 s |
| TLS handshake timeout | 10 s |

## Deliberate deviations

- **Lenient version negotiation**: future/skipped versions fall back instead of
  closing the connection, and the client's minimum is not enforced (a client
  asking only for `1.4.3` gets `1.4.2`). The negotiated version is returned,
  so a strict client can close. Owner decision, AUD-041.
- **`blockchain.estimatefee` never returns `-1`**: Core's relay/mempool fee is
  used as a fallback because some wallets treat `-1` as a send failure.
- **JSON-RPC batches** are answered in order with one array (max
  `MAX_BATCH_REQUESTS` = 1000 items; empty or larger batches get `-32600`;
  notifications inside get no entry). Items run on up to 8 threads unless the
  batch contains `server.version` or an unsubscribe. Sparrow upgrades to
  batches only for servers it recognises by the `server.version` name
  (ElectrumX, electrs >= 0.9, Fulcrum >= 1.6, mempool-electrs, Frigate,
  Cormorant), so by default it sends one request per line to e-lite. With
  `[server] advertise_electrumx_compat = true` the banner becomes
  `e-lite <version> (electrumx-compatible)`: Sparrow matches "electrumx" by
  substring and switches to batches (pages of 100, matched by id), while the
  banner still starts with e-lite. Off by default until measured with other
  wallets.
- **No client authentication**: rate limiting is a throttle, not access
  control. Use firewall/VPN/Tor/onion.
- **`server.features.pruning` is always `null`**: the field describes history
  pruning, and e-lite keeps the full history of every script whatever Core
  prunes. A pruned Core only changes where a raw transaction comes from.
- **Plain-text upstream Electrum servers** (`host:port:t` in
  `public-servers.toml`) are accepted only for `.onion` and local hosts.

## Performance and safety notes

- A pruned transaction is fetched by txid (default) or as a whole block by
  hash, and verified against the index either way. P2P requests `MSG_WITNESS_BLOCK`
  (BIP144), requires `NODE_WITNESS`, and validates hash, merkle root and the
  BIP141 witness commitment before returning a block.
- Concurrent requests for the same block share one download; decoys and prefetch
  are bounded. `rawtx.cache_dir` may point at tmpfs to keep block data off
  persistent disk.
- One block download has a 60 s network budget (`EXTERNAL_BUDGET`) after a
  slot wait of at most 120 s; requests sharing it wait 5 s past the leader's
  deadline.
