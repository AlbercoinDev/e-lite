# ADR-020 — Core lifecycle, local pruned IBD and lag guard

**Date:** 2026-11-02 · **Status:** accepted

## Context

e-lite must start without Core, wait for it, or launch it; and it must index from
genesis while Core runs with maximum pruning without ever losing a block to the
prune window. A remote Core over RPC cannot provide the block files and cannot
be paused safely.

## Decision

1. **`[core] manage = external | auto | managed`.**
   - `external`: connect only; never start, stop, pause or prune Core.
   - `auto`: use a running Core read-only; start it if absent.
   - `managed`: always start Core; fail if another instance is running.
2. **Only a Core started by e-lite can be controlled.** The process is tracked
   in `<data-dir>/core-child.pid`; a Core that was already running is treated as
   external for control purposes.
3. **Pruned IBD from zero is local-only and manual.** It requires `block_source`
   access to Core's `blocks/` directory and a Core owned by e-lite started with
   `prune_mib = 1`. e-lite is then the only pruning authority: the prune gate
   never requests a cut above `committed - 288`, so Core cannot delete a block
   the index still needs. An external or remote pruned Core with an empty index
   is rejected with instructions.
4. **Lag guard is only for automatic pruning.** With `prune_mib >= 550` and
   `allow_auto_prune = true`, `lag_limit = min(configured,
   (blocks - pruneheight) - 32, 224)` and resumes at 50%. The hard 224 cap
   leaves margin below Core's `MIN_BLOCKS_TO_KEEP` (288). Manual pruning does not
   use the guard. Active rawtx readers below `pruneheight + 32` also pause Core.
5. **Disk guard watches both filesystems.** Free space is the minimum of the
   e-lite data dir and the Core datadir. Below `min_free_gib` Core downloads are
   paused but indexing continues, so the prune gate can free space. Below
   `critical_free_gib` indexing also stops. Both resume with hysteresis.
6. **Pause marker.** A pause writes `<data-dir>/core-paused-by-e-lite`; after a
   crash, the next start resumes `setnetworkactive(true)` and removes it.
7. **Manual prune uses the height Core actually reports**, not the requested
   height, and is only issued for a Core owned by e-lite. Retries are throttled
   by the *requested* height with `min_gain` blocks of progress, because Core
   prunes at block-file granularity and can report the same height for a long
   time.
8. **Remote Core is maintenance-only.** HTTP over SSH/VPN; `rpc_user`/
   `rpc_password` or a copied cookie. No REST/P2P ingest in this iteration.

## Consequences

- A user who already runs Core must either stop it and let e-lite start it
  (`managed`) or keep `external` and use a full node for the first index. This
  is deliberate: e-lite cannot guarantee the prune race on a node it does not
  own.
- `stop_on_exit = true` prevents Core from pruning while e-lite is down, but
  only for a Core e-lite started. An external Core can still advance; the
  288-block window is documented.
- The lag guard no longer pauses at 32 blocks, so Core can download ahead and
  the index is not artificially throttled.
- The disk guard can pause indexing on a full disk; `/status` exposes
  `disk_low`, `retention_blocks`, `pruneheight` and `network_active`.

## Evidence

- `tests/e2e_launch_core.sh`: from an empty datadir, e-lite starts Core, indexes
  151 blocks, serves queries and stops Core.
- `tests/e2e_wait_core.sh`: `WAITING_FOR_CORE` → Core appears → `READY`.
- `tests/e2e_external_pruned.sh`: external pruned Core + empty index fails
  without touching `networkactive`.
- `tests/e2e_pruned_ibd.sh`: e-lite starts Core with `-prune=1 -fastprune`,
  indexes 2101 blocks, prunes to 1785 and keeps serving.
- `tests/e2e_resilience.sh`: Core down → cached serve → Core returns → `READY`.
- `docs/benchmarks/F2.md`.
