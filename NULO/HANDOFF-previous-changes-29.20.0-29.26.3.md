# Contenido retirado de la documentación (archivo temporal)

Este bloque reproduce literalmente las secciones «Last change» / «Previous change»
que `docs/HANDOFF.md` tenía antes de la limpieza (29.20.0 a 29.26.3).
Procedencia: `git show 61bbde2:docs/HANDOFF.md` (última versión publicada,
alpha.29.26.3). No está enlazado desde ninguna guía ni se valida con
`scripts/check-docs.sh`: es material histórico para revisión personal.

Su contenido sigue en `CHANGELOG.md` (detalle por release) y en la
documentación viva donde sigue siendo cierto. Si no lo necesitas, borra
la carpeta `NULO/` completa.

## Last change (alpha.29.26.3): timestamp wallet diagnostics

For the next cold Sparrow run, align the UTC debug timestamps with Sparrow's
local time, then follow `conn=` and `wallet block:`. `index_miss`
does not mean *unconfirmed*. No change to fetching, timeouts or Tor limits.

## Previous change (alpha.29.26.2): qualify external wallet routes

`crates/el-rawtx/src/qualification.rs` runs public probes with a single
background worker. The txid scheduler and block race check the result
again before opening a connection. Expired or paused routes cannot use
wallet traffic to become eligible again. See
[OPERATIONS](OPERATIONS.md#external-wallet-route-qualification) for the
metrics to record on several clean-cache Sparrow runs. The
[offline A/B](benchmarks/provider-qualification.md) compares the published
29.26.1 and 29.26.2 binaries with 239 synthetic transactions. Mainnet A/B
remains pending, as do AUD-198 and AUD-199; do not clear a live node's cache
to test.

## Previous change (alpha.29.26.1): stream admission in wallet recovery

`TorGovernor` now separates the two wallet demands: a ready per-txid batch and
a block download. Each has its own queue and they alternate at every freed
stream, so a backlog of block downloads can no longer starve the cheap path
(AUD-201). `wallet.rs` retries a failed per-txid round while the request still
has time, instead of leaving the transaction with no path until the block
failure memory expires. The 239-transaction load keeps both paths running:
`a_cold_load_keeps_the_txid_path_while_blocks_download`, and the admission
property is pinned by `a_ready_txid_batch_takes_its_turn_instead_of_the_whole_block_queue`
and `ready_batches_do_not_starve_the_block_fallback`.

Pending: a real mainnet load (Sparrow, cold, new wallet, Tor) with this
release, the `doctor history <hash>` output of the 1.3M-row script (AUD-198),
and a decision on AUD-199.

## Previous change (alpha.29.26.0): wallet recovery from zero

`crates/el-rawtx/src/wallet.rs` observes one shared transaction job and one
shared block job per indexed transaction, each waiter with its own deadline;
the first verified answer wins. The per-txid scheduler admits work only when a
server, session and stream are all available and wakes on events instead of
requeueing every 3 s. The governor's release and wait predicates share one
lock (AUD-200). Esplora-tx cancels its request and closes the socket before
returning the stream. A temporary expiry of a confirmed transaction closes the
Electrum connection instead of answering a per-item `-32000` (AUD-045). Short
live reorg/catch-up reads wait for the next state notification. `preset = "max"`
and `rawtx.path = "block"` now disable txid requests in prefetch too; AUD-199
records that standalone txid proofs do not authenticate SegWit witnesses.

Pending: a real mainnet load (Sparrow, cold, new wallet, Tor) with this
release, the `doctor history <hash>` output of the 1.3M-row script (AUD-198),
and a decision on AUD-199.

## Previous change (alpha.29.25.2): real stream budget

- One governor permit per open Tor stream (race attempt, Electrum session,
  Esplora-tx request), held until the connection ends; hedges never wait for
  a stream. Per-txid sessions wait at most 3 s in all and take the stream
  last; local congestion requeues without spending servers (AUD-193, 196).
- Every `unavailable` failure of a confirmed transaction is retried inside
  the wallet deadline and the retry waits out the failure memory (AUD-194).
- Only the local Tor's refusal pauses the governor; 429 is per provider
  (AUD-195). The SOCKS "os error 11" is a circuit timeout.
- Bounded history reads, script hash in the `-32000` error, `e-lite doctor
  history|history-top` (read-only) for the open 1.3M-row script (AUD-198).
- Detailed `debug:` lines (per tx.get, stream, block, session; governor
  summary every 10 s) with an opaque per-process tag instead of hashes.
- Pending: a real mainnet load (Sparrow, cold, new wallet) to confirm it,
  and the `doctor history <hash>` output of the 1.3M-row script (AUD-198).

## Previous change (alpha.29.25.1): Tor stream governor

- `rawtx.tor_streams` (default 8) caps every external stream (blocks, txid
  sessions, decoys) with wallet > prefetch > decoy priority; a refused stream
  (EAGAIN, 429) pauses new streams 0.5-4 s and any success lifts the pause.
- Onion clients (loopback peers) get 1800 s idle / 120 s write timeouts.
- Provider heartbeat keeps circuits warm while the node serves wallets.

## Previous change (alpha.29.25.0): the cheap path first

- `auto` runs two per-txid session attempts (6 s) before any block; the block
  gets 20 s of network and retries for transient failures inside the 45 s
  wallet deadline. `txid_race` (false by default) shortens the txid budget to
  2.5 s for an earlier block start.

## Previous change (alpha.29.24.1): subscribe retry and lock recovery

- `blockchain.scripthash.subscribe` retries once after 50 ms on a transient
  error instead of returning -32000 (Sparrow stores a null transaction).
- Poisoned mempool/index read locks are recovered with a warning.
- `debug: rpc error ... message=...` (redacted) for diagnosis.

## Previous change (alpha.29.24.0): cold wallet loads

- `auto` gives the per-txid path 1.5 s before starting the block; healthy
  loads download no blocks.
- `wallet_deadline_seconds` defaults to 35 s (queue ~27 s + 8 s network);
  a cold burst waits for a slot instead of failing a confirmed tx (AUD-045).
- One retry bypassing the failure memory before an error is returned.

## Previous change (alpha.29.23.0): P2P budget and provider quota

- `rawtx.p2p_blocks_per_peer` (1..4, default 3): per-peer in-flight budget.
- `rawtx.provider_quota` (0 or 1.0..4.0, default 1.5): long-term per-operator
  quota as a multiple of the fair share; over-quota operators rotate behind
  the others.

## Previous change (alpha.29.22.0): configurable history prefetch

- `rawtx.prefetch_history_blocks` (0..4096, default 0 = auto): block mode
  prefetches up to 256 blocks per history, per-txid mode only on demand; an
  explicit value enables it in both modes.
- The prefetch queue is sized from that value (256..4096).

## Previous change (alpha.29.21.0): concurrent race and wallet config

- `rawtx.path` (auto/tx/block), `wallet_deadline_seconds`, `txid_attempt_seconds`,
  `txid_race`, `decoy_sessions`, `sessions_total` and `jitter_ms`, validated,
  with defaults that keep the previous behaviour.
- `auto` races the in-flight block against the per-txid path (250 ms head
  start, one request deadline) and the first verified answer wins.
- Operator dedupe in the candidate set, HTTP/2 for Esplora, and
  `/status.rawtx.block_joins` + `race.wins_txid/wins_block`.

## Previous change (alpha.29.20.0): wallet rawtx budgets and block join

- Session budget 40 s -> 3 s; wallet txid budget 8 s; wallet block budget 20 s
  (12 s queue + 8 s network) instead of 60 s network + 120 s queue.
- A failed session records its real latency, not the whole budget.
- Adaptive batch size per server (50 -> halved down to 2) after a provider
  closes; `electrum.blockstream.info` accepts 20 txids and closes above that.
- A `transaction.get` of a pruned tx starts the block download on demand and
  joins the download already in flight instead of spending the txid budget.
- Decoy wallets use a quarter of the session slots; real sessions go first.

Measured over Tor: a 50-tx Electrum batch answers in 0.3-1.2 s and a 1.9 MB
block downloads at ~581 KiB/s. The old 40/60 s budgets were ~20x the p95.

## Previous change (alpha.29.19.2): closed output

A one-shot command whose stdout is closed early (`e-lite status --json | head
-1`) ends quietly by SIGPIPE instead of a FATAL abort (AUD-192); the daemons
still ignore SIGPIPE.

## Previous change (alpha.29.19.1): cleanup and reorganization

No behaviour change except the
all-in-one `--quick` fix; see `CHANGELOG.md`. Documentation has
one home per topic (`docs/README.md` is the map), the pipeline is split under
`bin/e-lite/src/pipeline/`, and the source layout is in `ARCHITECTURE.md`.

