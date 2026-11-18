# e-lite documentation

Rules for agents and maintainers are in [`../AGENTS.md`](../AGENTS.md).

## State and plans

1. [`HANDOFF.md`](HANDOFF.md) — current state, open risks and next steps.
2. [`FEATURES.md`](FEATURES.md) — what exists today.
3. [`ROADMAP.md`](ROADMAP.md) — future features and proposed improvements.
4. [`AUDIT.md`](AUDIT.md) — open findings and residual risks.
   [`audit/FIXED.md`](audit/FIXED.md) — fixed findings (AUD-001..201, except the ones open in AUDIT).
5. [`../CHANGELOG.md`](../CHANGELOG.md) — every release.

## Using e-lite

6. [`INSTALL.md`](INSTALL.md) — one-line install/update, Core, `.deb`, source.
7. [`GETTING-STARTED.md`](GETTING-STARTED.md) — acceptance tests from scratch.
8. [`OPERATIONS.md`](OPERATIONS.md) — first run, pruning, disks, backups, monitoring.
9. [`CLI.md`](CLI.md) — every command-line flag, `--set` and tuning examples.
10. [`TUNING.md`](TUNING.md) — machine plan, memory budget, hot reload, IBD shape.
11. [`PRIVACY.md`](PRIVACY.md) — local vs external data, Tor/clearnet, providers.
    [`PRIVACY-TUNING.md`](PRIVACY-TUNING.md) — `privacy.toml`: per-txid mode,
    decoy wallets, presets, every parameter with its range.
12. [`TOR.md`](TOR.md) — managed Tor, onion access, the TLS identity and reset.

## How it works and how it is tested

13. [`ARCHITECTURE.md`](ARCHITECTURE.md) — components and execution modes.
14. [`ELECTRUM_PROTOCOL.md`](ELECTRUM_PROTOCOL.md) — method/version matrix,
    notifications and limits.
15. [`TESTS.md`](TESTS.md) — test suites, prerequisites and what runs in CI;
    [`../tests/README.md`](../tests/README.md) for the layout and
    [`../tests/manual/README.md`](../tests/manual/README.md) for A/B runs on a
    real node.
16. [`RELEASING.md`](RELEASING.md) — version bump, tag, workflows and artifacts.

## Reference material

- [`adr/`](adr/README.md) — architecture decision records. Historical context;
  values may have been superseded (for example prune defaults). The current
  operational truth is the code plus [`FEATURES.md`](FEATURES.md).
- [`benchmarks/`](benchmarks/README.md) — dated measurements (F0/F1/F2), tied to
  the tree at the time.
- [`reviews/`](reviews/2026-11-analisis.md) — closed reviews (Spanish), tied to
  the tree of their date.
- [`../config.example.toml`](../config.example.toml) — every setting with its
  default.
