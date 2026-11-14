//! Decoy wallets built from the local index (AUD-163).
//!
//! A wallet's transactions form a connected group in the transaction graph,
//! so random decoy txids do not hide it. Instead, for every real address that
//! needs external transactions, e-lite picks `k` real addresses from its own
//! index with a history of similar size and age, and loads their transactions
//! alongside the real ones. Each decoy wallet then grows through the outputs
//! of its own transactions (local lookups), so it is connected like a real
//! wallet. A server sees `k + 1` plausible wallets loading at once.
//!
//! The choice is keyed by a local secret (`<data-dir>/decoy.key`): reloading
//! the same wallet yields the same decoys. Fresh decoys on every load would let
//! a server intersect two loads and keep only the real group.

use crate::privacy::PrivacyParams;
use crate::{RawtxError, Result};
use el_model::{Hash32, ScriptHash};
use el_store::Store;
use sha2::{Digest, Sha256};
use std::collections::{HashSet, VecDeque};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

pub const DECOY_KEY_FILE: &str = "decoy.key";

/// HMAC-SHA256 (RFC 2104) without an extra dependency.
pub(crate) fn hmac(key: &[u8; 32], data: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    k[..32].copy_from_slice(key);
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let inner = Sha256::new()
        .chain_update(ipad)
        .chain_update(data)
        .finalize();
    Sha256::new()
        .chain_update(opad)
        .chain_update(inner)
        .finalize()
        .into()
}

/// Read `<dir>/decoy.key`, creating it (0600, 32 random bytes) when missing.
pub fn load_or_create_secret(dir: &Path) -> Result<[u8; 32]> {
    let path = dir.join(DECOY_KEY_FILE);
    if let Ok(bytes) = std::fs::read(&path) {
        if bytes.len() == 32 {
            let mut key = [0u8; 32];
            key.copy_from_slice(&bytes);
            return Ok(key);
        }
    }
    let key: [u8; 32] = rand::random();
    let tmp = dir.join(format!(".{DECOY_KEY_FILE}.tmp"));
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| RawtxError::Io(format!("decoy key: {e}")))?;
        f.write_all(&key)
            .map_err(|e| RawtxError::Io(format!("decoy key: {e}")))?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| RawtxError::Io(format!("decoy key: {e}")))?;
    Ok(key)
}

/// One transaction of a history: txid plus its block position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TxRef {
    pub txid: Hash32,
    pub height: u32,
    pub pos: u32,
}

/// Picks decoy scripts from the index.
pub(crate) struct DecoyPicker {
    store: Arc<Store>,
    secret: [u8; 32],
    params: PrivacyParams,
}

impl DecoyPicker {
    pub(crate) fn new(store: Arc<Store>, secret: [u8; 32], params: PrivacyParams) -> Self {
        DecoyPicker {
            store,
            secret,
            params,
        }
    }

    /// The transactions of `script` from the index.
    pub(crate) fn history(&self, script: ScriptHash) -> Vec<TxRef> {
        let Ok(entries) = self.store.history_for_script(script) else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(entries.len());
        let mut last: Option<(u32, el_model::BlockRecord)> = None;
        for (height, pos, _) in entries {
            let rec = match &last {
                Some((h, r)) if *h == height => *r,
                _ => match self.store.get_block(height) {
                    Ok(Some(r)) => {
                        last = Some((height, r));
                        r
                    }
                    _ => continue,
                },
            };
            if let Ok(txid) = self.store.txid_at(rec.txnum_base + pos as u64) {
                out.push(TxRef { txid, height, pos });
            }
        }
        out.dedup_by_key(|t| t.txid);
        out
    }

    /// The decoy script number `slot` for `real` (stable for one secret), with
    /// a history of similar size and era, not in `exclude`.
    pub(crate) fn pick(
        &self,
        seed: &[u8],
        slot: u32,
        size: usize,
        era: (u32, u32),
        exclude: &HashSet<ScriptHash>,
    ) -> Option<(ScriptHash, Vec<TxRef>)> {
        let p = &self.params;
        let lo = ((size as f64) * p.decoy_size_min_ratio).floor().max(1.0) as usize;
        let hi = ((size as f64) * p.decoy_size_max_ratio).ceil().max(1.0) as usize;
        let era_lo = era.0.saturating_sub(p.decoy_era_margin_blocks);
        let era_hi = era.1.saturating_add(p.decoy_era_margin_blocks);
        let mut best: Option<(usize, ScriptHash, Vec<TxRef>)> = None;
        for attempt in 0..p.decoy_seek_attempts {
            let mut data = Vec::with_capacity(seed.len() + 8);
            data.extend_from_slice(seed);
            data.extend_from_slice(&slot.to_le_bytes());
            data.extend_from_slice(&attempt.to_le_bytes());
            let key = hmac(&self.secret, &data);
            let Ok(Some(script)) = self.store.script_at_or_after(&key) else {
                continue;
            };
            if exclude.contains(&script) {
                continue;
            }
            let hist = self.history(script);
            if hist.is_empty() {
                continue;
            }
            let n = hist.len();
            let in_era = hist
                .iter()
                .any(|t| t.height >= era_lo && t.height <= era_hi);
            if (lo..=hi).contains(&n) && in_era {
                return Some((script, hist));
            }
            // Keep the closest candidate in case nothing fits the limits.
            let distance = if in_era { 0 } else { 1_000_000 } + n.abs_diff(size);
            if best.as_ref().is_none_or(|(d, _, _)| distance < *d) {
                best = Some((distance, script, hist));
            }
        }
        best.map(|(_, s, h)| (s, h))
    }
}

/// The look-alike of one real address in one decoy wallet. Everything in it
/// is a function of the secret, the real address and chain data, never of
/// timing: two loads of the same wallet send the same decoys (AUD-163).
struct Pick {
    /// The decoy address's own transactions, in history order.
    base: Vec<Hash32>,
    /// Every transaction of this pick (base, then grown ones).
    txs: Vec<Hash32>,
    seen: HashSet<Hash32>,
    scripts: HashSet<ScriptHash>,
    /// Transactions to send: as many as the real address has.
    target: usize,
    /// Raw base transactions received, and the next one to grow from.
    arrived: std::collections::HashMap<Hash32, Vec<u8>>,
    cursor: usize,
}

/// The decoy wallets of one wallet connection.
pub(crate) struct DecoyBook {
    k: usize,
    picks: Vec<(usize, Pick)>,
    /// txid -> pick index, for growth.
    owner: std::collections::HashMap<Hash32, usize>,
    pending: VecDeque<(usize, Hash32)>,
    real_scripts: HashSet<ScriptHash>,
    unknown_slots: u32,
}

impl DecoyBook {
    pub(crate) fn new(k: u32) -> Self {
        DecoyBook {
            k: k as usize,
            picks: Vec::new(),
            owner: std::collections::HashMap::new(),
            pending: VecDeque::new(),
            real_scripts: HashSet::new(),
            unknown_slots: 0,
        }
    }

    fn era_of(hist: &[TxRef]) -> (u32, u32) {
        let lo = hist.iter().map(|t| t.height).min().unwrap_or(0);
        let hi = hist.iter().map(|t| t.height).max().unwrap_or(u32::MAX);
        (lo, hi)
    }

    fn add_pick(&mut self, wallet: usize, script: ScriptHash, hist: Vec<TxRef>, target: usize) {
        let idx = self.picks.len();
        let mut pick = Pick {
            base: hist.iter().map(|t| t.txid).collect(),
            txs: Vec::new(),
            seen: HashSet::new(),
            scripts: HashSet::from([script]),
            target: target.max(1),
            arrived: std::collections::HashMap::new(),
            cursor: 0,
        };
        for t in &hist {
            if pick.txs.len() >= pick.target {
                break;
            }
            if pick.seen.insert(t.txid) {
                pick.txs.push(t.txid);
                self.owner.insert(t.txid, idx);
                self.pending.push_back((wallet, t.txid));
            }
        }
        self.picks.push((wallet, pick));
    }

    /// A real address with history: give each decoy wallet a look-alike of
    /// it (similar size and era), keyed by the secret and the address only.
    pub(crate) fn add_real(&mut self, picker: &DecoyPicker, script: ScriptHash, hist: &[TxRef]) {
        if hist.is_empty() || !self.real_scripts.insert(script) {
            return;
        }
        let era = Self::era_of(hist);
        let exclude = HashSet::from([script]);
        for slot in 0..self.k {
            if let Some((decoy, dh)) =
                picker.pick(&script.0 .0, slot as u32, hist.len(), era, &exclude)
            {
                self.add_pick(slot, decoy, dh, hist.len());
            }
        }
    }

    /// A real transaction asked for without its address (a bare
    /// `transaction.get`): a one-transaction look-alike per decoy wallet.
    pub(crate) fn add_unknown(&mut self, picker: &DecoyPicker, t: &TxRef) {
        let n = self.unknown_slots;
        self.unknown_slots = self.unknown_slots.wrapping_add(1);
        let exclude = HashSet::new();
        for slot in 0..self.k {
            if let Some((decoy, dh)) = picker.pick(
                &t.txid.0,
                n * 16 + slot as u32,
                1,
                (t.height, t.height),
                &exclude,
            ) {
                self.add_pick(slot, decoy, dh, 1);
            }
        }
    }

    /// Every decoy transaction not sent yet, as `(wallet, txid)`: decoy
    /// wallets travel together with the real requests that created them.
    pub(crate) fn take(&mut self, _real: usize) -> Vec<(usize, Hash32)> {
        self.pending.drain(..).collect()
    }

    /// A decoy transaction arrived. Growth follows the pick's own history
    /// order (not arrival order) and adds the addresses its outputs pay to,
    /// from the local index, until the pick reaches its target size: the
    /// decoy wallet is connected like a real one and identical on reloads.
    pub(crate) fn grow(&mut self, picker: &DecoyPicker, _wallet: usize, txid: Hash32, raw: &[u8]) {
        let Some(&idx) = self.owner.get(&txid) else {
            return;
        };
        let wallet = self.picks[idx].0;
        let mut new_txs = Vec::new();
        {
            let pick = &mut self.picks[idx].1;
            if !pick.base.contains(&txid) {
                return;
            }
            pick.arrived.insert(txid, raw.to_vec());
            while pick.cursor < pick.base.len() && pick.txs.len() < pick.target {
                let Some(raw) = pick.arrived.get(&pick.base[pick.cursor]).cloned() else {
                    break;
                };
                pick.cursor += 1;
                let Ok(tx) = bitcoin::consensus::deserialize::<bitcoin::Transaction>(&raw) else {
                    continue;
                };
                for out in tx.output.iter().take(2) {
                    if pick.txs.len() >= pick.target {
                        break;
                    }
                    let script = ScriptHash::from_script(out.script_pubkey.as_bytes());
                    if self.real_scripts.contains(&script) || !pick.scripts.insert(script) {
                        continue;
                    }
                    let hist = picker.history(script);
                    // An exchange-sized address would dwarf the wallet.
                    if hist.is_empty() || hist.len() > pick.target {
                        continue;
                    }
                    for t in hist {
                        if pick.txs.len() >= pick.target {
                            break;
                        }
                        if pick.seen.insert(t.txid) {
                            pick.txs.push(t.txid);
                            new_txs.push(t.txid);
                        }
                    }
                }
            }
        }
        for t in new_txs {
            self.owner.entry(t).or_insert(idx);
            self.pending.push_back((wallet, t));
        }
    }

    pub(crate) fn wallets(&self) -> usize {
        self.k
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231_case_2_shape() {
        // Same key and message give the same tag; a different key does not.
        let a = hmac(&[1u8; 32], b"what do ya want for nothing?");
        let b = hmac(&[1u8; 32], b"what do ya want for nothing?");
        let c = hmac(&[2u8; 32], b"what do ya want for nothing?");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn the_secret_is_created_once_and_kept() {
        let dir = std::env::temp_dir().join(format!(
            "e-lite-decoykey-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let a = load_or_create_secret(&dir).unwrap();
        let b = load_or_create_secret(&dir).unwrap();
        assert_eq!(a, b);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join(DECOY_KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
