# Test scripts

Organized by purpose. See [`docs/TESTS.md`](../docs/TESTS.md) for what each
suite covers, the quality gates and CI vs local runs.

```text
tests/e2e/      end-to-end regtest/signet suites (bitcoind required)
tests/bench/    synthetic benchmarks (bitcoind required)
tests/manual/   operator-driven sampling (no bitcoind required)
tests/lib/      shared helpers: e2e.sh (shell suites), e2e.py (Electrum client)
tests/run-e2e.sh  runs every regtest suite with one build and a summary
```

Rules:

- Build a binary first: `cargo build -p e-lite` (the scripts use
  `target/debug/e-lite` unless `E_LITE_BIN` points elsewhere; bench scripts use
  `target/release/e-lite`).
- `bitcoind` must be in `PATH` for `e2e/` and `bench/`.
- Every script creates its own temporary datadir, ports and files under
  `${TMPDIR:-/tmp}` and cleans them up; never point one at an existing index or
  Core datadir.
- Scripts resolve the repository root and helpers from their own location, so
  they can be run from any working directory:

```bash
tests/run-e2e.sh                    # all regtest suites (or: tests/run-e2e.sh regtest)
tests/e2e/e2e_regtest.sh            # one suite on its own
BLOCKS=5000 VARIANTS="default::|readers-4:4:" tests/bench/bench_index.sh
tests/manual/ab_umbrel.sh baseline 1800
```

CI runs the Rust gates, `scripts/check-docs.sh` and `shellcheck`, not the
e2e/bench suites (they need `bitcoind`).
