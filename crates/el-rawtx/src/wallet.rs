//! Wallet recovery: local data first, then observe the independent txid and
//! block jobs through one event. No job owns the wallet's response deadline.

use super::*;

/// Rounds of `MAX_TX_ATTEMPTS` servers for one wallet request. The first is
/// the normal attempt; later ones run while the request still has time and the
/// block path has not answered, so a failing provider never leaves it without
/// any path (not even while it waits out the block failure memory).
const MAX_TXID_ROUNDS: u32 = 3;

impl Resolver {
    pub(super) fn recover_wallet(
        &self,
        ctx: u64,
        txid: Hash32,
        height: u32,
        path: &mut &'static str,
        connected: &dyn Fn() -> bool,
    ) -> Result<String> {
        let deadline = Instant::now() + self.wallet_deadline;
        let rec = self.block_record(height)?;
        if let Some(hex) = self.cached_tx(&rec, txid) {
            self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
            *path = "cache";
            return Ok(hex);
        }
        let pruned = height < self.prune_height.load(Ordering::Relaxed);
        if let Some((block, _)) = self.local_block(&rec, !pruned) {
            return self.serve_block_tx(&rec, &block, txid, None);
        }

        *path = "external";
        self.start_provider_probes();
        let t = self
            .position_of(txid, height)
            .map(|pos| TxRef { txid, height, pos });
        let mut tx = if pruned {
            self.tx_mode
                .as_ref()
                .zip(t)
                .and_then(|(mode, t)| mode.request(ctx, t, rec.hash))
        } else {
            None
        };
        let tried_tx = tx.is_some();
        let now = Instant::now();
        let recovery_started = now;
        let head_start = if self.path == ExternalPath::Auto && self.txid_race {
            TXID_HEAD_START
        } else {
            self.txid_attempt * 2
        };
        let tx_head_start = tried_tx
            && self
                .tx_mode
                .as_ref()
                .is_some_and(txbatch::TxMode::routes_ready);
        let fallback_at = if tx_head_start {
            (now + head_start).min(deadline.checked_sub(MIN_ATTEMPT).unwrap_or(now))
        } else {
            now
        };
        let mut block = self.join_block(&rec.hash, Priority::Wallet);
        let mut joined = block.is_some();
        if joined {
            self.log_wallet_block(
                ctx,
                &txid,
                height,
                "join",
                "block_in_flight",
                recovery_started,
            );
        }
        let mut retry_at = fallback_at;
        let mut last_error = None;
        let mut tx_failure = None;
        let mut block_terminal = false;
        let mut noted_stall = false;
        let mut block_started = block.is_some();
        // A per-txid round is up to `MAX_TX_ATTEMPTS` servers. When it fails
        // and the block path has not answered, another round is worth it: the
        // alternative is waiting out a block download that may fail, or its
        // failure memory, with no path at all. That is how a straggler reached
        // the wallet deadline and closed the connection.
        let mut tx_rounds = u32::from(tried_tx);
        let mut tx_retry_at = None;
        loop {
            let seq = self.recovery_event.sequence();
            if !connected() {
                return Err(RawtxError::Unavailable("wallet disconnected".into()));
            }
            if let Some(result) = tx.as_ref().and_then(txbatch::Ticket::result) {
                match result {
                    Ok(hex) => {
                        *path = "txid";
                        self.stats.txid_wins.fetch_add(1, Ordering::Relaxed);
                        if block.is_some() {
                            self.stats.race_wins_txid.fetch_add(1, Ordering::Relaxed);
                        }
                        return Ok(hex);
                    }
                    Err(e) => {
                        tx = None;
                        tx_failure = Some(e);
                        if !noted_stall {
                            retry_at = Instant::now();
                        }
                        if pruned && tx_rounds < MAX_TXID_ROUNDS {
                            tx_retry_at = Some(Instant::now() + MIN_ATTEMPT);
                        }
                    }
                }
            }
            let now = Instant::now();
            if let Some(result) = block.as_ref().and_then(race::BlockLease::result) {
                block = None;
                match result {
                    Ok(verified) => {
                        *path = if joined { "block (joined)" } else { "block" };
                        if joined {
                            self.stats.block_joins.fetch_add(1, Ordering::Relaxed);
                        }
                        if tried_tx {
                            self.stats.race_wins_block.fetch_add(1, Ordering::Relaxed);
                        }
                        return self.serve_block_tx(&rec, &verified, txid, t);
                    }
                    Err(error) => {
                        block_terminal = !error.is_retryable_wallet();
                        last_error = Some(error);
                        retry_at = self.retry_block_at(&rec.hash, now);
                    }
                }
            }
            if now >= deadline || (block_terminal && tx.is_none()) {
                // Another path can publish while a block failure is being
                // handled, even when this request did not get a txid ticket.
                if let Some(raw) = tx
                    .as_ref()
                    .and_then(txbatch::Ticket::result)
                    .and_then(Result::ok)
                    .or_else(|| self.cached_tx(&rec, txid))
                {
                    *path = "txid (completed during fallback)";
                    return Ok(raw);
                }
                let error = if tx.is_some() {
                    // NoSource from the block path is not definitive while
                    // a configured txid source is still trying to answer.
                    last_error.filter(RawtxError::is_retryable_wallet)
                } else if last_error
                    .as_ref()
                    .is_some_and(|e| matches!(e, RawtxError::NoSource(_)))
                {
                    tx_failure.or(last_error)
                } else {
                    last_error.or(tx_failure)
                };
                return Err(error.unwrap_or_else(|| {
                    RawtxError::Unavailable("wallet recovery deadline reached".into())
                }));
            }
            let mut wake_at = deadline;
            if tx.is_none() && pruned && tx_rounds < MAX_TXID_ROUNDS && tx_retry_at.is_some() {
                match tx_retry_at {
                    Some(at) if now >= at => {
                        tx_rounds += 1;
                        tx = self
                            .tx_mode
                            .as_ref()
                            .zip(t)
                            .and_then(|(mode, t)| mode.request(ctx, t, rec.hash));
                        tx_retry_at = if tx.is_some() {
                            None
                        } else {
                            Some(now + MIN_ATTEMPT)
                        };
                        if tx.is_some() {
                            continue;
                        }
                    }
                    Some(at) => wake_at = wake_at.min(at),
                    None => {}
                }
            }
            if block.is_none() && !block_terminal {
                if let Some(existing) = self.join_block(&rec.hash, Priority::Wallet) {
                    self.log_wallet_block(
                        ctx,
                        &txid,
                        height,
                        "join",
                        "block_in_flight",
                        recovery_started,
                    );
                    joined = true;
                    block_started = true;
                    block = Some(existing);
                    continue;
                }
                if now < retry_at {
                    wake_at = wake_at.min(retry_at);
                } else if deadline - now
                    >= if block_started {
                        MIN_ATTEMPT
                    } else {
                        MIN_ATTEMPT / 2
                    }
                {
                    if tried_tx && !noted_stall {
                        self.stats.txid_stalls.fetch_add(1, Ordering::Relaxed);
                        noted_stall = true;
                    }
                    match self.start_block(&rec, Priority::Wallet, Some(deadline)) {
                        Ok(Some(lease)) => {
                            let (action, reason) = if lease.joined_existing {
                                ("join", "block_in_flight")
                            } else if block_started {
                                ("start", "block_retry")
                            } else if !tried_tx {
                                ("start", "txid_path_unavailable")
                            } else if !tx_head_start {
                                ("start", "txid_operators_insufficient")
                            } else if tx_failure.is_some() {
                                ("start", "txid_round_failed")
                            } else {
                                ("start", "txid_head_start_elapsed")
                            };
                            self.log_wallet_block(
                                ctx,
                                &txid,
                                height,
                                action,
                                reason,
                                recovery_started,
                            );
                            joined = false;
                            block_started = true;
                            block = Some(lease);
                            continue;
                        }
                        Ok(None) => {} // woken on capacity, no provider attempt spent
                        Err(error) => {
                            block_terminal = !error.is_retryable_wallet();
                            last_error = Some(error);
                            retry_at = self.retry_block_at(&rec.hash, now);
                            continue;
                        }
                    }
                }
            }
            self.recovery_event.wait(seq, Some(wake_at));
        }
    }

    fn log_wallet_block(
        &self,
        ctx: u64,
        txid: &Hash32,
        height: u32,
        action: &str,
        reason: &str,
        recovery_started: Instant,
    ) {
        if debug_logs() {
            let (qualified, required) = self
                .tx_mode
                .as_ref()
                .map_or((0, 0), |m| m.route_operator_counts());
            let wait_ms = recovery_started.elapsed().as_millis();
            el_model::debug_event!(
                "wallet block: conn={ctx} obj={} height={height} action={action} reason={reason} \
                 txid_operators={qualified}/{required} external_wait_ms={wait_ms}",
                log_tag(txid)
            );
        }
    }

    fn retry_block_at(&self, hash: &Hash32, now: Instant) -> Instant {
        // A source failure stays remembered for its existing cooldown.
        // For local queue expiry or a pool of cooling sources, don't spin.
        self.failure_expiry(hash)
            .unwrap_or(now + Duration::from_millis(400))
    }

    fn serve_block_tx(
        &self,
        rec: &el_model::BlockRecord,
        block: &bitcoin::Block,
        txid: Hash32,
        t: Option<TxRef>,
    ) -> Result<String> {
        let hex = self
            .cache_txs(rec, block, &[txid])?
            .pop()
            .ok_or_else(|| RawtxError::Verify("transaction is not in the block".into()))?;
        if let Some((mode, t)) = self.tx_mode.as_ref().zip(t) {
            mode.publish(t, rec.hash, &hex);
        }
        Ok(hex)
    }
}
