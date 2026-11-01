//! Block application, undo records and reorg rewind.

pub mod synth;

use el_model::codec::SCHEMA_VERSION;
use el_model::{
    BlockRecord, Hash32, HistoryEffect, IndexedTip, Outpoint, ScriptHash, UndoCreated, UndoHistory,
    UndoRecord, UndoSpent, UtxoValue, HEADER_LEN,
};
use el_store::{Store, StoreError};
use rocksdb::WriteBatch;
use std::collections::{HashMap, HashSet};
use std::fmt;

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
}

fn is_provably_unspendable(script: &[u8]) -> bool {
    script.first() == Some(&0x6a) // OP_RETURN
}

/// Apply one block into `batch`, updating the overlay. The caller commits the
/// batch (plus the tip) atomically and then appends `txids` to the flat file.
pub fn apply_block(
    store: &Store,
    height: u32,
    block: &bitcoin::Block,
    prev_tip: Option<IndexedTip>,
    overlay: &mut GroupOverlay,
    batch: &mut WriteBatch,
) -> Result<ApplyOutcome> {
    // --- continuity -------------------------------------------------------
    let block_hash = hash_from_blockhash(&block.block_hash())?;
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

    // --- merkle -----------------------------------------------------------
    match block.compute_merkle_root() {
        Some(root) if root == block.header.merkle_root => {}
        _ => return Err(IndexError::MerkleMismatch { height }),
    }

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

    let mut undo = UndoRecord {
        height,
        block_hash,
        ..Default::default()
    };
    let mut txids: Vec<Hash32> = Vec::with_capacity(block.txdata.len());
    // script -> (received, spent, flags)
    let mut effects: HashMap<ScriptHash, HistoryEffect> = HashMap::new();
    // outputs created inside this very block (for undo de-duplication)
    let mut created_here: HashMap<Outpoint, ScriptHash> = HashMap::new();

    for (pos, tx) in block.txdata.iter().enumerate() {
        let txid = hash_from_txid(&tx.compute_txid())?;
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
            let utxo = overlay
                .get(store, &op)?
                .ok_or_else(|| IndexError::PrevoutMissing {
                    txid: op.txid.to_hex(),
                    vout: op.vout,
                    height,
                })?;
            // Remove from the live view.
            let was_created_here = created_here.remove(&op).is_some();
            overlay.remove(&op);
            store.delete_utxo(batch, &op, utxo.script_hash)?;
            let e = effects.entry(utxo.script_hash).or_default();
            e.spent_sat = e.spent_sat.saturating_add(utxo.value_sat);
            if was_created_here {
                // Created and spent inside this same block: after undo it simply
                // does not exist, so drop it from the created list too.
                undo.created.retain(|c| c.outpoint != op);
            } else {
                undo.spent.push(UndoSpent { outpoint: op, utxo });
            }
        }

        // outputs
        for (vout, out) in tx.output.iter().enumerate() {
            let script = out.script_pubkey.as_bytes();
            let script_hash = ScriptHash::from_script(script);
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
            created_here.insert(op, script_hash);
            undo.created.push(UndoCreated {
                outpoint: op,
                script_hash,
            });
        }

        let mut drained: Vec<(ScriptHash, HistoryEffect)> = effects.drain().collect();
        drained.sort_by_key(|e| e.0);
        for (script_hash, effect) in drained {
            store.put_history(batch, script_hash, height, tx_pos, &effect)?;
            undo.history.push(UndoHistory {
                script_hash,
                height,
                tx_pos,
            });
        }

        store.put_occurrence(batch, txid, height, tx_pos)?;
        undo.txids.push(txid);
        txids.push(txid);
    }

    let rec = BlockRecord {
        height,
        hash: block_hash,
        header: header_bytes,
        tx_count: txids.len() as u32,
        txnum_base,
    };
    store.put_block(batch, &rec)?;
    store.put_undo(batch, &undo)?;

    let tip = IndexedTip {
        height,
        block_hash,
        schema_version: SCHEMA_VERSION,
        txnum: txnum_base + txids.len() as u64,
    };

    Ok(ApplyOutcome {
        undo,
        txids,
        txnum_base,
        tip,
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
    let start = cutoff.saturating_sub(64);
    // Old undo records are outside the reorg window by definition, so deleting
    // them is idempotent and does not require decoding them. Parsing here used
    // to turn a codec bug into a hard indexing stop.
    for h in start..cutoff {
        store.delete_undo(batch, h)?;
    }
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
            let out = apply_block(store, height, block, tip, &mut overlay, &mut batch)?;
            all_txids.extend_from_slice(&out.txids);
            tip = Some(out.tip);
        }
        store.commit(batch, &tip.unwrap(), &all_txids)?;
        Ok(())
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
        );
        assert!(
            matches!(res, Err(IndexError::PrevoutMissing { .. })),
            "got {res:?}"
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
