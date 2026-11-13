//! On-disk layout of the rawtx cache and the only helpers allowed to delete it.
//!
//! The cache is the single piece of memoised data e-lite keeps on disk. Its
//! contents are exactly:
//!
//! - `<dir>/*.blk`: whole validated blocks (prune-gap repair and legacy entries).
//! - `<dir>/tx/<xx>/<txid>.tx`: verified transactions with their merkle branch.
//!
//! Everything else in that directory is foreign and is never touched. The
//! index (RocksDB), Core's datadir, TLS and Tor live next to it and share the
//! data directory, so deletion is pattern-based, never recursive: a wrong path
//! is refused instead of wiped (see [`refuse_reason`]).

use crate::{RawtxError, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Default cache directory name inside the data directory.
pub const CACHE_DIR_NAME: &str = "rawtx-cache";

/// Files and bytes held by the cache, split by kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheInventory {
    pub block_files: u64,
    pub block_bytes: u64,
    pub tx_files: u64,
    pub tx_bytes: u64,
    /// Entries that are not cache files (left untouched, counted for the report).
    pub skipped: u64,
}

impl CacheInventory {
    pub fn files(&self) -> u64 {
        self.block_files + self.tx_files
    }

    pub fn bytes(&self) -> u64 {
        self.block_bytes + self.tx_bytes
    }

    pub fn is_empty(&self) -> bool {
        self.files() == 0
    }
}

/// The cache directory the resolver uses for a data directory.
pub fn default_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(CACHE_DIR_NAME)
}

/// Resolve the cache directory exactly like [`crate::Resolver::new`]: an
/// explicit `[rawtx] cache_dir` wins, empty means `<data-dir>/rawtx-cache`.
pub fn resolve_dir(data_dir: &Path, configured: &str) -> PathBuf {
    let configured = configured.trim();
    if configured.is_empty() {
        default_dir(data_dir)
    } else {
        PathBuf::from(configured)
    }
}

/// Why this path must not be treated as the rawtx cache, if it must not.
///
/// The data directory holds the RocksDB index at its root and Core, TLS and
/// Tor beside it; a misconfigured `cache_dir` could otherwise point the
/// cleanup at any of them.
pub fn refuse_reason(dir: &Path, data_dir: &Path) -> Option<String> {
    let canonical = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let dir = canonical(dir);
    let data = canonical(data_dir);
    if dir == data {
        return Some("it resolves to the data directory itself".to_string());
    }
    if data.starts_with(&dir) {
        return Some("it contains the data directory".to_string());
    }
    if dir.parent().is_none() {
        return Some("it is a filesystem root".to_string());
    }
    if dir.join("CURRENT").is_file() {
        return Some("it holds a RocksDB index (CURRENT is present)".to_string());
    }
    for name in ["index", "bitcoin", "tls", "tor"] {
        if dir.join(name).is_dir() {
            return Some(format!("it holds {name}/"));
        }
        if dir.file_name().and_then(|n| n.to_str()) == Some(name) && dir.starts_with(&data) {
            return Some(format!("it is the {name} directory of the data dir"));
        }
    }
    None
}

/// Walk the cache directory and account its entries. A missing directory is an
/// empty cache, not an error.
///
/// Layout: `*.blk` at the root and `tx/<xx>/<txid>.tx` below it. Anything else
/// (a stray note, a non-hex shard, a symlink) is counted as skipped.
pub fn inventory(dir: &Path) -> Result<CacheInventory> {
    let mut out = CacheInventory::default();
    walk(dir, &mut out, |kind, _path, size, out| match kind {
        CacheKind::Block => {
            out.block_files += 1;
            out.block_bytes += size;
        }
        CacheKind::Tx => {
            out.tx_files += 1;
            out.tx_bytes += size;
        }
        CacheKind::Skipped => out.skipped += 1,
        CacheKind::Shard => {}
    })?;
    Ok(out)
}

/// Delete the cache entries and report what was removed.
///
/// Only regular `*.blk` files in the cache root and `*.tx` files under a
/// `tx/<xx>/` shard are removed; empty shard directories are dropped. Anything
/// else is left in place and counted as skipped. Symlinks are never followed or
/// removed. Idempotent: a second call removes nothing.
pub fn clear(dir: &Path, keep_blocks: bool) -> Result<CacheInventory> {
    let mut removed = CacheInventory::default();
    walk(dir, &mut removed, |kind, path, _size, removed| match kind {
        CacheKind::Block if !keep_blocks => {
            remove_file(path, &mut removed.block_files, &mut removed.block_bytes);
        }
        CacheKind::Block => {}
        CacheKind::Tx => remove_file(path, &mut removed.tx_files, &mut removed.tx_bytes),
        // A shard is structure, not an entry: only foreign files are skipped.
        CacheKind::Shard => {}
        CacheKind::Skipped => removed.skipped += 1,
    })?;
    // Drop every shard that is now empty; a foreign file keeps its shard.
    let tx = dir.join("tx");
    if let Ok(shards) = fs::read_dir(&tx) {
        for shard in shards.flatten() {
            if shard.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let _ = fs::remove_dir(shard.path());
            }
        }
    }
    Ok(removed)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CacheKind {
    Block,
    Tx,
    /// A `tx/<xx>` directory (nothing to remove itself).
    Shard,
    Skipped,
}

/// Visit every entry that is not a shard directory. `f` receives the kind, the
/// path and the file size (0 for non-files).
fn walk(
    dir: &Path,
    out: &mut CacheInventory,
    mut f: impl FnMut(CacheKind, &Path, u64, &mut CacheInventory),
) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(RawtxError::Io(format!(
                "cannot read cache {}: {e}",
                dir.display()
            )))
        }
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            f(CacheKind::Skipped, &entry.path(), 0, out);
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if kind.is_file() && name.ends_with(".blk") && is_hex(name.trim_end_matches(".blk")) {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            f(CacheKind::Block, &entry.path(), size, out);
        } else if kind.is_dir() && name == "tx" {
            let shards = match fs::read_dir(entry.path()) {
                Ok(shards) => shards,
                Err(_) => {
                    f(CacheKind::Skipped, &entry.path(), 0, out);
                    continue;
                }
            };
            for shard in shards.flatten() {
                let Ok(shard_kind) = shard.file_type() else {
                    f(CacheKind::Skipped, &shard.path(), 0, out);
                    continue;
                };
                let shard_name = shard.file_name();
                let shard_name = shard_name.to_string_lossy();
                if !shard_kind.is_dir() || !is_shard(&shard_name) {
                    f(CacheKind::Skipped, &shard.path(), 0, out);
                    continue;
                }
                f(CacheKind::Shard, &shard.path(), 0, out);
                let Ok(files) = fs::read_dir(shard.path()) else {
                    continue;
                };
                for file in files.flatten() {
                    let Ok(file_kind) = file.file_type() else {
                        f(CacheKind::Skipped, &file.path(), 0, out);
                        continue;
                    };
                    let file_name = file.file_name();
                    let file_name = file_name.to_string_lossy();
                    if file_kind.is_file()
                        && file_name.ends_with(".tx")
                        && is_hex(file_name.trim_end_matches(".tx"))
                    {
                        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
                        f(CacheKind::Tx, &file.path(), size, out);
                    } else {
                        f(CacheKind::Skipped, &file.path(), 0, out);
                    }
                }
            }
        } else {
            f(CacheKind::Skipped, &entry.path(), 0, out);
        }
    }
    Ok(())
}

fn remove_file(path: &Path, files: &mut u64, bytes: &mut u64) {
    // `remove_file` unlinks the entry itself and never follows a symlink; the
    // walk already skipped anything that is not a regular file.
    let len = path.metadata().map(|m| m.len()).unwrap_or(0);
    if fs::remove_file(path).is_ok() {
        *files += 1;
        *bytes += len;
    }
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_shard(name: &str) -> bool {
    name.len() == 2 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "e-lite-cache-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn populate(data_dir: &Path) -> PathBuf {
        // The data directory holds the index, Core, TLS, Tor and the cache.
        fs::write(data_dir.join("CURRENT"), b"MANIFEST-000001\n").unwrap();
        fs::write(data_dir.join("000005.sst"), vec![7u8; 4096]).unwrap();
        fs::create_dir_all(data_dir.join("bitcoin/blocks")).unwrap();
        fs::write(data_dir.join("bitcoin/blocks/blk00000.dat"), b"core").unwrap();
        fs::create_dir_all(data_dir.join("tls")).unwrap();
        fs::write(data_dir.join("tls/ca.key"), b"secret").unwrap();
        fs::create_dir_all(data_dir.join("tor/onion")).unwrap();
        fs::write(data_dir.join("tor/onion/hs_ed25519_secret_key"), b"key").unwrap();
        let cache = default_dir(data_dir);
        fs::create_dir_all(cache.join("tx/ab")).unwrap();
        fs::write(cache.join("0123456789abcdef.blk"), vec![1u8; 2048]).unwrap();
        fs::write(cache.join("tx/ab/cdef.tx"), vec![2u8; 512]).unwrap();
        // Foreign entries: never touched.
        fs::write(cache.join("notes.txt"), b"keep me").unwrap();
        fs::create_dir_all(cache.join("tx/zz")).unwrap();
        fs::write(cache.join("tx/zz/nothex.tx"), b"keep me too").unwrap();
        cache
    }

    #[test]
    fn inventory_counts_only_cache_entries() {
        let data = scratch("inv");
        let cache = populate(&data);
        let inv = inventory(&cache).unwrap();
        assert_eq!(inv.block_files, 1);
        assert_eq!(inv.block_bytes, 2048);
        assert_eq!(inv.tx_files, 1);
        assert_eq!(inv.tx_bytes, 512);
        assert_eq!(inv.files(), 2);
        assert_eq!(inv.bytes(), 2560);
        // notes.txt and tx/zz/nothex.tx
        assert_eq!(inv.skipped, 2);
        let _ = fs::remove_dir_all(&data);
    }

    #[test]
    fn clear_removes_cache_and_leaves_everything_else() {
        let data = scratch("clear");
        let cache = populate(&data);
        let before_sst = fs::read(data.join("000005.sst")).unwrap();
        let before_key = fs::read(data.join("tls/ca.key")).unwrap();
        let before_onion = fs::read(data.join("tor/onion/hs_ed25519_secret_key")).unwrap();
        let before_core = fs::read(data.join("bitcoin/blocks/blk00000.dat")).unwrap();

        let removed = clear(&cache, false).unwrap();
        assert_eq!(removed.files(), 2);
        assert_eq!(removed.bytes(), 2560);
        assert_eq!(removed.skipped, 2);

        assert!(!cache.join("0123456789abcdef.blk").exists());
        assert!(!cache.join("tx/ab/cdef.tx").exists());
        // The emptied shard goes away; the one holding a foreign file stays.
        assert!(!cache.join("tx/ab").exists());
        assert!(cache.join("tx/zz/nothex.tx").exists());
        assert!(cache.join("notes.txt").exists());
        // The index, Core, TLS and Tor are byte-identical.
        assert_eq!(fs::read(data.join("000005.sst")).unwrap(), before_sst);
        assert_eq!(fs::read(data.join("tls/ca.key")).unwrap(), before_key);
        assert_eq!(
            fs::read(data.join("tor/onion/hs_ed25519_secret_key")).unwrap(),
            before_onion
        );
        assert_eq!(
            fs::read(data.join("bitcoin/blocks/blk00000.dat")).unwrap(),
            before_core
        );
        assert!(data.join("CURRENT").is_file());

        // Idempotent.
        let again = clear(&cache, false).unwrap();
        assert_eq!(again.files(), 0);
        let _ = fs::remove_dir_all(&data);
    }

    #[test]
    fn keep_blocks_preserves_whole_blocks() {
        let data = scratch("keep");
        let cache = populate(&data);
        let removed = clear(&cache, true).unwrap();
        assert_eq!(removed.block_files, 0);
        assert_eq!(removed.tx_files, 1);
        assert!(cache.join("0123456789abcdef.blk").exists());
        let _ = fs::remove_dir_all(&data);
    }

    #[test]
    fn missing_cache_is_empty_not_an_error() {
        let data = scratch("missing");
        let cache = default_dir(&data);
        assert_eq!(inventory(&cache).unwrap(), CacheInventory::default());
        assert_eq!(clear(&cache, false).unwrap(), CacheInventory::default());
        let _ = fs::remove_dir_all(&data);
    }

    #[test]
    fn dangerous_paths_are_refused() {
        let data = scratch("guard");
        populate(&data);
        assert!(refuse_reason(&data, &data).is_some(), "data dir itself");
        assert!(
            refuse_reason(&data.join("bitcoin"), &data).is_some(),
            "Core datadir"
        );
        assert!(
            refuse_reason(&data.join("tor"), &data).is_some(),
            "Tor data"
        );
        assert!(
            refuse_reason(Path::new("/"), &data).is_some(),
            "filesystem root"
        );
        assert!(
            refuse_reason(&data, &data.join("e-lite")).is_some(),
            "ancestor of the data dir"
        );
        assert!(refuse_reason(&default_dir(&data), &data).is_none());
        // An explicit cache_dir outside the data dir is allowed.
        assert!(refuse_reason(&data.join("elsewhere"), &data).is_none());
        let _ = fs::remove_dir_all(&data);
    }

    #[test]
    fn resolve_dir_follows_the_resolver_default() {
        let data = Path::new("/data/networks/mainnet");
        assert_eq!(resolve_dir(data, ""), data.join(CACHE_DIR_NAME));
        assert_eq!(resolve_dir(data, "  "), data.join(CACHE_DIR_NAME));
        assert_eq!(
            resolve_dir(data, "/dev/shm/e-lite-rawtx"),
            PathBuf::from("/dev/shm/e-lite-rawtx")
        );
    }
}
