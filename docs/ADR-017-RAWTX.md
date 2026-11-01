# ADR-017 — Verified external raw transactions for pruned Core

**Date:** 2026-11-01 · **Status:** accepted

## Context
The index answers history/balance/UTXO/Merkle from local data, but raw
transactions need block bytes. A full node keeps them; a pruned node does not.
Fetching a transaction by TXID from a public API would leak the exact wallet
interest. The goal is to keep the server thin while preserving privacy and
integrity.

## Decision
- Resolve raw transactions in this order: **local Core → validated cache →
  external sources**.
- External sources return **whole blocks by hash** (`/block/:hash/raw` or P2P
  `getdata(MSG_BLOCK)`); never TXIDs.
- Verify every fetched block before serving or caching:
  1. block hash and 80-byte header equal the indexed record,
  2. Merkle root recomputed from the transactions,
  3. TXID recomputed and found in the block,
  4. BIP141 witness commitment (coinbase reserved value + wtxid root).
- Cache validated blocks (10 GiB LRU). Core-served blocks are cached too, so a
  wallet restore does not re-fetch the same block.
- Privacy defaults: Tor SOCKS5h with stream isolation, fail closed without Tor
  (`allow_clearnet = false`), 3 decoys per external fetch, no sensitive labels in
  logs or metrics.
- Sources: Esplora-compatible endpoints and a minimal P2P client (version/verack,
  getdata, NODE_NETWORK filtering). P2P requests are distributed across peers and
  Tor circuits.

## Consequences
- The provider(s) learn the **set of blocks** fetched, not the scripts or TXIDs.
  Decoys and random source selection enlarge that set at the cost of bandwidth.
- Availability depends on external sources for pruned blocks; failures produce an
  explicit `rawtx unavailable` error, never invented data.
- A malicious source cannot forge data without breaking the hash/Merkle/witness
  checks; it can only withhold, in which case another source is tried.
- P2P v1 is implemented; BIP324 v2 and a larger peer pool are future work.
- The index still requires a full node (or a snapshot) to build. "Thin" means no
  block store, not no index.

## Evidence
- Unit/mock: witness and Merkle tampering, corrupt/partial blocks, cache eviction,
  source fallback, decoys, Tor fail-closed, prune gap.
- Live: signet tip block fetched from Blockstream, verified, served and cached;
  block fetched from a real Bitcoin Core regtest peer over P2P.
- `transaction.get` served from cache on the second request in the regtest E2E.
