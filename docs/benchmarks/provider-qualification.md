# Provider qualification A/B — 2026-11-19

This is a controlled loopback test of the two published Linux AMD64 binaries,
not a mainnet, Tor circuit or Sparrow benchmark. It exercises the actual
Electrum listener, resolver, SOCKS5 handshake and stream governor. The block
fallback is disabled here to isolate per-txid admission; its qualification is
covered by the upstream block-route regression test.

## Reproduction

Use Python 3 and Bitcoin Core 32.1. Verify each downloaded tarball against its
published `SHA256SUMS`. Run with a new output directory:

```sh
python3 tests/bench/bench_provider_ab.py /path/to/29.26.1/e-lite \
  /path/to/29.26.2/e-lite --output /tmp/opencode/provider-ab --runs 3
```

Measured on the same Linux 6.12.117 AMD64 VM (8 vCPU, 7.7 GiB RAM, temp
directory on tmpfs). The tarball SHA256 values were
`052d0bf3f0d1bc5a20edea15effaffb04075291399e4a55cddf88151f7144764`
for 29.26.1 and
`05ccc774b4394488db895dc93166ba3cd460ca4c13b66865cac833aa2cfb8beb`
for 29.26.2.

The script creates disposable regtest Core data and one index, then copies the
index to a new directory with an empty `rawtx-cache` for **every** run. The
regtest RPC adapter makes the archive heights appear pruned; no live chain
data is used. The same synthetic script has 239 confirmed transactions; the
fixture has three additional histories for decoys. The client subscribes,
requests history and loads the 239 transaction bodies in 32-item JSON-RPC
batches. Each body must match Core's serialized transaction byte for byte.

Five separate simulated Electrum operators sit behind the local SOCKS5 fixture.
A healthy operator waits 75 ms before returning a batch; in the degraded
scenarios two operators accept a request but never return the batch. The
Tor stream limit is eight, the preset is `balanced`, and every run starts a
new e-lite process. For the settled scenario both versions get a fixed 7 s
lead time before the client starts; the cache is still empty. Variant order
alternates to reduce bias.

The timer starts at wallet protocol negotiation, not at server startup, and
the lead time is excluded. The proxy measures traffic independently of the
server counters. A failed stream is a batch deliberately left unanswered by
the fixture. Failed probe streams include a retry in flight if the proxy
already received it by the end of the observation. Operator share is measured
on real-wallet items **sent**, including retries, not on successful replies.
The script saves per-run stream logs, server logs, raw counters and
`results.json` in the output directory.

## Results

Three runs per version and condition; all **18/18** recovered exactly 239
transactions. The [per-run CSV](provider-qualification.csv) includes the
distribution of real items, including retries. Numbers below are medians;
the total time range is shown in brackets. Failed-stream columns are
wallet traffic and public probes respectively.

| Condition | Version | First batch | Total [min–max] | Wallet streams | Probe streams | Failed wallet / probes |
|---|---|---:|---:|---:|---:|---:|
| All providers healthy | 29.26.1 | 0.751 s | 0.915 s [0.902–0.925] | 21 | 0 | 0 / 0 |
| All providers healthy | 29.26.2 | 0.752 s | 0.961 s [0.883–0.989] | 21 | 5 | 0 / 0 |
| Two timeouts, immediate load | 29.26.1 | 9.809 s | 13.048 s [12.986–13.095] | 29 | 0 | 14 / 0 |
| Two timeouts, immediate load | 29.26.2 | 5.823 s | 6.007 s [5.998–6.112] | 21 | 6 | 0 / 3 |
| Two timeouts, 7 s lead | 29.26.1 | 9.729 s | 12.906 s [0.908–13.049] | 25 | 0 | 7 / 0 |
| Two timeouts, 7 s lead | 29.26.2 | 0.766 s | 0.918 s [0.912–0.968] | 21 | 6 | 0 / 3 |

- With timeouts at startup, the median dropped by 54%. Counting even
  unsuccessful public probes, failed streams fell from **36 to 9** across
  the three immediate runs. In the settled scenario they fell from **14 to
  9**; one old-version run randomly avoided both bad routes entirely.
- Each load was served by three operators: **79/80/80** real txids across
  the three responding operators in both versions (largest share **33.5%**).
  The lower sent-share for some old-version runs includes wasted attempts
  at the two unresponsive operators. In the healthy scenario, 29.26.2
  chose the same three operators on all three restarts; 29.26.1 used five
  in aggregate. This small sample does not establish rotation parity across
  restarts. The governor peaked at seven of eight streams, including probes.
- With all providers healthy, there was no failed stream to avoid. The
  first batch was effectively unchanged (+1 ms), but median total time
  increased **46 ms** and the new build opened five extra public probes.
  The benchmark does not show a universal latency or total-stream win.

Do not extrapolate these timings to a real Tor wallet. The tested improvement
is the admission rule under controlled provider failures. For a real
measurement, follow the [Sparrow A/B procedure](../../tests/manual/README.md#wallet-recovery-in-sparrow-29261-versus-29262)
with separate caches; the production wallet and its baseline are not part
of this fixture.
