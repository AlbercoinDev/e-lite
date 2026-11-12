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
- Logs and metrics do not print scripts, addresses, txids, wallet labels or
  the block hash of a wallet request: failures are logged by height and error
  class, and any 64-hex value in an error is replaced by `<hash>`.
- External requests do not name e-lite: HTTP requests use a generic
  `User-Agent` (`Mozilla/5.0`) and the P2P `version` message a common Core
  subversion. Behaviour (one `getdata`, then disconnect) can still be
  fingerprinted by a determined peer.

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
cache_mib = 256
# cache_dir = "/dev/shm/e-lite-rawtx"  # tmpfs: no disk, counts as RAM
decoys = 1
tor_socks = "127.0.0.1:9050"
allow_clearnet = false
tor_fallback_clearnet = true
request_timeout_seconds = 8
provider_cooldown_seconds = 300
max_parallel_fetches = 4
p2p_timeout_seconds = 8
use_p2p = true
use_esplora = true
p2p_v2 = true
esplora_urls = []          # empty = use public-servers.toml
p2p_peers = []             # empty = discover from local Core
# allow_clearnet = true forces clearnet; tor_fallback_clearnet uses it only if Tor fails

[public_servers]
# file = ""               # empty = <config dir>/public-servers.toml
generate = true           # create it from the shipped template when missing
require_ready = true      # run + pruned Core: refuse wallets until a provider answers
open_without_provider = false  # true: open with local data after ready_timeout_seconds
```

## Tor and clearnet

- Default is **Tor-first with clearnet fallback**: `tor_fallback_clearnet = true`
  tries Tor first, then the clearnet provider if the Tor proxy is unavailable or
  a Tor request fails. The fallback exposes the operator IP to that provider; set
  `tor_fallback_clearnet = false` to stay fail-closed. `allow_clearnet = true`
  makes clearnet a normal source instead of a fallback.
- Every Esplora request over Tor uses a fresh random SOCKS username. Tor
  isolates streams by SOCKS credentials (`IsolateSOCKSAuth`, on by default),
  so each block is fetched over its own circuit and a provider cannot group
  the blocks of one wallet by exit IP. Timing and the last-good-provider
  preference still let a single provider see most of them.
- e-lite runs its own Tor by default (`[tor] mode = "managed"`) and uses that
  SOCKS proxy for external rawtx/repair requests. There is no dependency on a
  system Tor daemon.
- When an external Tor proxy is configured (`socks5h://127.0.0.1:9050`), both
  clearnet and `.onion` requests go through the proxy.
- **Inbound onion:** the same managed Tor publishes Electrum as a v3 onion
  service. This hides the server IP and encrypts the transport, but anyone with
  the address can query it: e-lite does not authenticate clients; rate limiting
  is only a throttle, not access control. Treat the address as a secret; use
  `[tor] mode = "off"` to disable the onion.
- With a **pruned** Core, `run` waits for at least one provider to answer
  `/block-height/0` and serve the genesis raw block with a matching header hash
  before opening the Electrum listeners (`require_ready = true` by default).
  This prevents a wallet from receiving a history it cannot fully resolve
  (Sparrow stores a failed `transaction.get` as an unfetchable transaction).
  An unpruned Core serves every raw transaction locally, so the listeners open
  at once. `open_without_provider = true` opens them with local data after
  `ready_timeout_seconds`; e-lite then re-probes every 60 s with the same
  wallet-free requests until a provider answers.
- A provider that fails a Tor or direct request is skipped temporarily; the
  last provider that answered is retried first and cooled for at most 30 s. If
  every provider is cooling down, one request is still attempted rather than
  failing the wallet's history/tx load.
- `allow_clearnet = true` permits direct clearnet access if you explicitly
  accept the privacy trade-off. It is not the default.
- `decoys = 1` fetches one extra random block after a real external fetch, so
  the provider's view is not a single “interesting” block. At most two decoy
  downloads run at once and the background prefetch never spawns decoys. Set
  `decoys = 0` to reduce bandwidth at the cost of privacy.
- The validated block cache reduces repeated external fetches. It defaults to
  256 MiB. Set `cache_mib = 0` to disable it, or point `cache_dir` at tmpfs
  (`/dev/shm/e-lite-rawtx`) to keep blocks off persistent disk; repair still
  works from P2P/Esplora when a source is available.

## P2P details

- Minimal Bitcoin P2P client: `version`/`verack`, `ping`/`pong`, `getdata` for
  `MSG_WITNESS_BLOCK` only (BIP144). Peers must advertise `NODE_WITNESS`;
  stripped legacy blocks are rejected and the peer is cooled down.
- BIP324 v2 is attempted first; v1 is the fallback within the same per-peer
  timeout budget.
- Peers are discovered from Core's current connections first, then from
  addrman, and the list is refreshed every 10 minutes (every 60 s while it is
  empty, for example right after Core starts). Pinned `p2p_peers` are never
  replaced. Up to three peers are tried per fetch, with a total 2× timeout cap.
- The last peer that answered is tried first. A failed peer is skipped for
  `provider_cooldown_seconds` (300 s by default), so every block does not pay
  the same dead peer's timeout.
- If a Tor SOCKS connect fails, P2P uses clearnet for non-onion peers for the
  cooldown window (when `tor_fallback_clearnet = true`); `.onion` peers still
  require Tor.
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
- Electrum serves TLS on a separate listener by default (`[server] tls_listen`)
  for wallets that use SSL. TLS protects the transport but does not authenticate
  the wallet; use
  loopback, SSH/VPN, a firewall or a TLS reverse proxy. Core RPC is HTTP only:
  e-lite warns when `rpc_url` is non-loopback and `[core]
  require_loopback_rpc = true` makes it fatal; use an SSH/VPN tunnel.
