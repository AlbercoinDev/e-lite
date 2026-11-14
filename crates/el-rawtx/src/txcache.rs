//! Bounded on-disk cache of **verified raw transactions** (AUD-139).
//!
//! A wallet needs one transaction from a block, not the block: 256 MiB of
//! whole blocks held ~170 blocks, while the same budget holds tens of
//! thousands of transactions. Every eviction that forces a re-download is
//! another block hash sent to a third party, so a larger effective cache is
//! also a privacy improvement.
//!
//! Each entry stores the transaction with its merkle branch. On read the
//! caller recomputes the txid and the merkle root and compares it with the
//! indexed header, so a corrupt entry is detected and dropped. Witness bytes
//! are not committed by the txid; a SHA-256 of the stored bytes detects their
//! corruption (the cache is local and written only after full block
//! verification, including the BIP141 commitment).
//!
//! Layout: `<dir>/<first two hex digits>/<txid>.tx`. Sizes are accounted in
//! 4 KiB filesystem blocks, so the budget matches real disk use.

use crate::{RawtxError, Result};
use el_model::Hash32;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

const MAGIC: &[u8; 4] = b"ELT1";
const FS_BLOCK: u64 = 4096;
/// Branch length is bounded by the block size: 2^24 transactions per block
/// is far above consensus, so 24 levels is a safe parse limit.
const MAX_BRANCH: usize = 24;

/// A cached transaction and the proof that ties it to its block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedTx {
    pub height: u32,
    pub tx_pos: u32,
    pub branch: Vec<Hash32>,
    /// Consensus serialization (with witness).
    pub raw: Vec<u8>,
}

impl CachedTx {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + 9 + self.branch.len() * 32 + 32 + self.raw.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.height.to_be_bytes());
        out.extend_from_slice(&self.tx_pos.to_be_bytes());
        out.push(self.branch.len() as u8);
        for h in &self.branch {
            out.extend_from_slice(h.as_bytes());
        }
        out.extend_from_slice(&Sha256::digest(&self.raw));
        out.extend_from_slice(&self.raw);
        out
    }

    fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < 13 || &buf[..4] != MAGIC {
            return None;
        }
        let height = u32::from_be_bytes(buf[4..8].try_into().ok()?);
        let tx_pos = u32::from_be_bytes(buf[8..12].try_into().ok()?);
        let n = buf[12] as usize;
        if n > MAX_BRANCH {
            return None;
        }
        let mut off = 13;
        let mut branch = Vec::with_capacity(n);
        for _ in 0..n {
            branch.push(Hash32::from_slice(buf.get(off..off + 32)?).ok()?);
            off += 32;
        }
        let digest = buf.get(off..off + 32)?;
        let raw = buf.get(off + 32..)?.to_vec();
        if Sha256::digest(&raw).as_slice() != digest {
            return None;
        }
        Some(CachedTx {
            height,
            tx_pos,
            branch,
            raw,
        })
    }
}

fn disk_size(len: u64) -> u64 {
    len.div_ceil(FS_BLOCK).max(1) * FS_BLOCK
}

#[derive(Default)]
struct State {
    /// Accounted bytes (rounded to filesystem blocks).
    size: u64,
    clock: u64,
    /// txid -> (accounted size, recency stamp)
    entries: HashMap<Hash32, (u64, u64)>,
    /// recency stamp -> txid; the first entry is the least recently used.
    order: BTreeMap<u64, Hash32>,
}

impl State {
    fn touch(&mut self, txid: &Hash32) {
        self.clock += 1;
        let stamp = self.clock;
        if let Some(entry) = self.entries.get_mut(txid) {
            self.order.remove(&entry.1);
            entry.1 = stamp;
            self.order.insert(stamp, *txid);
        }
    }

    fn insert(&mut self, txid: Hash32, size: u64) {
        self.remove(&txid);
        self.clock += 1;
        self.entries.insert(txid, (size, self.clock));
        self.order.insert(self.clock, txid);
        self.size += size;
    }

    fn remove(&mut self, txid: &Hash32) -> bool {
        match self.entries.remove(txid) {
            Some((size, stamp)) => {
                self.order.remove(&stamp);
                self.size = self.size.saturating_sub(size);
                true
            }
            None => false,
        }
    }

    fn oldest(&self) -> Option<Hash32> {
        self.order.values().next().copied()
    }
}

pub struct TxCache {
    dir: PathBuf,
    capacity: u64,
    state: Mutex<State>,
}

impl TxCache {
    pub fn open(dir: &Path, capacity: u64) -> Result<Self> {
        fs::create_dir_all(dir).map_err(|e| {
            RawtxError::Io(format!("cannot create tx cache {}: {e}", dir.display()))
        })?;
        // Rebuild the recency order from mtimes (refreshed on every hit).
        let mut found: Vec<(SystemTime, Hash32, u64)> = Vec::new();
        for shard in fs::read_dir(dir)
            .map_err(|e| RawtxError::Io(format!("cannot read tx cache: {e}")))?
            .flatten()
        {
            if !shard.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Ok(files) = fs::read_dir(shard.path()) else {
                continue;
            };
            for file in files.flatten() {
                let name = file.file_name();
                let name = name.to_string_lossy();
                let Some(stem) = name.strip_suffix(".tx") else {
                    continue;
                };
                let Ok(txid) = Hash32::from_hex(stem) else {
                    continue;
                };
                if let Ok(md) = file.metadata() {
                    let mtime = md.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    found.push((mtime, txid, disk_size(md.len())));
                }
            }
        }
        found.sort_by_key(|(mtime, txid, _)| (*mtime, *txid));
        let mut state = State::default();
        for (_, txid, size) in found {
            state.insert(txid, size);
        }
        let cache = TxCache {
            dir: dir.to_path_buf(),
            capacity,
            state: Mutex::new(state),
        };
        if let Ok(mut state) = cache.state.lock() {
            cache.evict_to(&mut state, cache.capacity);
        }
        Ok(cache)
    }

    fn path(&self, txid: &Hash32) -> PathBuf {
        let hex = txid.to_hex();
        self.dir.join(&hex[..2]).join(format!("{hex}.tx"))
    }

    fn evict_to(&self, state: &mut State, limit: u64) {
        while state.size > limit {
            let Some(oldest) = state.oldest() else {
                break;
            };
            state.remove(&oldest);
            let _ = fs::remove_file(self.path(&oldest));
        }
    }

    pub fn len(&self) -> usize {
        self.state.lock().map(|s| s.entries.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Accounted size in bytes (filesystem blocks).
    pub fn size(&self) -> u64 {
        self.state.lock().map(|s| s.size).unwrap_or(0)
    }

    /// The stored entry, or `None`. A file that cannot be decoded (truncated
    /// write, corruption) is removed. The caller still authenticates the
    /// entry against the indexed header.
    pub fn get(&self, txid: &Hash32) -> Option<CachedTx> {
        {
            let state = self.state.lock().ok()?;
            if !state.entries.contains_key(txid) {
                return None;
            }
        }
        let path = self.path(txid);
        match fs::read(&path).ok().and_then(|b| CachedTx::decode(&b)) {
            Some(entry) => {
                let now = filetime::FileTime::from_system_time(SystemTime::now());
                let _ = filetime::set_file_mtime(&path, now);
                if let Ok(mut state) = self.state.lock() {
                    state.touch(txid);
                }
                Some(entry)
            }
            None => {
                self.remove(txid);
                None
            }
        }
    }

    pub fn put(&self, txid: &Hash32, entry: &CachedTx) -> Result<()> {
        let bytes = entry.encode();
        let size = disk_size(bytes.len() as u64);
        if self.capacity == 0 || size > self.capacity {
            return Ok(());
        }
        let path = self.path(txid);
        // Already cached (a wallet retrying a batch, or the prefetch, put it
        // a moment ago): nothing to write.
        if self
            .state
            .lock()
            .map(|s| s.entries.contains_key(txid))
            .unwrap_or(false)
        {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| RawtxError::Io(format!("tx cache dir: {e}")))?;
        }
        // No fsync: a torn write fails the checksum and is dropped on read.
        // A unique temporary name: two writers of the same txid at once must
        // not rename each other's file away.
        let tmp = path.with_extension(format!("tmp{:016x}", rand::random::<u64>()));
        {
            let mut f = fs::File::create(&tmp)
                .map_err(|e| RawtxError::Io(format!("tx cache create: {e}")))?;
            f.write_all(&bytes)
                .map_err(|e| RawtxError::Io(format!("tx cache write: {e}")))?;
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| RawtxError::Io("tx cache lock poisoned".into()))?;
        state.remove(txid);
        self.evict_to(&mut state, self.capacity.saturating_sub(size));
        fs::rename(&tmp, &path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            RawtxError::Io(format!("tx cache rename: {e}"))
        })?;
        state.insert(*txid, size);
        Ok(())
    }

    pub fn remove(&self, txid: &Hash32) {
        let removed = self
            .state
            .lock()
            .map(|mut s| s.remove(txid))
            .unwrap_or(false);
        if removed {
            let _ = fs::remove_file(self.path(txid));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "e-lite-txcache-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn entry(n: u8, len: usize) -> CachedTx {
        CachedTx {
            height: n as u32,
            tx_pos: 1,
            branch: vec![Hash32([n; 32]), Hash32([n.wrapping_add(1); 32])],
            raw: vec![n; len],
        }
    }

    #[test]
    fn concurrent_puts_of_one_transaction_all_succeed() {
        // A wallet retrying its batch and the prefetch cache the same
        // transaction at once: a shared temporary name made one rename fail.
        let dir = tmp("concurrent");
        let cache = std::sync::Arc::new(TxCache::open(&dir, 1 << 20).unwrap());
        let txid = Hash32([9; 32]);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let cache = std::sync::Arc::clone(&cache);
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    cache.put(&txid, &entry(9, 200))
                })
            })
            .collect();
        for h in handles {
            assert!(h.join().unwrap().is_ok());
        }
        assert_eq!(cache.get(&txid).unwrap(), entry(9, 200));
        assert_eq!(cache.len(), 1);
        let stray = fs::read_dir(cache.path(&txid).parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|f| !f.file_name().to_string_lossy().ends_with(".tx"))
            .count();
        assert_eq!(stray, 0, "no temporary file left behind");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn entries_round_trip_and_corruption_is_dropped() {
        let dir = tmp("roundtrip");
        let cache = TxCache::open(&dir, 1 << 20).unwrap();
        let txid = Hash32([7; 32]);
        cache.put(&txid, &entry(7, 300)).unwrap();
        assert_eq!(cache.get(&txid).unwrap(), entry(7, 300));
        assert_eq!(cache.size(), FS_BLOCK, "accounted in filesystem blocks");

        // Flip a byte of the stored transaction: the checksum rejects it and
        // the entry is removed.
        let path = cache.path(&txid);
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(&path, bytes).unwrap();
        assert!(cache.get(&txid).is_none());
        assert!(cache.is_empty());
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn evicts_least_recently_used_and_survives_reopen() {
        let dir = tmp("lru");
        let cache = TxCache::open(&dir, 3 * FS_BLOCK).unwrap();
        let (a, b, c, d) = (
            Hash32([1; 32]),
            Hash32([2; 32]),
            Hash32([3; 32]),
            Hash32([4; 32]),
        );
        cache.put(&a, &entry(1, 100)).unwrap();
        cache.put(&b, &entry(2, 100)).unwrap();
        cache.put(&c, &entry(3, 100)).unwrap();
        assert!(cache.get(&a).is_some()); // a becomes the most recent
        cache.put(&d, &entry(4, 100)).unwrap();
        assert!(cache.get(&b).is_none(), "least recently used is evicted");
        assert!(cache.get(&a).is_some());
        assert!(cache.get(&c).is_some());
        assert!(cache.get(&d).is_some());
        assert_eq!(cache.len(), 3);
        drop(cache);

        // A lower capacity applies at open time.
        let cache = TxCache::open(&dir, 2 * FS_BLOCK).unwrap();
        assert_eq!(cache.len(), 2);
        assert!(cache.size() <= 2 * FS_BLOCK);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_zero_capacity_cache_stores_nothing() {
        let dir = tmp("zero");
        let cache = TxCache::open(&dir, 0).unwrap();
        cache.put(&Hash32([9; 32]), &entry(9, 10)).unwrap();
        assert!(cache.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
