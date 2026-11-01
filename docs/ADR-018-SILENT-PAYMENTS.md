# ADR-018 — Silent payments: opt-in gate now, BIP352 indexer later

**Date:** 2026-11-02 · **Status:** accepted (gate), indexer deferred

## Context
Sparrow's silent payments wallet uses a non-standard Electrum extension:

```
blockchain.silentpayments.subscribe
  params: [scanPrivKeyHex, spendPubKeyHex, start, labels]
  result: { address, labels, start_height }

notification: blockchain.silentpayments.subscribe
  params: { subscription, progress, history: [{ height, tx_hash, tweak_key }] }
```

Implementing it requires BIP352 scanning: for each eligible input pubkey, compute
the shared secret with the wallet's **scan private key**, sum them, derive the
tweak, and compare the tweaked spend key against output keys. The server must
also handle labels and report `tweak_key` per matching transaction.

Two constraints make this unsafe to ship half-done:

1. The scan private key is sent to the server. On a self-hosted e-lite this is
   the user's own machine, but it is still a private key and must never be
   logged or persisted.
2. BIP352 needs the **prevout scripts/pubkeys** of each input. The current index
   stores only `script_hash` and deletes spent UTXOs, so a correct historical
   scan needs either a schema v2 retaining eligible prevout pubkeys or on-demand
   block re-reading from Core.

## Decision
- Ship an **opt-in gate now**, disabled by default:
  ```toml
  [silent_payments]
  enabled = false
  ```
- When disabled, `blockchain.silentpayments.subscribe` and
  `.unsubscribe` return an explicit server error (-32000) instead of
  `unknown method`. Normal wallets are unaffected.
- When enabled, the same methods return a clear "no BIP352 indexer in this
  build" error until the indexer lands. The protocol surface (method names,
  params, notification shape) is reserved.
- Do **not** advertise silent payments in `server.features`.

## Future implementation plan
1. **Schema v2**: retain, for each eligible output, the pubkey/script needed for
   BIP352 (P2PKH/P2WPKH/P2TR); migration or controlled reindex.
2. **`el-silentpayments` module**: tagged hashes, shared-secret sum, tweak
   derivation, labels, output matching; property tests against BIP352 vectors.
3. **Subscriptions**: per-client scan keys in memory only; no logs, no disk;
   scan from the requested start height with progress notifications, then follow
   new blocks.
4. **Notifications**: `{subscription, progress, history:[{height, tx_hash,
   tweak_key}]}`; status updates on reorgs.
5. **Validation**: Sparrow silent payments wallet on Signet with a known birth
   height and a label; verify receive, history, notifications and restart.
6. **Privacy**: document that the server sees the scan key; consider requiring a
   explicit `--i-understand` flag or restricting to loopback.

## Consequences
- Today: silent payments wallets get a clear error; all normal wallets work.
- Later: enabling the feature is a configuration change plus the indexer module,
  without changing the rest of the server or the database format unexpectedly
  (schema v2 will be versioned).
