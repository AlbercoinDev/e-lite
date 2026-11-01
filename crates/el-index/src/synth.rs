//! Deterministic synthetic chain generator and a naive, independent oracle.
//!
//! The oracle deliberately re-implements balances/UTXO/history with plain HashMaps
//! so it shares no code with the indexer.

use bitcoin::absolute::LockTime;
use bitcoin::block::Header;
use bitcoin::hashes::Hash;
use bitcoin::transaction::Version;
use bitcoin::{
    Amount, Block, BlockHash, CompactTarget, OutPoint, ScriptBuf, Sequence, Transaction, TxIn,
    TxMerkleNode, TxOut, Witness,
};
use el_model::{Hash32, ScriptHash};
use el_store::Store;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
pub struct ChainSpec {
    pub seed: u64,
    pub tx_per_block: u32,
    pub script_pool: usize,
}

#[derive(Clone)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn make_script(idx: usize) -> ScriptBuf {
    let mut seed = [0u8; 32];
    for (i, b) in seed.iter_mut().enumerate() {
        *b = (idx as u8).wrapping_mul(31).wrapping_add(i as u8 * 7);
    }
    let bytes = match idx % 5 {
        0 => {
            let mut s = vec![0x76, 0xa9, 0x14];
            s.extend_from_slice(&seed[..20]);
            s.extend_from_slice(&[0x88, 0xac]);
            s
        }
        1 => {
            let mut s = vec![0x00, 0x14];
            s.extend_from_slice(&seed[..20]);
            s
        }
        2 => {
            let mut s = vec![0xa9, 0x14];
            s.extend_from_slice(&seed[..20]);
            s.push(0x87);
            s
        }
        3 => {
            let mut s = vec![0x51, 0x20];
            s.extend_from_slice(&seed);
            s
        }
        _ => {
            let mut s = vec![0x6a, 0x04];
            s.extend_from_slice(&seed[..4]);
            s
        }
    };
    ScriptBuf::from_bytes(bytes)
}

// The pool also includes an empty script (valid and spendable) at the end.
fn scripts_for(pool: usize) -> Vec<ScriptBuf> {
    let mut v: Vec<ScriptBuf> = (0..pool).map(make_script).collect();
    if let Some(last) = v.last_mut() {
        *last = ScriptBuf::new();
    }
    v
}

pub fn seal(prev_blockhash: BlockHash, time: u32, nonce: u32, txdata: Vec<Transaction>) -> Block {
    let mut block = Block {
        header: Header {
            version: bitcoin::block::Version::from_consensus(1),
            prev_blockhash,
            merkle_root: TxMerkleNode::all_zeros(),
            time,
            bits: CompactTarget::from_consensus(0x207f_ffff),
            nonce,
        },
        txdata,
    };
    block.header.merkle_root = block.compute_merkle_root().expect("block has transactions");
    block
}

#[derive(Clone)]
pub struct ChainGen {
    spec: ChainSpec,
    rng: Rng,
    height: u32,
    prev: BlockHash,
    spendable: Vec<(OutPoint, u64)>,
    scripts: Vec<ScriptBuf>,
    nonce: u64,
}

impl ChainGen {
    pub fn new(spec: ChainSpec) -> Self {
        let rng = Rng::new(spec.seed);
        ChainGen {
            scripts: scripts_for(spec.script_pool.max(6)),
            spec,
            rng,
            height: 0,
            prev: BlockHash::all_zeros(),
            spendable: Vec::new(),
            nonce: 0,
        }
    }

    fn script(&mut self) -> ScriptBuf {
        let i = self.rng.below(self.scripts.len() as u64) as usize;
        self.scripts[i].clone()
    }

    fn coinbase(&mut self, height: u32) -> Transaction {
        self.nonce += 1;
        let mut sig = Vec::with_capacity(12);
        sig.extend_from_slice(&height.to_le_bytes());
        sig.extend_from_slice(&self.nonce.to_le_bytes());
        sig.extend_from_slice(&self.spec.seed.to_le_bytes());
        let value = 5_000_000u64;
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::from_bytes(sig),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(value),
                script_pubkey: self.script(),
            }],
        }
    }

    fn next_block(&mut self) -> Block {
        let height = self.height;
        let mut txs = Vec::with_capacity(self.spec.tx_per_block as usize + 1);
        let cb = self.coinbase(height);
        // Genesis coinbase is unspendable; OP_RETURN outputs are unspendable too.
        if height > 0 && cb.output[0].script_pubkey.as_bytes().first() != Some(&0x6a) {
            let cb_txid = cb.compute_txid();
            self.spendable.push((
                OutPoint {
                    txid: cb_txid,
                    vout: 0,
                },
                cb.output[0].value.to_sat(),
            ));
        }
        txs.push(cb);

        for i in 0..self.spec.tx_per_block {
            if self.spendable.is_empty() {
                break;
            }
            let n_in = if self.spendable.len() > 1 && self.rng.below(2) == 0 {
                2
            } else {
                1
            };
            let n_in = n_in.min(self.spendable.len()).max(1);
            let mut inputs = Vec::with_capacity(n_in);
            let mut in_sum = 0u64;
            for _ in 0..n_in {
                let (op, value) = self.spendable.remove(0);
                inputs.push(TxIn {
                    previous_output: op,
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness: Witness::new(),
                });
                in_sum += value;
            }
            if in_sum < 10 {
                continue;
            }
            self.nonce += 1;
            // Unique nonce in the first scriptSig keeps TXIDs distinct.
            inputs[0].script_sig = ScriptBuf::from_bytes({
                let mut v = Vec::from(self.nonce.to_le_bytes());
                v.extend_from_slice(&height.to_le_bytes());
                v.push(i as u8);
                v
            });
            let n_out = 1 + self.rng.below(3) as usize;
            let fee = 200u64.min(in_sum / 4);
            let distributable = in_sum - fee;
            let mut outputs = Vec::with_capacity(n_out);
            let mut left = distributable;
            for k in 0..n_out {
                let value = if k + 1 == n_out {
                    left
                } else {
                    let max = left / (n_out - k) as u64;
                    1 + if max > 1 { self.rng.below(max) } else { 0 }
                };
                left -= value;
                let script = self.script();
                outputs.push(TxOut {
                    value: Amount::from_sat(value),
                    script_pubkey: script.clone(),
                });
            }
            let tx = Transaction {
                version: Version::TWO,
                lock_time: LockTime::ZERO,
                input: inputs,
                output: outputs,
            };
            let txid = tx.compute_txid();
            for (vout, out) in tx.output.iter().enumerate() {
                let unspendable = out.script_pubkey.as_bytes().first() == Some(&0x6a);
                if !unspendable && out.value.to_sat() > 0 {
                    self.spendable.push((
                        OutPoint {
                            txid,
                            vout: vout as u32,
                        },
                        out.value.to_sat(),
                    ));
                }
            }
            txs.push(tx);
        }

        let time = 1_700_000_000 + height;
        let block = seal(self.prev, time, height, txs);
        self.prev = block.block_hash();
        self.height += 1;
        block
    }
}

impl Iterator for ChainGen {
    type Item = Block;
    fn next(&mut self) -> Option<Self::Item> {
        Some(self.next_block())
    }
}

// ---------------------------------------------------------------------- oracle

#[derive(Default)]
pub struct Oracle {
    /// "txid:vout" -> (value, script_hex, height, coinbase)
    utxos: HashMap<String, (u64, String, u32, bool)>,
    /// script_hex -> [(height, pos, received, spent)]
    history: HashMap<String, Vec<(u32, u32, u64, u64)>>,
    /// txid -> [(height, pos)]
    occurrences: HashMap<String, Vec<(u32, u32)>>,
    txnum: u64,
    tip: Option<(u32, String)>,
}

impl Oracle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, height: u32, block: &Block) {
        let mut local: HashMap<String, (u64, u64)> = HashMap::new();
        for (pos, tx) in block.txdata.iter().enumerate() {
            let txid = tx.compute_txid().to_string();
            let coinbase = pos == 0 && tx.is_coinbase();
            let genesis = height == 0 && coinbase;

            for input in &tx.input {
                if input.previous_output.is_null() {
                    continue;
                }
                let key = format!(
                    "{}:{}",
                    input.previous_output.txid, input.previous_output.vout
                );
                let (value, script_hex, _, _) = self.utxos.remove(&key).unwrap_or_else(|| {
                    let tid = input.previous_output.txid.to_string();
                    let near: Vec<_> = self
                        .utxos
                        .keys()
                        .filter(|k| k.starts_with(&tid))
                        .take(5)
                        .collect();
                    panic!(
                        "oracle: missing prevout {key} at height {height}; same-txid keys: {near:?}"
                    )
                });
                let e = local.entry(script_hex).or_default();
                e.1 += value;
            }

            for (vout, out) in tx.output.iter().enumerate() {
                let script_hex = ScriptHash::from_script(out.script_pubkey.as_bytes())
                    .0
                    .to_hex();
                let e = local.entry(script_hex.clone()).or_default();
                e.0 += out.value.to_sat();
                if genesis || out.script_pubkey.as_bytes().first() == Some(&0x6a) {
                    continue;
                }
                self.utxos.insert(
                    format!("{txid}:{vout}"),
                    (out.value.to_sat(), script_hex, height, coinbase),
                );
            }

            for (script_hex, (received, spent)) in local.drain() {
                self.history
                    .entry(script_hex)
                    .or_default()
                    .push((height, pos as u32, received, spent));
            }
            self.occurrences
                .entry(txid)
                .or_default()
                .push((height, pos as u32));
            self.txnum += 1;
        }
        self.tip = Some((height, block.block_hash().to_string()));
    }

    pub fn txnum(&self) -> u64 {
        self.txnum
    }

    pub fn tip(&self) -> Option<(u32, String)> {
        self.tip.clone()
    }

    pub fn assert_matches(&self, store: &Store) -> Result<(), String> {
        let tip = store
            .committed_tip()
            .map_err(|e| e.to_string())?
            .ok_or("store has no tip")?;
        let (height, hash) = self.tip.clone().ok_or("oracle has no tip")?;
        if tip.height != height || tip.block_hash.to_hex() != hash {
            return Err(format!(
                "tip mismatch: store {} {}/oracle {height} {hash}",
                tip.height, tip.block_hash
            ));
        }
        if tip.txnum != self.txnum {
            return Err(format!(
                "txnum mismatch: store {} oracle {}",
                tip.txnum, self.txnum
            ));
        }

        for (script_hex, expected) in &self.history {
            let sh = ScriptHash(Hash32::from_hex(script_hex)?);
            let mut got = store
                .history_for_script(sh)
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|(h, p, e)| (h, p, e.received_sat, e.spent_sat))
                .collect::<Vec<_>>();
            got.sort();
            let mut want = expected.clone();
            want.sort();
            if got != want {
                return Err(format!(
                    "history mismatch for {script_hex}: got {} entries, want {}",
                    got.len(),
                    want.len()
                ));
            }
        }

        let mut scripts: HashSet<&String> = self.history.keys().collect();
        scripts.extend(self.utxos.values().map(|(_, s, _, _)| s));
        for script_hex in scripts {
            let sh = ScriptHash(Hash32::from_hex(script_hex)?);
            let mut got = store
                .utxos_for_script(sh)
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|(op, v, h, cb)| (format!("{}:{}", op.txid, op.vout), v, h, cb))
                .collect::<Vec<_>>();
            got.sort();
            let mut want: Vec<(String, u64, u32, bool)> = self
                .utxos
                .iter()
                .filter(|(_, (_, s, _, _))| s == script_hex)
                .map(|(k, (v, _, h, cb))| (k.clone(), *v, *h, *cb))
                .collect();
            want.sort();
            if got != want {
                return Err(format!(
                    "utxo mismatch for {script_hex}: got {} want {}",
                    got.len(),
                    want.len()
                ));
            }
        }

        // Sample occurrence lookups (all duplicates plus a deterministic slice).
        for (i, (txid, occ)) in self.occurrences.iter().enumerate() {
            if i % 97 != 0 && occ.len() < 2 {
                continue;
            }
            let h = Hash32::from_hex(txid)?;
            let got = store.occurrences(h).map_err(|e| e.to_string())?;
            if got != *occ {
                return Err(format!("occurrence mismatch for {txid}"));
            }
        }
        Ok(())
    }
}
