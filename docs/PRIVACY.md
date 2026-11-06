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

Public servers are **not compiled into e-lite**. The effective list lives in an
editable file next to the config:

```
~/.config/e-lite/public-servers.toml
```

On a first run, or when the file is missing, e-lite copies the shipped template.
Existing files are never overwritten. If the file and the template are both
missing, e-lite uses local data only (fail closed). Set
`[public_servers] generate = false` to disable automatic creation.

Shipped defaults (verified 2026-11-06):

| Network | Default Esplora bases |
|---|---|
| mainnet | `https://blockstream.info/api`, `https://mempool.space/api`, `http://mempoolhqx4isw62xs7abwphsq7ldayuidyx2v2oethdhhj6mlo2r6ad.onion/api` (Tor; verified 2026-11-06) |
| signet | `https://mempool.space/signet/api`, `https://blockstream.info/signet/api` |
| testnet4 | `https://mempool.space/testnet4/api`, `https://blockstream.info/testnet4/api` |
| regtest | none |

Edit that file to add, remove or reorder providers. e-lite only requests whole
blocks by hash (`GET {base}/block/{hash}/raw`); it never sends txids, scripts or
`spk_hint` to these servers. Clearnet entries must be `https://`; plain `http://`
is accepted only for `.onion` hosts and still requires Tor.

P2P peers are discovered from Core's address manager (`getnodeaddresses`) or set
explicitly with `p2p_peers`. Only peers advertising full `NODE_NETWORK` service
are used for historical blocks; pruned/limited peers are rejected.

`[rawtx] esplora_urls` remains a one-off override (CLI/`--set` or config); if it
is non-empty, it replaces the file for that run:

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
esplora_urls = []          # empty = use public-servers.toml
p2p_peers = []             # empty = discover from local Core
# allow_clearnet = true is required for clearnet Esplora/P2P without Tor

[public_servers]
# file = ""               # empty = <config dir>/public-servers.toml
generate = true           # create it from the shipped template when missing
```

## Tor and clearnet

- Default is **Tor-first, fail closed**: with `allow_clearnet = false`,
  clearnet endpoints are skipped when Tor is not configured, and the resolver
  reports that no source is available.
- e-lite runs its own Tor by default (`[tor] mode = "managed"`) and uses that
  SOCKS proxy for external rawtx/repair requests. There is no dependency on a
  system Tor daemon.
- When an external Tor proxy is configured (`socks5h://127.0.0.1:9050`), both
  clearnet and `.onion` requests go through the proxy.
- **Inbound onion:** the same managed Tor publishes Electrum as a v3 onion
  service. This hides the server IP and encrypts the transport, but anyone with
  the address can query it: e-lite does not authenticate clients or rate limit
  requests. Treat the address as a secret; use `[tor] mode = "off"` to disable
  the onion.
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
