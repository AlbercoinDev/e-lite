//! Wallet recovery: local data first, then observe the independent txid and
//! block jobs through one event. No job owns the wallet's response deadline.

use super::*;

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
        let head_start = if self.path == ExternalPath::Auto && self.txid_race {
            TXID_HEAD_START
        } else {
            self.txid_attempt * 2
        };
        let fallback_at = if tried_tx {
            (now + head_start).min(deadline.checked_sub(MIN_ATTEMPT).unwrap_or(now))
        } else {
            now
        };
        let mut block = self.join_block(&rec.hash, Priority::Wallet);
        let mut joined = block.is_some();
        let mut retry_at = fallback_at;
        let mut last_error = None;
        let mut tx_failure = None;
        let mut block_terminal = false;
        let mut noted_stall = false;
        let mut block_started = block.is_some();
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
            if block.is_none() && !block_terminal {
                if let Some(existing) = self.join_block(&rec.hash, Priority::Wallet) {
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
