# Privacy and external sources

e-lite is local-first. The index answers headers, history, balances, UTXOs and
proofs without contacting third parties. External networks are used only when
the local Core cannot supply a raw transaction (pruned old blocks) or when
repairing a prune gap.

How pruned transactions are fetched, what a server can learn and every
setting of `privacy.toml` are explained in
[`PRIVACY-TUNING.md`](PRIVACY-TUNING.md).

## What stays local

- Wallet scripts, scripthashes, addresses, balances, histories, UTXOs.
- Electrum queries and subscriptions.
- Merkle/checkpoint proofs (computed from the local index and header cache).
- Mempool view, fee estimates and broadcasts come from the local/remote **Core**
  you configured, not from a public Electrum server. How Core itself reaches
  the Bitcoin network is described in "Bitcoin Core's own network" below.
- Logs and metrics do not print scripts, addresses, txids, wallet labels or
  the block hash of a wallet request: failures are logged by height and error
  class, and any 64-hex value in an error is replaced by `<hash>`.
- External requests do not name e-lite: HTTP requests use a generic
  `User-Agent` (`Mozilla/5.0`) and the P2P `version` message a common Core
  subversion. Behaviour (one `getdata`, then disconnect) can still be
  fingerprinted by a determined peer.

## What can leave the machine

Only raw transaction resolution and prune-gap repair can use external sources,
always through Tor unless clearnet is explicitly allowed. The default
(`privacy.toml`, preset `balanced`, AUD-163) fetches **loose transactions by
txid** from public Electrum servers and Esplora, mixed with **decoy wallets**
built from the local index and spread over several servers:

```text
tx cache -> Core local block -> per-txid (Electrum batch / Esplora /tx, with
decoy wallets, verified against the index) -> whole block by hash (fallback)
```

A server learns the txids it is asked for, never the wallet's scripts,
addresses, xpub or IP; it sees `k + 1` plausible wallets loading at once
(`decoys.wallets`, default 3). Every loose transaction is checked before use:
its txid, its position in the indexed block and its merkle branch up to the
indexed header. `preset = "max"` restores **whole blocks by block hash
only**: no txid ever leaves the node, and a provider learns only block
hashes. Every fetched block is verified before use:

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

Edit that file to add, remove or reorder providers. What a provider sees depends
on the mode: the default `balanced` preset asks for loose transactions by txid
(`GET {base}/tx/{txid}/raw` and the Electrum list), always with decoy wallets;
`preset = "max"` requests whole blocks by hash only. Scripts, addresses, xpubs
and `spk_hint` never leave the node. Clearnet entries must be `https://`; plain
`http://` is accepted only for `.onion` hosts and still requires Tor.

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
tor_fallback_clearnet = false
prefetch = true
request_timeout_seconds = 8
tor_connect_timeout_seconds = 10
tor_stall_timeout_seconds = 12
tor_request_timeout_seconds = 60
provider_cooldown_seconds = 300
max_parallel_fetches = 16
p2p_timeout_seconds = 8
p2p_tor_timeout_seconds = 30
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

- Default is **Tor only, fail-closed**: external blocks are requested through
  Tor, and if Tor is unavailable the request fails (history, balances and
  UTXOs keep working from the local index). `tor_fallback_clearnet = true`
  retries on clearnet when the Tor proxy is unavailable or a Tor request
  fails; that ties your IP to the block hashes of your wallet's transactions,
  which is why it is opt-in. `allow_clearnet = true` makes clearnet a normal
  source instead of a fallback.
- Every Esplora request over Tor uses a fresh random SOCKS username. Tor
  isolates streams by SOCKS credentials (`IsolateSOCKSAuth`, on by default),
  so each block is fetched over its own circuit and a provider cannot group
  the blocks of one wallet by exit IP. Providers and P2P peers are tried in
  random order among the healthy ones, so no single provider is preferred and
  the blocks of one wallet are spread over all of them. Timing can still
  correlate requests.
- A block request through Tor builds a new circuit and downloads a whole
  block. It gets `tor_connect_timeout_seconds` (10 s) for the circuit and the
  connection, then fails only when nothing arrives for
  `tor_stall_timeout_seconds` (12 s); both are doubled for `.onion`
  providers. A slow transfer that keeps receiving bytes continues up to
  `tor_request_timeout_seconds` (60 s).
- **Every block download is a race (AUD-160).** Endpoints (each provider over
  Tor, each P2P peer) are shuffled and tried with up to 3 attempts at once: a
  new attempt starts when one fails, when the newest one has sent nothing for
  6 s, crawls below 96 KB/s after 5 s, or is still running at 15 s (direct:
  2 s, 256 KB/s, 6 s). The slow attempt is not cut; the first verified block
  wins and the others are aborted. Over Tor a provider that failed on one
  circuit gets one more try on a new circuit in the same race. **Privacy
  cost:** up to 3 providers or peers may receive the same block hash for one
  download (each on its own circuit, none of them learns more than the block
  hash, and decoys still follow every download). Measured on mainnet over Tor
  with 32 distinct pruned blocks at once: 0.11-0.14 blocks/s and 66-78 %
  failures before, 0.55-0.66 blocks/s and 0-9 % failures after; over
  clearnet 32 blocks took 6 s.
- One block download has 60 s of network time from the moment it gets a
  download slot; waiting for the slot (a burst of distinct blocks) may add
  up to 120 s and does not eat that budget. Requests that share a download
  wait for it. When every endpoint failed or is paused, the race waits up to
  15 s for one to come back before it reports the failure (AUD-160). `/status.rawtx.tx_get_failures` says why a
  `transaction.get` failed (`no_source`, `external_failed`, ...), without any
  hash.
- A block that just failed is answered with that failure for 10 s instead
  of being asked for again: a wallet retrying it does not repeat its block
  hash to every source (AUD-158).
- Wallets reaching e-lite **through its onion service** pay a v3 onion round
  trip (1-7 s) per request. A wallet that sends one request per address scans
  for minutes; with JSON-RPC batches it is a few round trips per page. e-lite
  answers batches; Sparrow sends them only with
  `[server] advertise_electrumx_compat = true` (opt-in, see
  `docs/ELECTRUM_PROTOCOL.md`). Over the LAN, TLS is the fast path.
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
- With a **pruned** Core, `run` waits for at least one provider to serve the
  genesis raw block (requested by its known hash, the same constant request
  for every user) with a matching header hash before opening the Electrum listeners (`require_ready = true` by default).
  This prevents a wallet from receiving a history it cannot fully resolve
  (Sparrow stores a failed `transaction.get` as an unfetchable transaction).
  An unpruned Core serves every raw transaction locally, so the listeners open
  at once. `open_without_provider = true` opens them with local data after
  `ready_timeout_seconds`; e-lite then re-probes every 60 s with the same
  wallet-free requests until a provider answers.
- A provider that fails is skipped for a time that depends on the failure
  (AUD-155): a timeout, a stalled or truncated body or a 5xx is usually the
  circuit, so it backs off 5 s, doubling up to 60 s; a 429/503 honours
  `Retry-After` (15 s doubling, at most 120 s) - over Tor the limit is per
  exit, so one limited circuit only fails that attempt and the provider is
  paused after 3 limited circuits within 60 s (AUD-160); a provider that
  never answers (no connection, no byte) backs off 5 s doubling to 10 min;
  only a wrong answer gets the full `provider_cooldown_seconds` (300 s). A success clears the history. If
  every provider is cooling down, the one whose pause ends first is still
  attempted rather than failing the wallet's history/tx load.
- `allow_clearnet = true` permits direct clearnet access if you explicitly
  accept the privacy trade-off. It is not the default.
- `decoys = 1` owes one random indexed block per real external download
  (wallet or prefetch), so the provider's view is not a set of “interesting”
  blocks. One background thread downloads the owed decoys one at a time,
  spread 0.25-1.5 s apart, outside the wallet's download slots, and waits
  while a provider rate-limits or a wallet download is running (at most 30 s
  per decoy, so it does not take the wallet's Tor bandwidth); a decoy's failure never cools a provider or
  peer that real requests need (AUD-157). At most 32 are owed. Set
  `decoys = 0` to reduce bandwidth at the cost of privacy.
- The validated cache reduces repeated external fetches, and every avoided
  re-download is a block hash a provider does not see again. It defaults to
  256 MiB and stores **transactions** (with their merkle branch, checked
  against the indexed header on every read), not whole blocks: the same
  budget holds tens of thousands of wallet transactions instead of ~170
  blocks. A quarter (at most 64 MiB) keeps whole blocks for prune-gap repair.
  A wallet's requests for several transactions of one block are served from
  the last 8 blocks kept in memory, and the prefetcher caches every history
  transaction of a block from one download. Set `cache_mib = 0` to disable the
  disk cache, or point `cache_dir` at tmpfs (`/dev/shm/e-lite-rawtx`) to keep
  it off persistent disk; repair still works from P2P/Esplora when a source
  is available. The cache directory names the transactions you fetched: it
  is as sensitive as the wallet.

## History prefetch (on by default)

`[rawtx] prefetch = true` (the default since alpha.29.13.0) downloads the
blocks of a wallet's confirmed history in the background as soon as the
wallet asks for the history, so the transactions are already cached when it
asks for them. Without it, a pruned node makes every `transaction.get` wait
for a whole block over Tor, which is what made wallet loads slow after
alpha.29.12.0 (AUD-159).

What a provider can observe is the same as for the wallet's own requests:
each block goes to a random provider on its own circuit, and each external
download owes decoys exactly like a wallet request (AUD-134 is superseded).
It queues up to 64 blocks per history response and downloads them with
every download slot but one (it never takes the last free slot), so a wallet request is never queued
behind it. Blocks whose transactions are already in the transaction cache are
not downloaded, so a wallet that loaded once costs nothing on reconnect. Set
`prefetch = false` if you prefer the wallet to fetch only what it asks for.

## Bitcoin Core's own network

A wallet transaction you broadcast is handed to Core, and Core announces it to
its peers. On clearnet, those peers see your IP as the first announcer. For a
Core started by e-lite, `[core] network` decides which networks Core uses:

| `network` | Core flags | Who sees your IP | Cost |
|---|---|---|---|
| `auto` (default) | `-onion=<Tor SOCKS>`; plus `-i2psam` if an I2P SAM router answers and `-cjdnsreachable` if `cjdns = true` | clearnet peers, as any Bitcoin node | none: clearnet keeps the IBD fast, and Core also reaches onion/I2P/CJDNS peers |
| `private` | `-proxy=<Tor SOCKS> -onlynet=onion` (and `-onlynet=i2p`/`cjdns` when available), `-listen=0 -dnsseed=0 -dns=0 -discover=0 -natpmp=0` | **no Bitcoin peer** | much slower IBD; Core does not start if no private network is available (it never falls back to clearnet) |
| `clearnet` | none | clearnet peers | none |

`[core] private_broadcast = true` adds `-privatebroadcast=1` (experimental in
Bitcoin Core; e-lite checks that the installed `bitcoind` has it): transactions
submitted by wallets leave through short-lived Tor/I2P connections, so with
`auto` you keep fast clearnet blocks **and** hide the origin of your
transactions. Consequences to know: Core does not put the transaction in its
mempool until the network relays it back, so the wallet sees it a few seconds
later; and if Tor is down, Core keeps the transaction pending instead of
failing. Package broadcasts (`broadcast_package`) are not covered by Core's
option.

Tor is the one e-lite already runs (`[tor] mode = "managed"`) or the proxy in
`[rawtx] tor_socks` when it answers. I2P needs a router with SAM enabled that
you run yourself (i2pd or Java I2P); e-lite does not ship one. Core uses a
random SOCKS credential per connection (`-proxyrandomize`), so its Tor
connections are on separate circuits. Core's own onion **listener** stays off
(`-listenonion=0`): the node does not accept inbound connections over Tor.
Operator flags win: if `[core] extra_args` sets `-proxy`, `-onion`, `-onlynet`,
`-i2psam`, `-connect` or `-privatebroadcast`, e-lite adds none of the above.
An external Core is never changed: set these flags in its `bitcoin.conf`.

## P2P details

- Minimal Bitcoin P2P client: `version`/`verack`, `ping`/`pong`, `getdata` for
  `MSG_WITNESS_BLOCK` only (BIP144). Peers must advertise `NODE_WITNESS`;
  stripped legacy blocks are rejected and the peer is cooled down.
- BIP324 v2 is attempted first, unless Core reports that it talks v1 to that
  peer. v1 is tried only when the v2 connection was established and the v2
  handshake failed; a connection that could not be made is not paid for twice.
  Each protocol attempt has its own budget: `p2p_tor_timeout_seconds` (30 s)
  through Tor, `p2p_timeout_seconds` (8 s) direct. Inside it the connection
  phase is bounded on its own (10 s through Tor, 20 s to an `.onion` peer,
  4 s direct) and reads stall after 12 s (Tor) or 6 s (direct) without data
  (AUD-160): Tor keeps retrying a dead peer on new circuits for up to its
  `SocksTimeout` (2 min), so waiting longer only holds a slot. A peer that
  never answers or refuses the connection backs off 30 s doubling to 10 min.
- Peers are discovered from Core's **outbound** connections first, then from
  addrman, and the list is refreshed every 10 minutes (every 60 s while it is
  empty, for example right after Core starts). Inbound peers (their address
  is an ephemeral port), I2P/CJDNS peers (unreachable through Tor) and peers
  without `NODE_NETWORK` + `NODE_WITNESS` are skipped, and onion peers go
  first: they need no exit node, whose policy often rejects port 8333
  (AUD-154). Pinned `p2p_peers` are never replaced. Up to three peers are
  tried per fetch, within two attempt budgets.
- Peers are tried in random order. A peer that times out or refuses backs off
  30 s, doubling up to `provider_cooldown_seconds` (300 s); a peer that
  answers wrongly (no full blocks, `notfound`, invalid block) gets the full
  cooldown. A success clears it. When P2P is the only external source and
  every peer is cooling down, the one whose pause ends first is tried.
- Only a failure of the local Tor proxy itself (no connection, failed
  handshake) pauses P2P over Tor, for 15 s. A SOCKS reply about one peer
  (refused, unreachable, exit policy) affects that peer only (AUD-153). With
  `tor_fallback_clearnet = true`, a Tor failure uses clearnet for that
  non-onion peer; `.onion` peers still require Tor.
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
