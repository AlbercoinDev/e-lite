# Getting started and acceptance

This guide is for **testing from zero**. Installation variants are in
[`INSTALL.md`](INSTALL.md); operation details are in
[`OPERATIONS.md`](OPERATIONS.md).

## 0. Install

Install e-lite with Core using the one-line command in
[`INSTALL.md`](INSTALL.md#one-line-install-or-update), then check it:

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
# Defaults: index/TLS/Tor/logs in ~/e-lite/signet, Core in
# ~/e-lite/signet/bitcoin. No paths are needed.
e-lite doctor --network signet
e-lite run --network signet --prune-mib 1
```

For a custom layout (separate index and Core disks, or an external Core) pass
`--data-dir` and `--core-datadir`; e-lite never writes to a Core datadir it
does not own.

Expected:

- Core starts or is adopted; state `WAITING_FOR_CORE` → `BUILDING_INDEX` → `READY`.
- `prune_mib=1`: Core does not prune by itself; e-lite’s prune gate does.
- `/status` shows `indexed_height`, `core_height`, `pruneheight`, `tuning`.
- Electrum at `0.0.0.0:55022` (TLS) and loopback-only `127.0.0.1:55011`
  (plain) for mainnet;
  signet uses `55211`/`55222`; metrics at `127.0.0.1:55084` (`55284` signet).
- TLS is enabled by default on the network TLS port with a generated
  certificate signed by a local CA (`tls/ca.crt`); `--no-tls` disables it. Operator PEMs:
  `--tls-listen`, `[server] tls_cert`/`tls_key` (see
  [`OPERATIONS.md`](OPERATIONS.md#electrum-tls)).

## 4. Automated suite

```bash
tests/e2e/e2e_launch_core.sh
tests/e2e/e2e_wait_core.sh
tests/e2e/e2e_pruned_ibd.sh
tests/e2e/e2e_regtest.sh
tests/e2e/e2e_resilience.sh
tests/e2e/e2e_external_pruned.sh
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
pkill -USR1 -o -f 'e-lite run'   # -o: the watching parent, which passes it on
curl -s http://127.0.0.1:55084/status | python3 -m json.tool | grep -A20 '"tuning"'
```

Change `readers`, `cache_mib` (maximum), `commit_interval_ms`, `batch_blocks`,
`profile` or `adaptive` in the TOML, send `SIGUSR1` and verify the values in
`/status`. `prune_mib`, `block_source`, `reorg_window`, memory budget fields,
data dir/network require restart.

## 7. Acceptance checklist

- [ ] Installer works on a clean Debian/LXC and updates without touching config.
- [ ] `--with-core` installs/updates the latest official Core.
- [ ] Installer downloads/verifies the Tor bundle; `--without-tor` skips it.
- [ ] `e-lite --version` matches the release.
- [ ] Managed Tor publishes the network plain/TLS ports (55011/55022 on mainnet); the TLS certificate SAN includes the onion.
- [ ] `e-lite onion show` prints the address; `onion reset` (stopped) changes it on the next start.
- [ ] The memory budget line appears, the cache grows in steps, and page cache stays above the reserve.
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
- [ ] `READY`/`FULLY SYNCED` appears in the log/status and `server.banner` shows INDEXING then READY.
- [ ] Custom `banner_file` is created if missing and its placeholders are substituted.
- [ ] Prune gate advances in 1k-safe-block steps; free-space deferral is off by default.
- [ ] `--set index.profile=hdd|ssd|nvme|manual` and other `--set` keys work without a config file.
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
