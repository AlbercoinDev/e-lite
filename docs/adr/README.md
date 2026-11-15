# Architecture decision records

Historical decisions, newest last. They describe the context at the time and
may contain values or paths that were later superseded (prune defaults, module
layout, `docs/` paths). The current behaviour is the code plus
[`../FEATURES.md`](../FEATURES.md) and `config.example.toml`.

| ADR | Date | Decision |
|---|---|---|
| [ADR-012](ADR-012-STORAGE.md) | 2026-11-01 | RocksDB as the storage engine (atomic `WriteBatch`, iterators, replaceable behind the store API). |
| [ADR-013](ADR-013-TXREF.md) | 2026-11-01 | Derived `tx_ref` and a flat `txids.dat` in chain order; never index by txid alone (BIP30). |
| [ADR-014](ADR-014-UTXO.md) | 2026-11-01 | Two UTXO indexes (`utxo` by outpoint, `utxo_by_script`), no materialized balances table. |
| [ADR-015](ADR-015-INGEST.md) | 2026-11-01 | Ingest pipeline: raw blocks, prevout prefetch, reader/apply split and group commit. |
| [ADR-016](ADR-016-REORG.md) | 2026-11-01 | Bounded undo window instead of an unbounded journal; a deeper rollback is `NEEDS_RECOVERY`. |
| [ADR-017](ADR-017-RAWTX.md) | 2026-11-01 | Verified external raw transactions for a pruned Core; only validated data is served or cached. |
| [ADR-018](ADR-018-SILENT-PAYMENTS.md) | 2026-11-02 | Silent payments: opt-in gate now, BIP352 indexer deferred. |
| [ADR-019](ADR-019-BULK.md) | 2026-11-02 | Bulk ingest from Core block files, including manual pruning and the prune gate. |
| [ADR-020](ADR-020-CORE-LIFECYCLE.md) | 2026-11-02 | Core lifecycle, local pruned IBD, lag guard and the pause/resume policy. |

New decisions that change architecture should be added here with the next
number and linked from `../HANDOFF.md` or `../FEATURES.md` when they change
observable behaviour.
