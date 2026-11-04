# Getting started and acceptance

This guide is for **testing from zero**. Installation variants are in
[`INSTALL.md`](INSTALL.md); operation details are in
[`OPERATIONS.md`](OPERATIONS.md).

## 0. One-line setup

```bash
# e-lite only
curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh | sh

# e-lite + latest official Bitcoin Core (recommended)
curl -fsSL https://github.com/AlbercoinDev/e-lite/releases/latest/download/install.sh | sh -s -- --with-core --yes
```

```bash
export PATH="$HOME/.local/bin:$PATH"
e-lite --version
```

## 1. Requirements

- Debian 12+/LMDE 6+, x86_64, Core 28+.
- `python3` for query/report scripts.
- Free disk: regtest <1 GiB; signet ≥15 GiB; testnet4 ≥20 GiB for a full
  from-zero test.
- For a pruned from-zero IBD: local Core owned by e-lite and read access to its
  `blocks/` directory.

## 2. Quick regtest from zero

```bash
bash scripts/e-lite-all-in-one.sh --network regtest --quick --with-core --yes
```

The harness installs/updates e-lite, starts Core, indexes, queries Electrum and
writes `~/e-lite-test/last-report.txt`.

## 3. Manual regtest / signet start

```bash
CORE="$HOME/.bitcoin-signet"
DATA="$HOME/.local/share/e-lite/signet"
mkdir -p "$CORE" "$DATA"

e-lite doctor --network signet --core-datadir "$CORE" --data-dir "$DATA"

e-lite run --network signet --manage managed \
  --core-datadir "$CORE" --data-dir "$DATA" --prune-mib 1
```

Expected:

- Core starts or is adopted; state `WAITING_FOR_CORE` → `BUILDING_INDEX` → `READY`.
- `prune_mib=1`: Core does not prune by itself; e-lite’s prune gate does.
- `/status` shows `indexed_height`, `core_height`, `pruneheight`, `tuning`.
- Electrum at `127.0.0.1:50001` (plain); metrics at `127.0.0.1:9184`.
- Optional TLS listener: `--tls-listen`/`--tls-cert`/`--tls-key` (see
  [`OPERATIONS.md`](OPERATIONS.md#electrum-tls-optional)).

## 4. Automated suite

```bash
tests/e2e_launch_core.sh
tests/e2e_wait_core.sh
tests/e2e_pruned_ibd.sh
tests/e2e_regtest.sh
tests/e2e_resilience.sh
tests/e2e_external_pruned.sh
```

Or:

```bash
bash scripts/e-lite-all-in-one.sh --network regtest --suite --with-core --yes
```

## 5. Signet / testnet4 acceptance

```bash
bash scripts/e-lite-all-in-one.sh --network signet --acceptance --with-core --yes
bash scripts/e-lite-all-in-one.sh --network testnet4 --acceptance --with-core --yes
```

The script refuses to start below 15/20 GiB free unless
`E_LITE_E2E_REQUIRED_FREE_GIB` is overridden.

## 6. Hot reload

```bash
kill -USR1 "$(pgrep -f 'e-lite run')"
curl -s http://127.0.0.1:9184/status | python3 -m json.tool | grep -A20 '"tuning"'
```

Change `readers`, `cache_mib`, `commit_interval_ms`, `batch_blocks`, `profile`
or `adaptive` in the TOML, send `SIGUSR1` and verify the values in `/status`.
`prune_mib`, `block_source`, `reorg_window`, data dir/network require restart.

## 7. Acceptance checklist

- [ ] Installer works on a clean Debian/LXC and updates without touching config.
- [ ] `--with-core` installs/updates the latest official Core.
- [ ] `e-lite --version` matches the release.
- [ ] From-zero pruned sync reaches `READY` with `prune_mib=1`.
- [ ] `pruneheight` stays at or below `indexed_height - 288`.
- [ ] No `NEEDS_RECOVERY` / prune gap.
- [ ] Electrum history/balance/UTXO/mempool/fees/broadcast work.
- [ ] Optional Electrum TLS works and a renewed PEM is picked up without restart.
- [ ] Startup log has no pre-Core `Connection refused`, `p2p discovery failed` or `memory pressure` lines.
- [ ] Ctrl+C logs `stopped at …` with no post-shutdown Core/mempool errors.
- [ ] `/status` shows real RAM/cgroup figures and the configured cache, not a false 0 MiB.
- [ ] Progress shows height/Core percentages and a milestone every 10k blocks or 6 h.
- [ ] `<data-dir>/e-lite.log` contains the same lines as the console (start/stop, progress, milestones).
- [ ] Prune gate runs at most every 10k safe blocks by default (or immediately on low disk).
- [ ] `--profile hdd|ssd|nvme|manual` and `--set` work without a config file.
- [ ] Raw tx works from Core/cache; pruned fallback is explicit if unavailable.
- [ ] Disk-low pauses downloads; indexing continues until the critical floor.
- [ ] `SIGUSR1` applies hot fields; cold fields are logged.
- [ ] No OOM in LXC with explicit readers/cache; cgroup memory is respected.
- [ ] Restart resumes from the committed tip.

## 8. Getting help

Include:

```bash
e-lite --version
e-lite status --json
tail -100 <data-dir>/bitcoind.log
tail -100 <data-dir>/LOG
dmesg -T | grep -i -E 'oom|killed process'
```

Do not paste wallet addresses, private keys or seed phrases.
