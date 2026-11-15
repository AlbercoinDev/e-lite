# e-lite documentation

Start here:

1. [`HANDOFF.md`](HANDOFF.md) — current state, open risks and next steps.
2. [`INSTALL.md`](INSTALL.md) — one-line install/update, Core, `.deb`, source.
3. [`ARCHITECTURE.md`](ARCHITECTURE.md) — components and execution modes.
4. [`OPERATIONS.md`](OPERATIONS.md) — first run, pruning, disks, monitoring.
5. [`PRIVACY.md`](PRIVACY.md) — local vs external data, Tor/clearnet, providers.
   [`PRIVACY-TUNING.md`](PRIVACY-TUNING.md) — `privacy.toml`: per-txid mode,
   decoy wallets, presets, every parameter with its range.
6. [`FEATURES.md`](FEATURES.md) — what exists today.
7. [`ROADMAP.md`](ROADMAP.md) — future features and proposed improvements.
8. [`CLI.md`](CLI.md) — every command-line flag, `--set` and tuning examples.
9. [`TUNING.md`](TUNING.md) — HDD/SSD/NVMe profiles, hot reload and memory budget.
10. [`ELECTRUM_PROTOCOL.md`](ELECTRUM_PROTOCOL.md) — method/version matrix,
    notifications and limits.
11. [`TOR.md`](TOR.md) — managed Tor, onion access, TLS certificate and reset.
12. [`GETTING-STARTED.md`](GETTING-STARTED.md) — acceptance tests from scratch.
13. [`TESTS.md`](TESTS.md) — test suites, prerequisites and what runs in CI.
14. [`RELEASING.md`](RELEASING.md) — version bump, tag, workflows and artifacts.
15. [`AB_TESTS.md`](AB_TESTS.md) — concrete mainnet/Umbrel A/B protocol.
16. [`AUDIT.md`](AUDIT.md) — open findings and residual risks.
    [`audit/FIXED.md`](audit/FIXED.md) — fixed findings (AUD-001..172).
17. [`PROJECT_MEMORY.md`](PROJECT_MEMORY.md) — project invariants.
18. [`ANALISIS.md`](ANALISIS.md) — análisis del proyecto (en español):
    funcionalidad, sincronización e indexación, servicio a las wallets, copias
    de seguridad, plan de mejoras y propuestas.

Reference material:

- [`adr/`](adr/README.md) — architecture decision records. Historical context;
  values may have been superseded (for example prune defaults). The current
  operational truth is the code plus [`FEATURES.md`](FEATURES.md).
- [`benchmarks/`](benchmarks/README.md) — dated measurements (F0/F1/F2), tied to
  the tree at the time.
- [`../config.example.toml`](../config.example.toml) — every setting with its
  default.
