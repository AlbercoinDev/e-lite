# ADR-014 — Two UTXO indexes, no `balances` table

**Date:** 2026-11-01 · **Status:** accepted

## Context
The original schema keyed `utxo` by `(txid, vout)` and kept a materialized
`balances` table. Spending needs the outpoint lookup, but `listunspent` and
`get_balance` need enumeration **by script**, which the outpoint index cannot do.
The balance table also duplicated writes and could diverge from the UTXO set.

## Decision
- `utxo (txid, vout) -> value, script_hash, creation_height, coinbase`
- `utxo_by_script (script_hash, txid, vout) -> value, creation_height, coinbase`
- `get_balance` sums the script range; `listunspent` iterates it.
- No `balances` table. Balance is always derived from spendable coins.

## Consequences
- Two writes and two deletes per output (the second value is 13 bytes).
- No divergence invariant to maintain.
- Balance for scripts with very many UTXOs is O(n); a cache can be added later
  if measurements demand it.

## Evidence
Oracle parity over 100k occurrences (balances and UTXO sets compared per
script); `blockchain.scripthash.get_balance` fixture.
