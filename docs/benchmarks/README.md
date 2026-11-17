# Benchmarks

Dated measurements, not capacity claims. Each document records its host,
build profile and corpus, so results are comparable only on the same machine.
Use `tests/bench/bench_ibd.sh` and `tests/bench/bench_index.sh` to reproduce
the synthetic runs, and `tests/bench/bench_node.sh` against a real node or
index.

| Document | Date | What it measures |
|---|---|---|
| [F0](F0.md) | 2026-11-01 | Storage, ingest and RPC on an 8 vCPU / 7.7 GiB HDD VM (regtest). |
| [F1](F1.md) | 2026-11-02 | Bulk `blk*.dat` ingest on the F0 host (one coinbase per block). |
| [F2](F2.md) | 2026-11-02 | From-zero startup, Core launch/wait and local pruned IBD (Spanish, alpha.10 base). |

Mainnet numbers live in the release notes and in `docs/TUNING.md` when they are
measured on a real pruned node; they are not extrapolated from these files.
