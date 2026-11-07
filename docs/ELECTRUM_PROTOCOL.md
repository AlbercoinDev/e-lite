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
| Max history entries per script | 100,000 |
| Max outpoint subscriptions per client | 1,000 |
| Max headers per request | 2,016 |
| Max clients | 32 (configurable) |
| Rate limit | 200 req/s, burst 5000, soft wait (never an error) |
| Request line limit | 1 MiB |
| Client idle timeout | 600 s |
| TLS handshake timeout | 10 s |

## Deliberate deviations

- **Lenient version negotiation**: future/skipped versions fall back instead of
  closing the connection. The negotiated version is returned.
- **`blockchain.estimatefee` never returns `-1`**: Core's relay/mempool fee is
  used as a fallback because some wallets treat `-1` as a send failure.
- **JSON-RPC batches are not implemented**: Electrum clients send one request
  per line; a batch array is answered with an invalid-request error.
- **No client authentication**: rate limiting is a throttle, not access
  control. Use firewall/VPN/Tor/onion.

## Performance and safety notes

- Whole raw blocks are fetched by hash. P2P requests `MSG_WITNESS_BLOCK`
  (BIP144), requires `NODE_WITNESS`, and validates hash, merkle root and the
  BIP141 witness commitment before returning a block.
- Concurrent requests for the same block share one download; decoys and prefetch
  are bounded. `rawtx.cache_dir` may point at tmpfs to keep block data off
  persistent disk.
- The external source deadline is 30 s and an inflight wait is capped at 45 s.
