//! Reconstructible mempool overlay.
//!
//! Core remains the source of truth. We poll `getrawmempool` (verbose) and fetch
//! the raw transactions of new entries in batches, then build:
//! - per-script history and balance deltas,
//! - mempool UTXOs and spent outpoints (including chained transactions),
//! - a fee histogram for Electrum.
//!
//! Nothing is persisted: the overlay is rebuilt from Core after a restart.

use el_model::{Hash32, Outpoint, ScriptHash};
use el_rpc::CoreClient;
use el_store::Store;
use std::collections::{HashMap, HashSet};
use std::fmt;

#[derive(Debug)]
pub struct MempoolError(pub String);

impl fmt::Display for MempoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mempool: {}", self.0)
    }
}

impl std::error::Error for MempoolError {}

impl From<el_rpc::CoreError> for MempoolError {
    fn from(e: el_rpc::CoreError) -> Self {
        MempoolError(e.to_string())
    }
}

impl From<el_store::StoreError> for MempoolError {
    fn from(e: el_store::StoreError) -> Self {
        MempoolError(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, MempoolError>;

#[derive(Clone)]
pub struct MempoolTx {
    pub txid: Hash32,
    pub fee_sat: u64,
    pub vsize: u64,
    /// Per-script (received_sat, spent_sat) for this transaction.
    pub effects: HashMap<ScriptHash, (u64, u64)>,
    pub outputs: Vec<(Outpoint, ScriptHash, u64)>,
    /// Electrum height: 0 when all inputs are confirmed, -1 when the tx
    /// depends on another unconfirmed transaction.
    pub height: i32,
}

#[derive(Default)]
pub struct Mempool {
    txs: HashMap<Hash32, MempoolTx>,
    /// Raw transactions retained between polls so new entries are incremental.
    raw: HashMap<Hash32, bitcoin::Transaction>,
    by_script: HashMap<ScriptHash, Vec<Hash32>>,
    spending: HashSet<Outpoint>,
    histogram: Vec<(f64, u64)>,
    version: u64,
    unresolved_inputs: u64,
}

impl Mempool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn len(&self) -> usize {
        self.txs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.txs.is_empty()
    }

    pub fn fee_histogram(&self) -> Vec<(f64, u64)> {
        self.histogram.clone()
    }

    pub fn unresolved_inputs(&self) -> u64 {
        self.unresolved_inputs
    }

    /// Confirmed UTXOs spent by a mempool transaction must not be listed as
    /// unspent, and their value is part of the unconfirmed delta.
    pub fn is_spent(&self, op: &Outpoint) -> bool {
        self.spending.contains(op)
    }

    /// Unconfirmed transactions touching `script`: (txid, fee, electrum height).
    pub fn history(&self, script: ScriptHash) -> Vec<(Hash32, u64, i32)> {
        self.by_script
            .get(&script)
            .map(|v| {
                v.iter()
                    .filter_map(|txid| {
                        self.txs
                            .get(txid)
                            .map(|tx| (tx.txid, tx.fee_sat, tx.height))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Net unconfirmed balance change for `script` (received - spent).
    pub fn balance_delta(&self, script: ScriptHash) -> i64 {
        let mut delta = 0i64;
        for tx in self.txs.values() {
            if let Some((recv, spent)) = tx.effects.get(&script) {
                delta += *recv as i64 - *spent as i64;
            }
        }
        delta
    }

    /// Mempool outputs for `script` that are not spent by another mempool tx.
    pub fn utxos(&self, script: ScriptHash) -> Vec<(Outpoint, u64)> {
        let mut out = Vec::new();
        for tx in self.txs.values() {
            for (op, sh, value) in &tx.outputs {
                if *sh == script && !self.spending.contains(op) {
                    out.push((*op, *value));
                }
            }
        }
        out.sort_by_key(|(op, _)| *op);
        out
    }

    /// Poll Core and rebuild the overlay if the mempool changed.
    pub fn refresh(&mut self, core: &CoreClient, store: &Store) -> Result<bool> {
        let entries = core.raw_mempool_verbose()?;
        let mut wanted: HashMap<Hash32, (u64, u64)> = HashMap::new();
        for (txid_hex, entry) in &entries {
            let Ok(txid) = Hash32::from_hex(txid_hex) else {
                continue;
            };
            let fee_sat = entry
                .fees
                .as_ref()
                .map(|f| (f.base * 100_000_000.0).round() as u64)
                .unwrap_or(0);
            wanted.insert(txid, (entry.vsize, fee_sat));
        }

        let wanted_set: HashSet<Hash32> = wanted.keys().copied().collect();
        let current: HashSet<Hash32> = self.txs.keys().copied().collect();
        if current == wanted_set {
            return Ok(false);
        }

        // Drop vanished transactions, fetch only the new ones (in batches).
        self.raw.retain(|txid, _| wanted_set.contains(txid));
        let missing: Vec<String> = wanted
            .keys()
            .filter(|t| !self.raw.contains_key(*t))
            .map(|t| t.to_hex())
            .collect();
        for chunk in missing.chunks(100) {
            let hexes = core.batch_raw_transactions(chunk)?;
            for (txid_hex, maybe_hex) in chunk.iter().zip(hexes) {
                let Some(hex) = maybe_hex else { continue };
                let Ok(bytes) = hex::decode(&hex) else {
                    continue;
                };
                let Ok(tx) = bitcoin::consensus::deserialize::<bitcoin::Transaction>(&bytes) else {
                    continue;
                };
                if let Ok(txid) = Hash32::from_hex(txid_hex) {
                    self.raw.insert(txid, tx);
                }
            }
        }

        // All mempool outputs, so chained transactions resolve regardless of order.
        let mut outputs: HashMap<Outpoint, (ScriptHash, u64)> = HashMap::new();
        for (txid, tx) in &self.raw {
            for (vout, out) in tx.output.iter().enumerate() {
                outputs.insert(
                    Outpoint {
                        txid: *txid,
                        vout: vout as u32,
                    },
                    (
                        ScriptHash::from_script(out.script_pubkey.as_bytes()),
                        out.value.to_sat(),
                    ),
                );
            }
        }

        let mut txs: HashMap<Hash32, MempoolTx> = HashMap::new();
        let mut spending = HashSet::new();
        let mut unresolved = 0u64;
        for (txid, tx) in &self.raw {
            let (vsize, fee_sat) = wanted.get(txid).copied().unwrap_or((0, 0));
            let mut effects: HashMap<ScriptHash, (u64, u64)> = HashMap::new();
            let mut all_inputs_confirmed = true;
            for input in &tx.input {
                if input.previous_output.is_null() {
                    continue;
                }
                let op = Outpoint {
                    txid: Hash32::from_hex(&input.previous_output.txid.to_string())
                        .unwrap_or(Hash32::ZERO),
                    vout: input.previous_output.vout,
                };
                let from_mempool = outputs.get(&op).copied();
                let prev = from_mempool.or_else(|| {
                    store
                        .get_utxo(&op)
                        .ok()
                        .flatten()
                        .map(|u| (u.script_hash, u.value_sat))
                });
                match prev {
                    Some((script, value)) => {
                        if from_mempool.is_some() {
                            all_inputs_confirmed = false;
                        }
                        let e = effects.entry(script).or_default();
                        e.1 = e.1.saturating_add(value);
                        spending.insert(op);
                    }
                    None => {
                        all_inputs_confirmed = false;
                        unresolved += 1;
                    }
                }
            }
            let mut tx_outputs = Vec::new();
            for (vout, out) in tx.output.iter().enumerate() {
                let script = ScriptHash::from_script(out.script_pubkey.as_bytes());
                let value = out.value.to_sat();
                let op = Outpoint {
                    txid: *txid,
                    vout: vout as u32,
                };
                let e = effects.entry(script).or_default();
                e.0 = e.0.saturating_add(value);
                tx_outputs.push((op, script, value));
            }
            txs.insert(
                *txid,
                MempoolTx {
                    txid: *txid,
                    fee_sat,
                    vsize,
                    effects,
                    outputs: tx_outputs,
                    height: if all_inputs_confirmed { 0 } else { -1 },
                },
            );
        }

        let mut by_script: HashMap<ScriptHash, Vec<Hash32>> = HashMap::new();
        for tx in txs.values() {
            for script in tx.effects.keys() {
                by_script.entry(*script).or_default().push(tx.txid);
            }
        }
        for v in by_script.values_mut() {
            v.sort();
            v.dedup();
        }

        self.txs = txs;
        self.by_script = by_script;
        self.spending = spending;
        self.unresolved_inputs = unresolved;
        self.histogram = build_histogram(&self.txs);
        self.version += 1;
        Ok(true)
    }
}

fn build_histogram(txs: &HashMap<Hash32, MempoolTx>) -> Vec<(f64, u64)> {
    let mut buckets: HashMap<u64, u64> = HashMap::new();
    for tx in txs.values() {
        if tx.vsize == 0 {
            continue;
        }
        let rate = tx.fee_sat as f64 / tx.vsize as f64;
        // 0.1 sat/vB granularity, capped at 10_000 sat/vB.
        let key = ((rate * 10.0).round() as u64).min(100_000);
        *buckets.entry(key).or_insert(0) += tx.vsize;
    }
    let mut out: Vec<(f64, u64)> = buckets
        .into_iter()
        .map(|(k, v)| (k as f64 / 10.0, v))
        .collect();
    out.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(200);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::absolute::LockTime;
    use bitcoin::transaction::Version;
    use bitcoin::{
        Amount, OutPoint as BOutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
    };
    use el_model::{IndexedTip, UtxoValue};
    use el_store::Store;
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "e-lite-mempool-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn make_tx(spends: &[(String, u32)], outputs: &[(u64, u8)]) -> Transaction {
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: spends
                .iter()
                .map(|(txid, vout)| TxIn {
                    previous_output: BOutPoint {
                        txid: txid.parse().unwrap(),
                        vout: *vout,
                    },
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness: Witness::new(),
                })
                .collect(),
            output: outputs
                .iter()
                .map(|(value, byte)| TxOut {
                    value: Amount::from_sat(*value),
                    script_pubkey: ScriptBuf::from_bytes(vec![*byte]),
                })
                .collect(),
        }
    }

    #[test]
    fn indexes_chained_mempool_transactions() {
        let dir = tmp("chain");
        let store = Store::open(&dir).unwrap();
        let script_a = ScriptHash::from_script(&[0xaa]);
        let script_b = ScriptHash::from_script(&[0xbb]);
        let confirmed = Outpoint {
            txid: Hash32([0x11; 32]),
            vout: 0,
        };
        let mut batch = Store::batch();
        store
            .put_utxo(
                &mut batch,
                &confirmed,
                &UtxoValue {
                    value_sat: 1000,
                    script_hash: script_a,
                    creation_height: 1,
                    coinbase: false,
                },
            )
            .unwrap();
        store
            .commit_tip_only(
                batch,
                &IndexedTip {
                    height: 1,
                    block_hash: Hash32([0x22; 32]),
                    schema_version: el_model::codec::SCHEMA_VERSION,
                    txnum: 0,
                },
            )
            .unwrap();

        let parent = make_tx(&[("11".repeat(32), 0)], &[(900, 0xbb)]);
        let parent_id = Hash32::from_hex(&parent.compute_txid().to_string()).unwrap();
        let parent_hex = parent_id.to_hex();
        let child = make_tx(&[(parent_hex, 0)], &[(800, 0xaa)]);
        let child_id = Hash32::from_hex(&child.compute_txid().to_string()).unwrap();

        // Build the overlay through the same code path as refresh, injecting raw
        // transactions instead of calling Core.
        let mut m = Mempool::new();
        m.raw = HashMap::from([(parent_id, parent), (child_id, child)]);
        let mut wanted = HashMap::new();
        wanted.insert(parent_id, (200u64, 100u64));
        wanted.insert(child_id, (200, 100));

        let mut outputs: HashMap<Outpoint, (ScriptHash, u64)> = HashMap::new();
        for (txid, tx) in &m.raw {
            for (vout, out) in tx.output.iter().enumerate() {
                outputs.insert(
                    Outpoint {
                        txid: *txid,
                        vout: vout as u32,
                    },
                    (
                        ScriptHash::from_script(out.script_pubkey.as_bytes()),
                        out.value.to_sat(),
                    ),
                );
            }
        }
        let mut txs = HashMap::new();
        let mut spending = HashSet::new();
        for (txid, tx) in &m.raw {
            let (vsize, fee_sat) = wanted[txid];
            let mut effects: HashMap<ScriptHash, (u64, u64)> = HashMap::new();
            let mut confirmed_parents = true;
            for input in &tx.input {
                let op = Outpoint {
                    txid: Hash32::from_hex(&input.previous_output.txid.to_string()).unwrap(),
                    vout: input.previous_output.vout,
                };
                if outputs.contains_key(&op) {
                    confirmed_parents = false;
                }
                let (script, value) = outputs
                    .get(&op)
                    .copied()
                    .or_else(|| {
                        store
                            .get_utxo(&op)
                            .ok()
                            .flatten()
                            .map(|u| (u.script_hash, u.value_sat))
                    })
                    .unwrap();
                effects.entry(script).or_default().1 += value;
                spending.insert(op);
            }
            let mut outs = Vec::new();
            for (vout, out) in tx.output.iter().enumerate() {
                let script = ScriptHash::from_script(out.script_pubkey.as_bytes());
                let op = Outpoint {
                    txid: *txid,
                    vout: vout as u32,
                };
                effects.entry(script).or_default().0 += out.value.to_sat();
                outs.push((op, script, out.value.to_sat()));
            }
            txs.insert(
                *txid,
                MempoolTx {
                    txid: *txid,
                    fee_sat,
                    vsize,
                    effects,
                    outputs: outs,
                    height: if confirmed_parents { 0 } else { -1 },
                },
            );
        }
        m.txs = txs;
        m.spending = spending;
        m.histogram = build_histogram(&m.txs);
        let mut by_script: HashMap<ScriptHash, Vec<Hash32>> = HashMap::new();
        for tx in m.txs.values() {
            for s in tx.effects.keys() {
                by_script.entry(*s).or_default().push(tx.txid);
            }
        }
        m.by_script = by_script;

        assert_eq!(m.len(), 2);
        assert!(m.is_spent(&confirmed));
        assert_eq!(m.balance_delta(script_a), -200);
        assert_eq!(m.balance_delta(script_b), 0);
        assert_eq!(m.utxos(script_a).len(), 1);
        assert_eq!(m.utxos(script_b).len(), 0);
        let hist = m.history(script_a);
        assert_eq!(hist.len(), 2);
        // The child spends an unconfirmed parent, so its Electrum height is -1.
        assert!(hist.iter().any(|(_, _, h)| *h == -1));
        assert!(hist.iter().any(|(_, _, h)| *h == 0));
        assert!(!m.fee_histogram().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
