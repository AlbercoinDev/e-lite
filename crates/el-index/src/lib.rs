//! Block application, undo records and reorg rewind.

pub mod synth;

use el_model::codec::SCHEMA_VERSION;
use el_model::{
    merkle_root, BlockRecord, Hash32, HistoryEffect, IndexedTip, Outpoint, ScriptHash, UndoCreated,
    UndoHistory, UndoRecord, UndoSpent, UtxoValue, HEADER_LEN,
};
use el_store::{Store, StoreError};
use rocksdb::WriteBatch;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum IndexError {
    Message(String),
    PrevoutMissing {
        txid: String,
        vout: u32,
        height: u32,
    },
    MerkleMismatch {
        height: u32,
    },
    NonContiguous {
        height: u32,
        expected_prev: String,
        got_prev: String,
    },
    NoUndo {
        height: u32,
    },
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IndexError::Message(m) => write!(f, "{m}"),
            IndexError::PrevoutMissing { txid, vout, height } => write!(
                f,
                "prevout {txid}:{vout} missing while applying height {height} (refusing to invent value)"
            ),
            IndexError::MerkleMismatch { height } => {
                write!(f, "merkle root mismatch at height {height}")
            }
            IndexError::NonContiguous {
                height,
                expected_prev,
                got_prev,
            } => write!(
                f,
                "non-contiguous block {height}: expected prev {expected_prev}, got {got_prev}"
            ),
            IndexError::NoUndo { height } => {
                write!(f, "no undo record for height {height} (outside reorg window)")
            }
        }
    }
}

impl std::error::Error for IndexError {}

impl From<StoreError> for IndexError {
    fn from(e: StoreError) -> Self {
        IndexError::Message(e.to_string())
    }
}

impl From<el_model::CodecError> for IndexError {
    fn from(e: el_model::CodecError) -> Self {
        IndexError::Message(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, IndexError>;

fn err<T>(m: impl Into<String>) -> Result<T> {
    Err(IndexError::Message(m.into()))
}

pub fn hash_from_txid(txid: &bitcoin::Txid) -> Result<Hash32> {
    Hash32::from_hex(&txid.to_string()).map_err(IndexError::Message)
}

pub fn hash_from_blockhash(h: &bitcoin::BlockHash) -> Result<Hash32> {
    Hash32::from_hex(&h.to_string()).map_err(IndexError::Message)
}

/// In-memory view of outputs created but not yet committed (group commit).
#[derive(Default)]
pub struct GroupOverlay {
    created: HashMap<Outpoint, UtxoValue>,
    deleted: HashSet<Outpoint>,
}

impl GroupOverlay {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.created.clear();
        self.deleted.clear();
    }

    pub fn get(&self, store: &Store, op: &Outpoint) -> Result<Option<UtxoValue>> {
        if let Some(v) = self.created.get(op) {
            return Ok(Some(*v));
        }
        if self.deleted.contains(op) {
            return Ok(None);
        }
        Ok(store.get_utxo(op)?)
    }

    pub fn contains_created(&self, op: &Outpoint) -> bool {
        self.created.contains_key(op)
    }

    pub fn contains_deleted(&self, op: &Outpoint) -> bool {
        self.deleted.contains(op)
    }

    /// Same semantics as `get`, but consults the block's prefetched prevouts
    /// before falling back to a single point lookup.
    pub fn get_with(
        &self,
        store: &Store,
        op: &Outpoint,
        prefetched: &HashMap<Outpoint, UtxoValue>,
    ) -> Result<Option<UtxoValue>> {
        if let Some(v) = self.created.get(op) {
            return Ok(Some(*v));
        }
        if self.deleted.contains(op) {
            return Ok(None);
        }
        if let Some(v) = prefetched.get(op) {
            return Ok(Some(*v));
        }
        Ok(store.get_utxo(op)?)
    }

    fn insert(&mut self, op: Outpoint, v: UtxoValue) {
        self.deleted.remove(&op);
        self.created.insert(op, v);
    }

    fn remove(&mut self, op: &Outpoint) -> Option<UtxoValue> {
        if let Some(v) = self.created.remove(op) {
            return Some(v);
        }
        self.deleted.insert(*op);
        None
    }
}

#[derive(Debug)]
pub struct ApplyOutcome {
    pub undo: UndoRecord,
    pub txids: Vec<Hash32>,
    pub txnum_base: u64,
    pub tip: IndexedTip,
    /// Scripts whose confirmed history/UTXO view changed in this block.
    pub touched_scripts: HashSet<ScriptHash>,
}

fn is_provably_unspendable(script: &[u8]) -> bool {
    script.first() == Some(&0x6a) // OP_RETURN
}

/// A block already decoded, hash-checked and Merkle-validated in a reader
/// thread. The apply thread only performs continuity, UTXO work and batch
/// writes, which overlaps the CPU-heavy decode with the previous block.
pub struct PreparedBlock {
    block: bitcoin::Block,
    block_hash: Hash32,
    txids: Vec<Hash32>,
    out_script_hashes: Vec<Vec<ScriptHash>>,
    /// Raw serialized block size, used to bound a commit group by bytes.
    raw_size: usize,
}

/// Prevouts resolved from the committed UTXO set before apply. A speculative
/// superset is fine: apply consults the in-memory group overlay first and falls
/// back to a point lookup for anything absent.
pub type PrefetchedPrevouts = HashMap<Outpoint, UtxoValue>;

/// Resolve the prevouts a block will need, in one batch. This is safe to run in
/// a reader thread while another block is being applied: it reads committed
/// state only, and misses are resolved from the overlay during apply. Skipping
/// outputs created by the same block is what makes the speculation sound.
fn prevouts_needed(prepared: &PreparedBlock) -> Result<Vec<Outpoint>> {
    let created_txids: HashSet<Hash32> = prepared.txids.iter().copied().collect();
    let mut needed: HashSet<Outpoint> = HashSet::with_capacity(prepared.txids.len());
    for tx in &prepared.block.txdata {
        for input in &tx.input {
            if input.previous_output.is_null() {
                continue;
            }
            let op = Outpoint {
                txid: hash_from_txid(&input.previous_output.txid)?,
                vout: input.previous_output.vout,
            };
            if created_txids.contains(&op.txid) {
                continue;
            }
            needed.insert(op);
        }
    }
    Ok(needed.into_iter().collect())
}

pub fn prefetch_prevouts(store: &Store, prepared: &PreparedBlock) -> Result<PrefetchedPrevouts> {
    let needed = prevouts_needed(prepared)?;
    Ok(store.get_utxos(&needed)?)
}

/// Chunked prefetch with a soft deadline. A single `batched_multi_get` cannot
/// be interrupted, so the deadline is checked between 512-key chunks; the
/// apply path already falls back to point lookups for anything missing.
pub fn prefetch_prevouts_with_deadline(
    store: &Store,
    prepared: &PreparedBlock,
    deadline: Duration,
) -> Result<PrefetchedPrevouts> {
    let needed = prevouts_needed(prepared)?;
    let started = Instant::now();
    let mut out = HashMap::with_capacity(needed.len());
    for chunk in needed.chunks(512) {
        out.extend(store.get_utxos(chunk)?);
        if started.elapsed() >= deadline {
            break;
        }
    }
    Ok(out)
}

fn compute_out_script_hashes(block: &bitcoin::Block) -> Vec<Vec<ScriptHash>> {
    block
        .txdata
        .iter()
        .map(|tx| {
            tx.output
                .iter()
                .map(|out| ScriptHash::from_script(out.script_pubkey.as_bytes()))
                .collect()
        })
        .collect()
}

fn compute_txids(block: &bitcoin::Block) -> Result<Vec<Hash32>> {
    block
        .txdata
        .iter()
        .map(|tx| hash_from_txid(&tx.compute_txid()))
        .collect::<Result<Vec<_>>>()
}

fn validate_merkle(block: &bitcoin::Block, txids: &[Hash32], height: u32) -> Result<()> {
    let expected_merkle =
        Hash32::from_hex(&block.header.merkle_root.to_string()).map_err(IndexError::Message)?;
    match merkle_root(txids) {
        Some(root) if root == expected_merkle => Ok(()),
        _ => Err(IndexError::MerkleMismatch { height }),
    }
}

/// Decode and validate a block outside the apply thread.
pub fn prepare_block_bytes(
    bytes: &[u8],
    expected_hash: &str,
    height: u32,
) -> Result<PreparedBlock> {
    let block: bitcoin::Block = bitcoin::consensus::deserialize(bytes)
        .map_err(|e| IndexError::Message(format!("cannot decode block {height}: {e}")))?;
    let block_hash = hash_from_blockhash(&block.block_hash())?;
    if block_hash.to_hex() != expected_hash {
        return Err(IndexError::Message(format!(
            "block hash mismatch at {height}: expected {expected_hash}, computed {}",
            block_hash.to_hex()
        )));
    }
    let txids = compute_txids(&block)?;
    validate_merkle(&block, &txids, height)?;
    let out_script_hashes = block
        .txdata
        .iter()
        .map(|tx| {
            tx.output
                .iter()
                .map(|out| ScriptHash::from_script(out.script_pubkey.as_bytes()))
                .collect()
        })
        .collect();
    Ok(PreparedBlock {
        block,
        block_hash,
        txids,
        out_script_hashes,
        raw_size: bytes.len(),
    })
}

impl PreparedBlock {
    /// Serialized size of the block that produced this prepared unit.
    pub fn raw_size(&self) -> usize {
        self.raw_size
    }
}

/// Apply one block into `batch`, updating the overlay. The caller commits the
/// batch (plus the tip) atomically and then appends `txids` to the flat file.
/// This compatibility wrapper prepares the block in the caller's thread; the
/// indexer uses `prepare_block_bytes` in its reader threads and `apply_prepared`.
pub fn apply_block(
    store: &Store,
    height: u32,
    block: &bitcoin::Block,
    prev_tip: Option<IndexedTip>,
    overlay: &mut GroupOverlay,
    batch: &mut WriteBatch,
    store_undo: bool,
) -> Result<ApplyOutcome> {
    let block_hash = hash_from_blockhash(&block.block_hash())?;
    let txids = compute_txids(block)?;
    validate_merkle(block, &txids, height)?;
    let out_script_hashes = compute_out_script_hashes(block);
    apply_inner(
        store,
        height,
        block,
        block_hash,
        &txids,
        &out_script_hashes,
        prev_tip,
        overlay,
        batch,
        store_undo,
        store_undo,
        None,
    )
}

pub fn apply_prepared(
    store: &Store,
    height: u32,
    prepared: &PreparedBlock,
    prev_tip: Option<IndexedTip>,
    overlay: &mut GroupOverlay,
    batch: &mut WriteBatch,
    store_undo: bool,
) -> Result<ApplyOutcome> {
    apply_prepared_with_prefetch(
        store, height, prepared, prev_tip, overlay, batch, store_undo, None,
    )
}

/// Apply a prepared block using prevouts already fetched in a reader thread.
#[allow(clippy::too_many_arguments)]
pub fn apply_prepared_with_prefetch(
    store: &Store,
    height: u32,
    prepared: &PreparedBlock,
    prev_tip: Option<IndexedTip>,
    overlay: &mut GroupOverlay,
    batch: &mut WriteBatch,
    store_undo: bool,
    prefetched: Option<&PrefetchedPrevouts>,
) -> Result<ApplyOutcome> {
    apply_inner(
        store,
        height,
        &prepared.block,
        prepared.block_hash,
        &prepared.txids,
        &prepared.out_script_hashes,
        prev_tip,
        overlay,
        batch,
        store_undo,
        store_undo,
        prefetched,
    )
}

/// Apply a prepared block and return its undo record **without** writing it
/// to `batch`. The caller writes only the records that the committed tip
/// still needs (`height >= tip - reorg_window`), so a bulk group does not
/// write undo into RocksDB just to delete it at commit time (AUD-131).
#[allow(clippy::too_many_arguments)]
pub fn apply_prepared_deferred_undo(
    store: &Store,
    height: u32,
    prepared: &PreparedBlock,
    prev_tip: Option<IndexedTip>,
    overlay: &mut GroupOverlay,
    batch: &mut WriteBatch,
    prefetched: Option<&PrefetchedPrevouts>,
) -> Result<ApplyOutcome> {
    apply_inner(
        store,
        height,
        &prepared.block,
        prepared.block_hash,
        &prepared.txids,
        &prepared.out_script_hashes,
        prev_tip,
        overlay,
        batch,
        true,
        false,
        prefetched,
    )
}

/// Write the undo records a commit at `tip_height` still needs: the reorg
/// window `[tip - window, tip]`. Older records are skipped entirely.
/// Returns how many were written.
pub fn write_window_undo<'a>(
    store: &Store,
    batch: &mut WriteBatch,
    records: impl IntoIterator<Item = &'a UndoRecord>,
    tip_height: u32,
    window: u32,
) -> Result<usize> {
    let cutoff = tip_height.saturating_sub(window);
    let mut written = 0;
    for rec in records {
        if rec.height >= cutoff && rec.height <= tip_height {
            store.put_undo(batch, rec)?;
            written += 1;
        }
    }
    Ok(written)
}

#[allow(clippy::too_many_arguments)]
fn apply_inner(
    store: &Store,
    height: u32,
    block: &bitcoin::Block,
    block_hash: Hash32,
    txids: &[Hash32],
    out_script_hashes: &[Vec<ScriptHash>],
    prev_tip: Option<IndexedTip>,
    overlay: &mut GroupOverlay,
    batch: &mut WriteBatch,
    store_undo: bool,
    write_undo: bool,
    prefetched: Option<&PrefetchedPrevouts>,
) -> Result<ApplyOutcome> {
    // `store_undo` builds the undo record; `write_undo` also puts it in the
    // batch. The deferred path builds it and lets the caller decide.
    // --- continuity -------------------------------------------------------
    let prev_hash = hash_from_blockhash(&block.header.prev_blockhash)?;
    let expected_height = prev_tip.map(|t| t.height + 1).unwrap_or(0);
    if height != expected_height {
        return err(format!(
            "height {height} does not follow tip height {}",
            expected_height.saturating_sub(1)
        ));
    }
    if let Some(tip) = prev_tip {
        if tip.block_hash != prev_hash {
            return Err(IndexError::NonContiguous {
                height,
                expected_prev: tip.block_hash.to_hex(),
                got_prev: prev_hash.to_hex(),
            });
        }
    } else if !prev_hash.is_zero() {
        return Err(IndexError::NonContiguous {
            height,
            expected_prev: Hash32::ZERO.to_hex(),
            got_prev: prev_hash.to_hex(),
        });
    }

    // Prevouts come from the reader-thread prefetch when available; otherwise
    // resolve every prevout that is not created inside this block in one
    // RocksDB multi-get. Outputs created in this block are resolved through the
    // group overlay, and outputs already spent in this group must stay missing.
    let fetched;
    let prefetched: &PrefetchedPrevouts = match prefetched {
        Some(map) => map,
        None => {
            let created_txids: HashSet<Hash32> = txids.iter().copied().collect();
            let mut needed: HashSet<Outpoint> = HashSet::with_capacity(txids.len());
            for tx in &block.txdata {
                for input in &tx.input {
                    if input.previous_output.is_null() {
                        continue;
                    }
                    let op = Outpoint {
                        txid: hash_from_txid(&input.previous_output.txid)?,
                        vout: input.previous_output.vout,
                    };
                    if created_txids.contains(&op.txid)
                        || overlay.contains_created(&op)
                        || overlay.contains_deleted(&op)
                    {
                        continue;
                    }
                    needed.insert(op);
                }
            }
            let needed: Vec<Outpoint> = needed.into_iter().collect();
            fetched = store.get_utxos(&needed)?;
            &fetched
        }
    };

    let txnum_base = prev_tip.map(|t| t.txnum).unwrap_or(0);
    let header_bytes: [u8; HEADER_LEN] = {
        let raw = bitcoin::consensus::serialize(&block.header);
        if raw.len() != HEADER_LEN {
            return err("serialized header is not 80 bytes");
        }
        let mut h = [0u8; HEADER_LEN];
        h.copy_from_slice(&raw);
        h
    };

    let mut touched_scripts: HashSet<ScriptHash> = HashSet::with_capacity(txids.len());
    let mut undo = UndoRecord {
        height,
        block_hash,
        ..Default::default()
    };
    // script -> (received, spent, flags)
    let mut effects: HashMap<ScriptHash, HistoryEffect> =
        HashMap::with_capacity(txids.len().saturating_mul(2));
    // outputs created inside this very block (for undo de-duplication)
    let mut created_here: HashMap<Outpoint, ScriptHash> = HashMap::new();
    let mut drained: Vec<(ScriptHash, HistoryEffect)> = Vec::new();

    for (pos, tx) in block.txdata.iter().enumerate() {
        let txid = txids[pos];
        let tx_pos = pos as u32;
        let coinbase = pos == 0 && tx.is_coinbase();
        let genesis_coinbase = height == 0 && coinbase;

        // inputs
        for input in &tx.input {
            if input.previous_output.is_null() {
                continue; // coinbase
            }
            let op = Outpoint {
                txid: hash_from_txid(&input.previous_output.txid)?,
                vout: input.previous_output.vout,
            };
            let utxo = overlay.get_with(store, &op, prefetched)?.ok_or_else(|| {
                IndexError::PrevoutMissing {
                    txid: op.txid.to_hex(),
                    vout: op.vout,
                    height,
                }
            })?;
            // Remove from the live view.
            let was_created_here = if store_undo {
                created_here.remove(&op).is_some()
            } else {
                false
            };
            overlay.remove(&op);
            store.delete_utxo(batch, &op, utxo.script_hash)?;
            let e = effects.entry(utxo.script_hash).or_default();
            e.spent_sat = e.spent_sat.saturating_add(utxo.value_sat);
            if store_undo {
                if was_created_here {
                    // Created and spent inside this same block: after undo it
                    // simply does not exist, so drop it from the created list.
                    undo.created.retain(|c| c.outpoint != op);
                } else {
                    undo.spent.push(UndoSpent { outpoint: op, utxo });
                }
            }
        }

        // outputs
        for (vout, out) in tx.output.iter().enumerate() {
            let script = out.script_pubkey.as_bytes();
            let script_hash = out_script_hashes
                .get(pos)
                .and_then(|v| v.get(vout))
                .copied()
                .unwrap_or_else(|| ScriptHash::from_script(script));
            let value_sat = out.value.to_sat();
            let e = effects.entry(script_hash).or_default();
            e.received_sat = e.received_sat.saturating_add(value_sat);

            if genesis_coinbase || is_provably_unspendable(script) {
                // Not spendable: no UTXO entry (matches Core semantics), but the
                // history relation is still recorded so the script is queryable.
                continue;
            }
            let op = Outpoint {
                txid,
                vout: vout as u32,
            };
            let value = UtxoValue {
                value_sat,
                script_hash,
                creation_height: height,
                coinbase,
            };
            store.put_utxo(batch, &op, &value)?;
            overlay.insert(op, value);
            if store_undo {
                created_here.insert(op, script_hash);
                undo.created.push(UndoCreated {
                    outpoint: op,
                    script_hash,
                });
            }
        }

        drained.extend(effects.drain());
        drained.sort_by_key(|e| e.0);
        for (script_hash, effect) in &drained {
            touched_scripts.insert(*script_hash);
            store.put_history(batch, *script_hash, height, tx_pos, effect)?;
            if store_undo {
                undo.history.push(UndoHistory {
                    script_hash: *script_hash,
                    height,
                    tx_pos,
                });
            }
        }
        drained.clear();

        store.put_occurrence(batch, txid, height, tx_pos)?;
        if store_undo {
            undo.txids.push(txid);
        }
    }

    let rec = BlockRecord {
        height,
        hash: block_hash,
        header: header_bytes,
        tx_count: txids.len() as u32,
        txnum_base,
    };
    store.put_block(batch, &rec)?;
    if store_undo && write_undo {
        store.put_undo(batch, &undo)?;
    }

    let tip = IndexedTip {
        height,
        block_hash,
        schema_version: SCHEMA_VERSION,
        txnum: txnum_base + txids.len() as u64,
    };

    Ok(ApplyOutcome {
        undo,
        txids: txids.to_vec(),
        txnum_base,
        tip,
        touched_scripts,
    })
}

/// Undo a single block (top-down). Returns the previous tip.
pub fn revert_block(store: &Store, height: u32, batch: &mut WriteBatch) -> Result<IndexedTip> {
    let undo = store
        .get_undo(height)?
        .ok_or(IndexError::NoUndo { height })?;
    if undo.height != height {
        return err(format!("undo record height mismatch for {height}"));
    }

    for c in &undo.created {
        store.delete_utxo(batch, &c.outpoint, c.script_hash)?;
    }
    for s in &undo.spent {
        store.put_utxo(batch, &s.outpoint, &s.utxo)?;
    }
    for h in &undo.history {
        store.delete_history(batch, h.script_hash, h.height, h.tx_pos)?;
    }
    for (pos, txid) in undo.txids.iter().enumerate() {
        store.delete_occurrence(batch, *txid, height, pos as u32)?;
    }
    store.delete_block(batch, height)?;
    store.delete_undo(batch, height)?;

    if height == 0 {
        return Ok(IndexedTip {
            height: 0,
            block_hash: Hash32::ZERO,
            schema_version: SCHEMA_VERSION,
            txnum: 0,
        });
    }
    let prev = store
        .get_block(height - 1)?
        .ok_or_else(|| IndexError::Message(format!("missing block {}", height - 1)))?;
    Ok(IndexedTip {
        height: prev.height,
        block_hash: prev.hash,
        schema_version: SCHEMA_VERSION,
        txnum: prev.txnum_base + prev.tx_count as u64,
    })
}

/// Rewind the tip down to `target` (inclusive), one committed batch per block.
pub fn rewind_to(store: &Store, target: u32) -> Result<()> {
    let mut tip = match store.committed_tip()? {
        Some(t) => t,
        None => return Ok(()),
    };
    while tip.height > target {
        let mut batch = WriteBatch::default();
        let new_tip = revert_block(store, tip.height, &mut batch)?;
        store.commit_tip_only(batch, &new_tip)?;
        tip = new_tip;
    }
    store.truncate_txids(tip.txnum)?;
    Ok(())
}

/// Drop undo records older than the configured window, keeping reorgs bounded.
pub fn prune_undo(
    store: &Store,
    tip_height: u32,
    window: u32,
    batch: &mut WriteBatch,
) -> Result<()> {
    if tip_height <= window {
        return Ok(());
    }
    let cutoff = tip_height - window;
    // The undo family is only recovery metadata. Delete every record below the
    // window in one pass: batch sizes above 64 previously left a growing tail
    // of stale records and made the canonical digest depend on commit timing.
    store.delete_undo_below(cutoff, batch)?;
    Ok(())
}

/// Aggregate a script's confirmed balance from its UTXO set.
pub fn confirmed_balance(store: &Store, script: ScriptHash) -> Result<u64> {
    let mut total = 0u64;
    for (_, value, _, _) in store.utxos_for_script(script)? {
        total = total.saturating_add(value);
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::synth::{ChainGen, ChainSpec, Oracle};
    use super::*;
    use el_store::failpoints;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "e-lite-index-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn apply_group(store: &Store, blocks: &[bitcoin::Block], start_height: u32) -> Result<()> {
        let mut overlay = GroupOverlay::new();
        let mut batch = WriteBatch::default();
        let mut all_txids = Vec::new();
        let mut tip = store.committed_tip()?;
        for (i, block) in blocks.iter().enumerate() {
            let height = start_height + i as u32;
            // Mirror the daemon: decode/validate and resolve prevouts outside
            // apply, then apply from the speculative prefetch.
            let raw = bitcoin::consensus::serialize(block);
            let prepared = prepare_block_bytes(&raw, &block.block_hash().to_string(), height)?;
            let prevouts = prefetch_prevouts(store, &prepared)?;
            let out = apply_prepared_with_prefetch(
                store,
                height,
                &prepared,
                tip,
                &mut overlay,
                &mut batch,
                true,
                Some(&prevouts),
            )?;
            all_txids.extend_from_slice(&out.txids);
            tip = Some(out.tip);
        }
        store.commit(batch, &tip.unwrap(), &all_txids)?;
        Ok(())
    }

    #[test]
    fn prepared_block_exposes_its_raw_byte_size() {
        let gen = ChainGen::new(ChainSpec {
            seed: 11,
            tx_per_block: 3,
            script_pool: 4,
        });
        let block = gen.take(1).next().unwrap();
        let raw = bitcoin::consensus::serialize(&block);
        let prepared = prepare_block_bytes(&raw, &block.block_hash().to_string(), 0).unwrap();
        assert_eq!(prepared.raw_size(), raw.len());
    }

    #[test]
    fn prefetch_deadline_returns_partial_without_panicking() {
        let dir = tmp("prefetch-deadline");
        let store = Store::open(&dir).unwrap();
        let gen = ChainGen::new(ChainSpec {
            seed: 17,
            tx_per_block: 3,
            script_pool: 4,
        });
        let block = gen.take(1).next().unwrap();
        let raw = bitcoin::consensus::serialize(&block);
        let prepared = prepare_block_bytes(&raw, &block.block_hash().to_string(), 0).unwrap();
        let got = prefetch_prevouts_with_deadline(&store, &prepared, Duration::ZERO).unwrap();
        // No UTXOs in this store; the important part is that a zero deadline
        // stops safely and leaves the apply fallback to resolve anything.
        assert!(got.is_empty());
    }

    #[test]
    fn apply_block_reports_touched_scripts() {
        failpoints::set(failpoints::NONE);
        let dir = tmp("touched");
        let store = Store::open(&dir).unwrap();
        let gen = ChainGen::new(ChainSpec {
            seed: 3,
            tx_per_block: 2,
            script_pool: 4,
        });
        let blocks: Vec<_> = gen.take(2).collect();
        let coinbase_script =
            ScriptHash::from_script(blocks[0].txdata[0].output[0].script_pubkey.as_bytes());

        let mut overlay = GroupOverlay::new();
        let mut batch = WriteBatch::default();
        let out = apply_block(&store, 0, &blocks[0], None, &mut overlay, &mut batch, true).unwrap();
        assert!(
            out.touched_scripts.contains(&coinbase_script),
            "the coinbase output script must be marked dirty"
        );
        store.commit(batch, &out.tip, &out.txids).unwrap();

        let mut overlay = GroupOverlay::new();
        let mut batch = WriteBatch::default();
        let out = apply_block(
            &store,
            1,
            &blocks[1],
            store.committed_tip().unwrap(),
            &mut overlay,
            &mut batch,
            true,
        )
        .unwrap();
        assert!(
            out.touched_scripts.contains(&coinbase_script),
            "a spent input script must be marked dirty"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oracle_parity_small_chain() {
        failpoints::set(failpoints::NONE);
        let dir = tmp("oracle");
        let store = Store::open(&dir).unwrap();
        let gen = ChainGen::new(ChainSpec {
            seed: 7,
            tx_per_block: 6,
            script_pool: 9,
        });
        let mut oracle = Oracle::new();
        let blocks: Vec<_> = gen.take(30).collect();
        for (h, b) in blocks.iter().enumerate() {
            oracle.apply(h as u32, b);
        }
        apply_group(&store, &blocks, 0).unwrap();
        oracle.assert_matches(&store).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oracle_parity_100k_transactions() {
        // 1000 blocks x 100 txs = ~100k occurrences.
        failpoints::set(failpoints::NONE);
        let dir = tmp("oracle100k");
        let store = Store::open(&dir).unwrap();
        let gen = ChainGen::new(ChainSpec {
            seed: 42,
            tx_per_block: 100,
            script_pool: 500,
        });
        let mut oracle = Oracle::new();
        let blocks: Vec<_> = gen.take(1200).collect();
        for (h, b) in blocks.iter().enumerate() {
            oracle.apply(h as u32, b);
        }
        for (gi, chunk) in blocks.chunks(32).enumerate() {
            apply_group(&store, chunk, (gi * 32) as u32).unwrap();
        }
        oracle.assert_matches(&store).unwrap();
        store.flush().unwrap();
        let tip = store.committed_tip().unwrap().unwrap();
        eprintln!(
            "100k corpus: tip {} txnum {} db_bytes {}",
            tip.height,
            tip.txnum,
            store.disk_usage_bytes()
        );
        assert!(tip.txnum >= 100_000, "corpus must exceed 100k occurrences");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_prevout_is_fatal() {
        failpoints::set(failpoints::NONE);
        let dir = tmp("prevout");
        let store = Store::open(&dir).unwrap();
        let gen = ChainGen::new(ChainSpec {
            seed: 1,
            tx_per_block: 1,
            script_pool: 6,
        });
        let genesis: Vec<_> = gen.take(1).collect();
        apply_group(&store, &genesis, 0).unwrap();

        // Block 1 spends an outpoint that was never indexed.
        let bogus = bitcoin::OutPoint {
            txid: "1111111111111111111111111111111111111111111111111111111111111111"
                .parse()
                .unwrap(),
            vout: 3,
        };
        let tx = bitcoin::Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![bitcoin::TxIn {
                previous_output: bogus,
                script_sig: bitcoin::ScriptBuf::new(),
                sequence: bitcoin::Sequence::MAX,
                witness: bitcoin::Witness::new(),
            }],
            output: vec![bitcoin::TxOut {
                value: bitcoin::Amount::from_sat(1),
                script_pubkey: bitcoin::ScriptBuf::new(),
            }],
        };
        let block = crate::synth::seal(genesis[0].block_hash(), 1, 1, vec![tx]);
        let mut overlay = GroupOverlay::new();
        let mut batch = WriteBatch::default();
        let res = apply_block(
            &store,
            1,
            &block,
            store.committed_tip().unwrap(),
            &mut overlay,
            &mut batch,
            true,
        );
        assert!(
            matches!(res, Err(IndexError::PrevoutMissing { .. })),
            "got {res:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_is_optional_per_block() {
        failpoints::set(failpoints::NONE);
        let dir = tmp("undo-optional");
        let store = Store::open(&dir).unwrap();
        let gen = ChainGen::new(ChainSpec {
            seed: 3,
            tx_per_block: 2,
            script_pool: 5,
        });
        let blocks: Vec<_> = gen.take(2).collect();
        let mut overlay = GroupOverlay::new();
        let mut batch = WriteBatch::default();
        let out =
            apply_block(&store, 0, &blocks[0], None, &mut overlay, &mut batch, false).unwrap();
        store.commit(batch, &out.tip, &out.txids).unwrap();
        assert!(
            store.get_undo(0).unwrap().is_none(),
            "historical block must not write undo"
        );

        let mut overlay = GroupOverlay::new();
        let mut batch = WriteBatch::default();
        let out = apply_block(
            &store,
            1,
            &blocks[1],
            store.committed_tip().unwrap(),
            &mut overlay,
            &mut batch,
            true,
        )
        .unwrap();
        store.commit(batch, &out.tip, &out.txids).unwrap();
        assert!(
            store.get_undo(1).unwrap().is_some(),
            "window block must write undo"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reorg_rewind_and_reapply() {
        failpoints::set(failpoints::NONE);
        let dir = tmp("reorg");
        let store = Store::open(&dir).unwrap();
        let gen = ChainGen::new(ChainSpec {
            seed: 11,
            tx_per_block: 4,
            script_pool: 6,
        });
        let mut gen = gen;
        let prefix: Vec<_> = (&mut gen).take(8).collect();
        let fork_a = gen.clone();
        let fork_b = gen.clone();
        let a: Vec<_> = fork_a.take(5).collect();
        let b: Vec<_> = fork_b.take(5).collect();

        let mut oracle = Oracle::new();
        for (h, blk) in prefix.iter().enumerate() {
            oracle.apply(h as u32, blk);
        }
        apply_group(&store, &prefix, 0).unwrap();
        for (h, blk) in a.iter().enumerate() {
            oracle.apply(8 + h as u32, blk);
        }
        apply_group(&store, &a, 8).unwrap();
        oracle.assert_matches(&store).unwrap();

        // Reorg: go back to 7 and apply branch B.
        rewind_to(&store, 7).unwrap();
        let mut oracle_b = Oracle::new();
        for (h, blk) in prefix.iter().enumerate() {
            oracle_b.apply(h as u32, blk);
        }
        for (h, blk) in b.iter().enumerate() {
            oracle_b.apply(8 + h as u32, blk);
        }
        apply_group(&store, &b, 8).unwrap();
        oracle_b.assert_matches(&store).unwrap();
        assert_eq!(
            store.committed_tip().unwrap().unwrap().block_hash,
            hash_from_blockhash(&b[4].block_hash()).unwrap()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_undo_removes_old_records_and_keeps_recent_ones() {
        failpoints::set(failpoints::NONE);
        let dir = tmp("prune");
        let store = Store::open(&dir).unwrap();
        let mut batch = Store::batch();
        for h in 0..20u32 {
            // Input-heavy records: the case that broke the undo reader.
            let mut spent = Vec::new();
            for i in 0..200u32 {
                let mut txid = [0u8; 32];
                txid[0..4].copy_from_slice(&i.to_be_bytes());
                txid[4..8].copy_from_slice(&h.to_be_bytes());
                spent.push(UndoSpent {
                    outpoint: Outpoint {
                        txid: Hash32(txid),
                        vout: i,
                    },
                    utxo: UtxoValue {
                        value_sat: i as u64,
                        script_hash: ScriptHash(Hash32([0x44; 32])),
                        creation_height: h,
                        coinbase: false,
                    },
                });
            }
            let rec = UndoRecord {
                height: h,
                block_hash: Hash32([h as u8; 32]),
                spent,
                created: Vec::new(),
                history: Vec::new(),
                txids: vec![Hash32([0x55; 32])],
            };
            store.put_undo(&mut batch, &rec).unwrap();
        }
        let tip_at_19 = IndexedTip {
            height: 19,
            block_hash: Hash32([19u8; 32]),
            schema_version: SCHEMA_VERSION,
            txnum: 0,
        };
        store.commit_tip_only(batch, &tip_at_19).unwrap();

        let mut prune_batch = Store::batch();
        prune_undo(&store, 19, 5, &mut prune_batch).unwrap();
        store.commit_tip_only(prune_batch, &tip_at_19).unwrap();

        for h in 0..14 {
            assert!(
                store.get_undo(h).unwrap().is_none(),
                "height {h} should be pruned"
            );
        }
        for h in 14..20 {
            let rec = store
                .get_undo(h)
                .unwrap()
                .unwrap_or_else(|| panic!("height {h} kept"));
            assert_eq!(rec.spent.len(), 200);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_undo_deletes_the_whole_tail_not_only_64_blocks() {
        failpoints::set(failpoints::NONE);
        let dir = tmp("prune-tail");
        let store = Store::open(&dir).unwrap();
        let mut batch = Store::batch();
        for h in 0..200u32 {
            let rec = UndoRecord {
                height: h,
                block_hash: Hash32([h as u8; 32]),
                txids: vec![Hash32([0x55; 32])],
                ..Default::default()
            };
            store.put_undo(&mut batch, &rec).unwrap();
        }
        let tip = IndexedTip {
            height: 199,
            block_hash: Hash32([199u8; 32]),
            schema_version: SCHEMA_VERSION,
            txnum: 0,
        };
        store.commit_tip_only(batch, &tip).unwrap();

        let mut prune = Store::batch();
        prune_undo(&store, 199, 5, &mut prune).unwrap();
        store.commit_tip_only(prune, &tip).unwrap();

        // Old implementation only removed cutoff-64..cutoff and left 0..129.
        for h in 0..194 {
            assert!(
                store.get_undo(h).unwrap().is_none(),
                "height {h} should be pruned"
            );
        }
        for h in 194..200 {
            assert!(store.get_undo(h).unwrap().is_some(), "height {h} kept");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AUD-131: a group applied with deferred undo and committed with only
    /// the window's records must leave exactly the database the old path
    /// (write every record, prune at commit) produced, and must rewind.
    #[test]
    fn deferred_undo_matches_write_then_prune_and_rewinds() {
        failpoints::set(failpoints::NONE);
        const WINDOW: u32 = 10;
        let gen = ChainGen::new(ChainSpec {
            seed: 23,
            tx_per_block: 6,
            script_pool: 9,
        });
        let blocks: Vec<_> = gen.take(40).collect();

        // Old path: undo for every block, pruned to the window afterwards.
        let old_dir = tmp("undo-old");
        let old = Store::open(&old_dir).unwrap();
        apply_group(&old, &blocks, 0).unwrap();
        let tip = old.committed_tip().unwrap().unwrap();
        let mut prune = Store::batch();
        prune_undo(&old, tip.height, WINDOW, &mut prune).unwrap();
        old.commit_tip_only(prune, &tip).unwrap();

        // New path: one group, undo kept in memory, window written at commit.
        let dir = tmp("undo-deferred");
        let store = Store::open(&dir).unwrap();
        let mut overlay = GroupOverlay::new();
        let mut batch = WriteBatch::default();
        let mut txids = Vec::new();
        let mut undos = Vec::new();
        let mut tip = None;
        for (h, block) in blocks.iter().enumerate() {
            let raw = bitcoin::consensus::serialize(block);
            let prepared =
                prepare_block_bytes(&raw, &block.block_hash().to_string(), h as u32).unwrap();
            let out = apply_prepared_deferred_undo(
                &store,
                h as u32,
                &prepared,
                tip,
                &mut overlay,
                &mut batch,
                None,
            )
            .unwrap();
            txids.extend_from_slice(&out.txids);
            tip = Some(out.tip);
            undos.push(out.undo);
        }
        let tip = tip.unwrap();
        let written = write_window_undo(&store, &mut batch, &undos, tip.height, WINDOW).unwrap();
        assert_eq!(written, WINDOW as usize + 1);
        store.commit(batch, &tip, &txids).unwrap();

        assert_eq!(
            store.canonical_digest().unwrap(),
            old.canonical_digest().unwrap(),
            "deferred undo must produce the same database"
        );
        for h in 0..tip.height - WINDOW {
            assert!(store.get_undo(h).unwrap().is_none(), "no undo at {h}");
        }

        // The window is enough to rewind to its bottom and match the oracle.
        let target = tip.height - WINDOW;
        rewind_to(&store, target).unwrap();
        let mut oracle = Oracle::new();
        for (h, b) in blocks.iter().enumerate().take(target as usize + 1) {
            oracle.apply(h as u32, b);
        }
        oracle.assert_matches(&store).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&old_dir);
    }

    #[test]
    fn restart_midway_equals_continuous() {
        failpoints::set(failpoints::NONE);
        let dir = tmp("restart");
        let gen = ChainGen::new(ChainSpec {
            seed: 5,
            tx_per_block: 5,
            script_pool: 7,
        });
        let blocks: Vec<_> = gen.take(20).collect();
        let digest_continuous = {
            let store = Store::open(&dir).unwrap();
            apply_group(&store, &blocks, 0).unwrap();
            store.canonical_digest().unwrap()
        };
        // Same chain in two runs, reopening between them.
        let dir2 = tmp("restart2");
        {
            let store = Store::open(&dir2).unwrap();
            apply_group(&store, &blocks[..10], 0).unwrap();
        }
        let digest_split = {
            let store = Store::open(&dir2).unwrap();
            apply_group(&store, &blocks[10..], 10).unwrap();
            store.canonical_digest().unwrap()
        };
        assert_eq!(
            digest_continuous,
            digest_split,
            "continuous={:?} split={:?}",
            {
                let s = Store::open(&dir).unwrap();
                s.family_digests().unwrap()
            },
            {
                let s = Store::open(&dir2).unwrap();
                s.family_digests().unwrap()
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }
}
