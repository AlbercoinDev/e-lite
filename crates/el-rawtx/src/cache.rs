//! Bounded on-disk cache of **validated** raw blocks (LRU by mtime).

use crate::{RawtxError, Result};
use el_model::Hash32;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

struct State {
    size: u64,
    entries: HashMap<Hash32, u64>,
}

pub struct BlockCache {
    dir: PathBuf,
    capacity: u64,
    state: Mutex<State>,
}

impl BlockCache {
    pub fn open(dir: &Path, capacity: u64) -> Result<Self> {
        fs::create_dir_all(dir)
            .map_err(|e| RawtxError::Io(format!("cannot create cache {}: {e}", dir.display())))?;
        let mut entries = HashMap::new();
        let mut size = 0u64;
        for entry in fs::read_dir(dir)
            .map_err(|e| RawtxError::Io(format!("cannot read cache: {e}")))?
            .flatten()
        {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(stem) = name.strip_suffix(".blk") else {
                continue;
            };
            let Ok(hash) = Hash32::from_hex(stem) else {
                continue;
            };
            if let Ok(md) = entry.metadata() {
                entries.insert(hash, md.len());
                size += md.len();
            }
        }
        Ok(BlockCache {
            dir: dir.to_path_buf(),
            capacity,
            state: Mutex::new(State { size, entries }),
        })
    }

    fn path(&self, hash: &Hash32) -> PathBuf {
        self.dir.join(format!("{}.blk", hash.to_hex()))
    }

    pub fn is_empty(&self) -> bool {
        self.state
            .lock()
            .map(|state| state.entries.is_empty())
            .unwrap_or(true)
    }

    pub fn get(&self, hash: &Hash32) -> Option<Vec<u8>> {
        let mut st = self.state.lock().ok()?;
        if !st.entries.contains_key(hash) {
            return None;
        }
        let path = self.path(hash);
        match fs::read(&path) {
            Ok(bytes) => {
                // Refresh mtime so this entry is the most recently used.
                let now = filetime::FileTime::from_system_time(SystemTime::now());
                let _ = filetime::set_file_mtime(&path, now);
                Some(bytes)
            }
            Err(_) => {
                if let Some(sz) = st.entries.remove(hash) {
                    st.size = st.size.saturating_sub(sz);
                }
                None
            }
        }
    }

    pub fn put(&self, hash: &Hash32, bytes: &[u8]) -> Result<()> {
        if self.capacity == 0 {
            return Ok(());
        }
        if bytes.len() as u64 > self.capacity {
            return Ok(()); // never let one block evict the whole cache
        }
        let mut st = self
            .state
            .lock()
            .map_err(|_| RawtxError::Io("cache lock poisoned".into()))?;

        if let Some(old) = st.entries.get(hash).copied() {
            st.size = st.size.saturating_sub(old);
            st.entries.remove(hash);
        }
        while st.size + bytes.len() as u64 > self.capacity {
            if !evict_one(&self.dir, &mut st) {
                break;
            }
        }

        let path = self.path(hash);
        let tmp = path.with_extension("tmp");
        {
            let mut f =
                fs::File::create(&tmp).map_err(|e| RawtxError::Io(format!("cache create: {e}")))?;
            f.write_all(bytes)
                .map_err(|e| RawtxError::Io(format!("cache write: {e}")))?;
            f.sync_data()
                .map_err(|e| RawtxError::Io(format!("cache sync: {e}")))?;
        }
        fs::rename(&tmp, &path).map_err(|e| RawtxError::Io(format!("cache rename: {e}")))?;
        st.size += bytes.len() as u64;
        st.entries.insert(*hash, bytes.len() as u64);
        Ok(())
    }

    pub fn remove(&self, hash: &Hash32) -> Result<()> {
        let mut st = self
            .state
            .lock()
            .map_err(|_| RawtxError::Io("cache lock poisoned".into()))?;
        if let Some(sz) = st.entries.remove(hash) {
            st.size = st.size.saturating_sub(sz);
            let _ = fs::remove_file(self.path(hash));
        }
        Ok(())
    }

    pub fn size(&self) -> u64 {
        self.state.lock().map(|s| s.size).unwrap_or(0)
    }
}

/// Remove the oldest entry (by mtime). Returns false when empty.
fn evict_one(dir: &Path, st: &mut State) -> bool {
    let mut oldest: Option<(Hash32, SystemTime, u64)> = None;
    let keys: Vec<Hash32> = st.entries.keys().copied().collect();
    for hash in keys {
        let path = dir.join(format!("{}.blk", hash.to_hex()));
        if let Ok(md) = fs::metadata(&path) {
            let mtime = md.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            if oldest.as_ref().is_none_or(|(_, t, _)| mtime < *t) {
                oldest = Some((hash, mtime, md.len()));
            }
        } else {
            // Stale entry.
            let sz = st.entries.get(&hash).copied().unwrap_or(0);
            st.entries.remove(&hash);
            st.size = st.size.saturating_sub(sz);
            return true;
        }
    }
    if let Some((hash, _, size)) = oldest {
        st.entries.remove(&hash);
        st.size = st.size.saturating_sub(size);
        let _ = fs::remove_file(dir.join(format!("{}.blk", hash.to_hex())));
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "e-lite-cache-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn stores_reads_and_evicts() {
        let dir = tmp("lru");
        let cache = BlockCache::open(&dir, 1000).unwrap();
        let a = Hash32([1; 32]);
        let b = Hash32([2; 32]);
        let c = Hash32([3; 32]);
        cache.put(&a, &[1u8; 400]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        cache.put(&b, &[2u8; 400]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        // Touch a so b becomes the oldest.
        assert!(cache.get(&a).is_some());
        std::thread::sleep(std::time::Duration::from_millis(20));
        cache.put(&c, &[3u8; 400]).unwrap();
        assert!(cache.get(&a).is_some(), "recently used entry must survive");
        assert!(cache.get(&b).is_none(), "oldest entry must be evicted");
        assert!(cache.get(&c).is_some());
        assert!(cache.size() <= 1000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skips_blocks_larger_than_capacity() {
        let dir = tmp("big");
        let cache = BlockCache::open(&dir, 100).unwrap();
        cache.put(&Hash32([9; 32]), &[0u8; 200]).unwrap();
        assert!(cache.get(&Hash32([9; 32])).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
