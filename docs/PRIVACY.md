# Privacy and external sources

e-lite is local-first. The index answers headers, history, balances, UTXOs and
proofs without contacting third parties. External networks are used only when
the local Core cannot supply a raw transaction block (pruned old blocks) or when
repairing a prune gap.

## What stays local

- Wallet scripts, scripthashes, addresses, balances, histories, UTXOs.
- Electrum queries and subscriptions.
- Merkle/checkpoint proofs (computed from the local index and header cache).
- Mempool view, fee estimates and broadcasts come from the local/remote **Core**
  you configured, not from a public Electrum server.
- Logs and metrics do not print scripts, addresses, txids or wallet labels.

## What can leave the machine

Only raw transaction resolution and prune-gap repair can use external sources.
They request **whole blocks by block hash**:

```text
Core local block -> validated disk cache -> P2P/Esplora (whole block by hash)
```

A provider learns the set of block hashes fetched. It does **not** receive the
wallet's scripts, txids or query history. Every fetched block is verified before
use:

1. block hash equals the expected hash from Core's active chain;
2. header/chain continuity;
3. Merkle root recomputed;
4. BIP141 witness commitment checked for serving raw transactions.

A malicious source can withhold data, but cannot forge a block without breaking
these checks.

## Default providers

`[rawtx] esplora_urls = []` selects per-network defaults:

| Network | Default Esplora bases |
|---|---|
| mainnet | `https://blockstream.info/api`, `https://mempool.space/api`, mempool.space onion |
| signet | `https://mempool.space/signet/api` |
| testnet4 | `https://mempool.space/testnet4/api` |
| regtest | none |

P2P peers are discovered from Core's address manager (`getnodeaddresses`) or set
explicitly with `p2p_peers`. Only peers advertising full `NODE_NETWORK` service
are used for historical blocks; pruned/limited peers are rejected.

These defaults are **configurable**:

```toml
[rawtx]
enabled = true
cache_mib = 2048
decoys = 3
tor_socks = "127.0.0.1:9050"
allow_clearnet = false
use_p2p = true
use_esplora = true
p2p_v2 = true
esplora_urls = []          # empty = defaults above
p2p_peers = []             # empty = discover from local Core
# allow_clearnet = true is required for clearnet Esplora/P2P without Tor
```

## Tor and clearnet

- Default is **Tor-first, fail closed**: with `allow_clearnet = false`,
  clearnet endpoints are skipped when Tor is not configured, and the resolver
  reports that no source is available.
- When Tor is configured (`socks5h://127.0.0.1:9050`), both clearnet and `.onion`
  requests go through the proxy.
- `allow_clearnet = true` permits direct clearnet access if you explicitly
  accept the privacy trade-off. It is not the default.
- `decoys = 3` fetches extra random blocks after a real external fetch, so the
  provider's view is not a single “interesting” block. Set `decoys = 0` to
  reduce bandwidth at the cost of privacy.
- The validated block cache reduces repeated external fetches. Set
  `cache_mib = 0` to disable it; repair still works from P2P/Esplora when a
  source is available.

## P2P details

- Minimal Bitcoin P2P client: `version`/`verack`, `ping`/`pong`, `getdata` for
  `MSG_BLOCK` only.
- BIP324 v2 is attempted first; v1 is the fallback.
- Requests are distributed/randomised; up to four peers are tried per fetch.
- Tor is used for P2P when configured; otherwise clearnet requires
  `allow_clearnet = true`.

## Silent payments

BIP352 is **not implemented** in the current build. The opt-in gate returns an
explicit error and the feature is not advertised. If implemented in the future,
the scan private key must remain in memory only, never be logged or persisted,
and should be restricted to loopback/self-hosted use.

## Limits

- If a pruned block cannot be obtained from any configured source, raw
  `transaction.get` fails with an explicit `rawtx unavailable` error. History,
  balances and UTXOs still work from the index.
- External providers can be unavailable, rate-limited or blocked. Configure
  multiple `esplora_urls`/`p2p_peers` or use a full node if this matters.
- Electrum has an optional TLS listener (`[server] tls_listen`) for LAN/WAN
  wallets. TLS protects the transport but does not authenticate the wallet; use
  loopback, SSH/VPN, a firewall or a TLS reverse proxy. Core RPC has no TLS in
  e-lite.
