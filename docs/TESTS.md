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
cargo test --workspace          # unit, resolver, txmode, protocol
scripts/check-docs.sh           # links, anchors, version, test paths, source references
shellcheck install.sh scripts/*.sh tests/*.sh tests/lib/*.sh tests/e2e/*.sh tests/bench/*.sh tests/manual/*.sh
```

`scripts/check-docs.sh` needs `python3`. `shellcheck` is in Debian's repos.
CI runs the gates on every push and pull request and the release workflow
smoke-tests the published artifacts.

## End-to-end (`tests/e2e/`)

Run them all, with one build, a summary and a check that no process is left
behind:

```bash
tests/run-e2e.sh                  # every regtest suite (not e2e_networks)
tests/run-e2e.sh regtest snapshot # only these
tests/run-e2e.sh --release        # against the release binary
```

Logs stay under `target/e2e-logs/<time>/`; `E2E_TIMEOUT` bounds each suite
(1200 s). Every suite can still be run alone (`tests/e2e/e2e_regtest.sh`).

The suites share `tests/lib/e2e.sh` (run directory and cleanup that waits for
every process to end, `core_start`/`core_mine_new`/`scripthash_of`, the
`e2e_conf` config generator, `elite_start`/`elite_stop`, `status`,
`wait_ready`, `fail`) and `tests/lib/e2e.py` (Electrum client and
bitcoin-cli, also as `e2e.py history|height|call` for the shell suites). A new
suite starts with:

```bash
. "$(dirname "$0")/../lib/e2e.sh"
e2e_init <name>
```

`e2e_networks.sh` keeps its own setup: it syncs a real network and cannot run
on a regtest-only machine.

| Script | Covers |
|---|---|
| `e2e_defaults.sh` | Per-network ports, generated CA/leaf, wallet-style TLS trust and hostname/IP checks, plain listener loopback-only, `/status.rawtx.path`. |
| `e2e_launch_core.sh` | A Core owned by e-lite and e-lite restarting itself: starts Core from an empty datadir with `-prune=1` and indexes; the node is killed with SIGABRT and SIGKILL and comes back by itself (same Core pid, index keeps following, one Tor); a clean stop stops Core and is final; an error exit and `--no-supervise` are not restarted. |
| `e2e_wait_core.sh` | Starts before Core, waits in `WAITING_FOR_CORE`, then initializes and indexes without a restart. |
| `e2e_external_pruned.sh` | A pruned Core e-lite did not start is refused for a from-scratch IBD with instructions. |
| `e2e_pruned_ibd.sh` | Manual pruning, prune gate, SIGKILL + restart, pruning keeps serving, discarded work bound. |
| `e2e_recovery.sh` | Repeated SIGKILL while indexing, a reorg at the tip (undo), write failure, `NEEDS_RECOVERY` rules. |
| `e2e_resilience.sh` | Core outage, cookie rotation, cached serving, clean shutdown. |
| `e2e_regtest.sh` | Full wallet path: plain + TLS, 1.6/1.7 negotiation, Sparrow sequence, subscriptions, broadcast, rate limit, log file. |
| `e2e_multi_instance.sh` | Two instances at once: distinct ports, certificates, heights, independent shutdown. |
| `e2e_snapshot.sh` | Export/import per network, index + optional Core section, digest, second import rejected, `--verify=full`. |
| `e2e_privacy.sh` | `privacy check` as text and JSON, a `--set privacy.*` override, an out-of-range value refused; `esplora = []` plus an Electrum list reported as the per-txid mode; `server.features.genesis_hash` equals Core's genesis. |
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

`tests/lib/e2e.py history <port> <scripthash>` is also what the all-in-one
harness (`scripts/e-lite-all-in-one.sh`) uses to count history entries.

## CI vs local

- CI: `fmt`, `clippy`, `cargo test --workspace`, `cargo check --release`,
  `check-docs.sh` and `shellcheck`. No `bitcoind`, so e2e/bench stay local or
  operator-run.
- Before a release: the gates above plus `tests/run-e2e.sh` (all regtest
  suites, about 10 minutes) and `tests/run-e2e.sh --release launch_core
  regtest` against the binary that will be published.
- Real network acceptance (signet/testnet4/mainnet) is operator-run; record
  results in the release notes.
