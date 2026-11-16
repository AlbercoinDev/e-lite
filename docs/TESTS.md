# Tests

Everything a change must pass, what each suite covers and what it needs.
`bitcoind` must be in `PATH` for the e2e/bench scripts, and a debug binary must
exist first (`cargo build -p e-lite`); every script creates its own regtest
datadir, ports and temporary directory and cleans up after itself, unless it is
explicitly an operator-driven network test.

## Quality gates (always)

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace          # 372 tests: unit, resolver, txmode, protocol
scripts/check-docs.sh           # links, anchors, version, test paths, source references
shellcheck install.sh scripts/*.sh tests/e2e/*.sh tests/bench/*.sh tests/manual/*.sh
```

`scripts/check-docs.sh` needs `python3`. `shellcheck` is in Debian's repos.
CI runs the gates on every push and pull request and the release workflow
smoke-tests the published artifacts.

## End-to-end (`tests/e2e/`)

| Script | Covers |
|---|---|
| `e2e_defaults.sh` | Per-network ports, generated CA/leaf, wallet-style TLS trust and hostname/IP checks, plain listener loopback-only, `/status.rawtx.path`. |
| `e2e_launch_core.sh` | e-lite starts Core from scratch with `-prune=1`, indexes, `READY`, SIGTERM stops both. |
| `e2e_wait_core.sh` | Starts before Core, waits in `WAITING_FOR_CORE`, then initializes and indexes without a restart. |
| `e2e_external_pruned.sh` | A pruned Core e-lite did not start is refused for a from-scratch IBD with instructions. |
| `e2e_pruned_ibd.sh` | Manual pruning, prune gate, SIGKILL + restart, pruning keeps serving, discarded work bound. |
| `e2e_recovery.sh` | Repeated SIGKILL while indexing, a reorg at the tip (undo), write failure, `NEEDS_RECOVERY` rules. |
| `e2e_resilience.sh` | Core outage, cookie rotation, cached serving, clean shutdown. |
| `e2e_supervisor.sh` | Automatic restart without an init system: the node is killed with SIGABRT and SIGKILL and comes back, same Core pid, index keeps following, one Tor; a clean stop, an error exit and `--no-supervise` are not restarted. |
| `e2e_regtest.sh` | Full wallet path: plain + TLS, 1.6/1.7 negotiation, Sparrow sequence, subscriptions, broadcast, rate limit, log file. |
| `e2e_multi_instance.sh` | Two instances at once: distinct ports, certificates, heights, independent shutdown. |
| `e2e_snapshot.sh` | Export/import per network, index + optional Core section, digest, second import rejected, `--verify=full`. |
| `e2e_networks.sh` | Opt-in real signet/testnet4 sync and acceptance; needs network access and free disk. |

Wallet protocol helpers (`e2e_electrum16.py`, `e2e_electrum17.py`,
`e2e_sparrow.py`) are invoked by `e2e_regtest.sh`.

## Benchmarks (`tests/bench/`)

| Script | Covers |
|---|---|
| `bench_index.sh` | Mines a disposable regtest chain and indexes it with several tuning variants, one CSV row per run. |
| `bench_ibd.sh` | Joint Core IBD + e-lite indexing with a feeder node and sampling (`bench_sample.py`, `bench_run.py`). |

`scripts/benchmark-index.sh` wraps a real node/index for a fixed window; it is
not synthetic and is not run in CI. Dated results are in
[`docs/benchmarks/`](benchmarks/README.md).

## Manual (`tests/manual/`)

| Script | Covers |
|---|---|
| `ab_umbrel.sh` | Samples a running `/status` for a labelled A/B run; see [`docs/AB_TESTS.md`](AB_TESTS.md). It does not change config or restart anything. |

`tests/lib/query_history.py` is the shared Electrum history helper used by the
e2e scripts and the all-in-one harness.

## CI vs local

- CI: `fmt`, `clippy`, `cargo test --workspace`, `cargo check --release`,
  `check-docs.sh` and `shellcheck`. No `bitcoind`, so e2e/bench stay local or
  operator-run.
- Before a release: the gates above plus at least `e2e_defaults`,
  `e2e_regtest`, `e2e_snapshot` and, when commit/undo/rewind changed,
  `e2e_recovery`; when Core lifecycle or pruning changed, `e2e_pruned_ibd` and
  `e2e_multi_instance`.
- Real network acceptance (signet/testnet4/mainnet) is operator-run; record
  results in the release notes.
