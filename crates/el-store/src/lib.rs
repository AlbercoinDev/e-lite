//! RocksDB-backed storage for e-lite.
//!
//! One column family per logical family. Keys are fixed-size and big-endian so
//! lexicographic order equals numeric order. `txids.dat` is a flat append-only
//! file of 32-byte display-order TXIDs at `txnum * 32`.

use el_model::codec::{self, SCHEMA_VERSION};
use el_model::{
    BlockRecord, Chain, Hash32, HistoryEffect, IndexedTip, NetworkIdentity, Outpoint, ScriptHash,
    UndoRecord, UtxoValue,
};
use rocksdb::{
    BlockBasedOptions, Cache, ColumnFamilyDescriptor, DBCompressionType, Direction, IteratorMode,
    Options, WriteBatch, WriteOptions, DB,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

pub mod failpoints {
    //! Deterministic failure injection for crash tests (no env races).
    use std::sync::atomic::{AtomicU8, Ordering};
    pub const NONE: u8 = 0;
    pub const BEFORE_COMMIT: u8 = 1;
    pub const AFTER_COMMIT: u8 = 2;
    static STATE: AtomicU8 = AtomicU8::new(NONE);
    pub fn set(v: u8) {
        STATE.store(v, Ordering::SeqCst);
    }
    pub fn get() -> u8 {
        STATE.load(Ordering::SeqCst)
    }
}

pub const CF_NAMES: [&str; 7] = [
    "meta",
    "blocks",
    "tx_occurrences",
    "history",
    "utxo",
    "utxo_by_script",
    "undo",
];

#[derive(Debug)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "store error: {}", self.0)
    }
}

impl std::error::Error for StoreError {}

impl From<String> for StoreError {
    fn from(s: String) -> Self {
        StoreError(s)
    }
}

impl From<rocksdb::Error> for StoreError {
    fn from(e: rocksdb::Error) -> Self {
        StoreError(format!("rocksdb: {e}"))
    }
}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        StoreError(format!("io: {e}"))
    }
}

impl From<el_model::CodecError> for StoreError {
    fn from(e: el_model::CodecError) -> Self {
        StoreError(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(StoreError(msg.into()))
}

pub struct Store {
    db: DB,
    path: PathBuf,
    txids_path: PathBuf,
    txids: File,
}

/// Tunables for the RocksDB instance. `Store::open` uses conservative
/// defaults; the daemon can raise them from the config.
#[derive(Debug, Clone)]
pub struct StoreOptions {
    pub cache_bytes: usize,
    pub write_buffer_bytes: usize,
    pub max_open_files: i32,
    pub background_jobs: i32,
}

impl Default for StoreOptions {
    fn default() -> Self {
        StoreOptions {
            cache_bytes: 256 * 1024 * 1024,
            write_buffer_bytes: 64 * 1024 * 1024,
            max_open_files: 1024,
            background_jobs: 4,
        }
    }
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, StoreOptions::default())
    }

    pub fn open_with(path: impl AsRef<Path>, store_opts: StoreOptions) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        std::fs::create_dir_all(&path)?;

        // One shared cache for every CF: the ingest path does point lookups on
        // `utxo` and range scans on `history`, so an 8 MiB per-CF default is
        // useless on mainnet.
        let cache = Cache::new_lru_cache(store_opts.cache_bytes.max(8 * 1024 * 1024));
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        opts.set_compression_type(DBCompressionType::Lz4);
        opts.set_max_total_wal_size(256 * 1024 * 1024);
        opts.set_max_open_files(store_opts.max_open_files.max(64));
        opts.set_max_background_jobs(store_opts.background_jobs.max(1));
        opts.set_bytes_per_sync(1024 * 1024);
        opts.set_wal_bytes_per_sync(1024 * 1024);
        opts.set_write_buffer_size(store_opts.write_buffer_bytes.max(8 * 1024 * 1024));
        opts.set_max_write_buffer_number(3);
        let mut descriptors = Vec::new();
        for name in CF_NAMES {
            let mut cf_opts = Options::default();
            cf_opts.set_compression_type(DBCompressionType::Lz4);
            cf_opts.set_write_buffer_size(store_opts.write_buffer_bytes.max(8 * 1024 * 1024));
            cf_opts.set_max_write_buffer_number(3);
            let mut table = BlockBasedOptions::default();
            table.set_block_cache(&cache);
            table.set_bloom_filter(10.0, true);
            table.set_cache_index_and_filter_blocks(true);
            table.set_pin_l0_filter_and_index_blocks_in_cache(true);
            cf_opts.set_block_based_table_factory(&table);
            descriptors.push(ColumnFamilyDescriptor::new(name, cf_opts));
        }
        let db = DB::open_cf_descriptors(&opts, &path, descriptors)?;

        let txids_path = path.join("txids.dat");
        let txids = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&txids_path)?;

        let store = Store {
            db,
            path,
            txids_path,
            txids,
        };
        store.check_schema()?;
        store.reconcile_txids()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn cf(&self, name: &str) -> Result<&rocksdb::ColumnFamily> {
        self.db
            .cf_handle(name)
            .ok_or_else(|| StoreError(format!("missing column family {name}")))
    }

    // ------------------------------------------------------------------- meta

    pub fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let cf = self.cf("meta")?;
        Ok(self.db.get_cf(&cf, key.as_bytes())?)
    }

    pub fn put_meta(&self, batch: &mut WriteBatch, key: &str, value: &[u8]) -> Result<()> {
        let cf = self.cf("meta")?;
        batch.put_cf(&cf, key.as_bytes(), value);
        Ok(())
    }

    fn check_schema(&self) -> Result<()> {
        if let Some(raw) = self.get_meta("format")? {
            if raw.len() != 4 {
                return err("meta.format is not a u32");
            }
            let mut a = [0u8; 4];
            a.copy_from_slice(&raw);
            let v = u32::from_be_bytes(a);
            if v > SCHEMA_VERSION {
                return err(format!(
                    "database schema v{v} is newer than supported v{SCHEMA_VERSION}"
                ));
            }
        }
        if let Some(tip) = self.committed_tip()? {
            if tip.schema_version > SCHEMA_VERSION {
                return err(format!(
                    "committed tip schema v{} is newer than supported v{SCHEMA_VERSION}",
                    tip.schema_version
                ));
            }
        }
        Ok(())
    }

    /// On open, the flat txid file must match the committed tip exactly; extra
    /// bytes (crash between append and DB commit) are truncated, short files are
    /// a fatal integrity error.
    fn reconcile_txids(&self) -> Result<()> {
        let expected = match self.committed_tip()? {
            Some(tip) => tip.txnum * 32,
            None => 0,
        };
        let actual = self.txids.metadata()?.len();
        if actual < expected {
            return err(format!(
                "txids.dat has {actual} bytes but committed tip requires {expected}"
            ));
        }
        if actual > expected {
            self.txids.set_len(expected)?;
            self.txids.sync_data()?;
        }
        Ok(())
    }

    pub fn init_identity(&self, identity: &NetworkIdentity) -> Result<()> {
        let mut batch = WriteBatch::default();
        self.put_meta(&mut batch, "format", &SCHEMA_VERSION.to_be_bytes())?;
        self.put_meta(&mut batch, "network", identity.chain.name().as_bytes())?;
        self.put_meta(&mut batch, "genesis", identity.genesis.as_bytes())?;
        if let Some(challenge) = identity.signet_challenge {
            self.put_meta(&mut batch, "signet_challenge", challenge.as_bytes())?;
        }
        self.write_batch(batch, false)?;
        Ok(())
    }

    pub fn network_identity(&self) -> Result<Option<NetworkIdentity>> {
        let Some(name) = self.get_meta("network")? else {
            return Ok(None);
        };
        let name = String::from_utf8(name).map_err(|_| StoreError("bad network name".into()))?;
        let chain = Chain::from_name(&name)
            .ok_or_else(|| StoreError(format!("unknown chain in db: {name}")))?;
        let genesis = self
            .get_meta("genesis")?
            .ok_or_else(|| StoreError("db has network but no genesis".into()))?;
        let genesis = Hash32::from_slice(&genesis)?;
        let signet_challenge = self
            .get_meta("signet_challenge")?
            .map(|b| Hash32::from_slice(&b))
            .transpose()?;
        Ok(Some(NetworkIdentity {
            chain,
            genesis,
            signet_challenge,
        }))
    }

    // -------------------------------------------------------------------- tip

    pub fn committed_tip(&self) -> Result<Option<IndexedTip>> {
        match self.get_meta("committed_tip")? {
            None => Ok(None),
            Some(raw) => Ok(Some(codec::parse_tip_value(&raw)?)),
        }
    }

    fn put_tip(&self, batch: &mut WriteBatch, tip: &IndexedTip) -> Result<()> {
        self.put_meta(batch, "committed_tip", &codec::tip_value(tip))
    }

    // ----------------------------------------------------------------- blocks

    pub fn put_block(&self, batch: &mut WriteBatch, rec: &BlockRecord) -> Result<()> {
        let cf = self.cf("blocks")?;
        batch.put_cf(&cf, codec::block_key(rec.height), codec::block_value(rec));
        Ok(())
    }

    pub fn delete_block(&self, batch: &mut WriteBatch, height: u32) -> Result<()> {
        let cf = self.cf("blocks")?;
        batch.delete_cf(&cf, codec::block_key(height));
        Ok(())
    }

    pub fn get_block(&self, height: u32) -> Result<Option<BlockRecord>> {
        let cf = self.cf("blocks")?;
        match self.db.get_cf(&cf, codec::block_key(height))? {
            None => Ok(None),
            Some(raw) => Ok(Some(codec::parse_block_value(height, &raw)?)),
        }
    }

    pub fn last_block(&self) -> Result<Option<BlockRecord>> {
        match self.committed_tip()? {
            Some(tip) => self.get_block(tip.height),
            None => Ok(None),
        }
    }

    // -------------------------------------------------------- occurrences

    pub fn put_occurrence(
        &self,
        batch: &mut WriteBatch,
        txid: Hash32,
        height: u32,
        tx_pos: u32,
    ) -> Result<()> {
        let cf = self.cf("tx_occurrences")?;
        batch.put_cf(&cf, codec::occurrence_key(txid, height, tx_pos), []);
        Ok(())
    }

    pub fn delete_occurrence(
        &self,
        batch: &mut WriteBatch,
        txid: Hash32,
        height: u32,
        tx_pos: u32,
    ) -> Result<()> {
        let cf = self.cf("tx_occurrences")?;
        batch.delete_cf(&cf, codec::occurrence_key(txid, height, tx_pos));
        Ok(())
    }

    pub fn occurrences(&self, txid: Hash32) -> Result<Vec<(u32, u32)>> {
        let cf = self.cf("tx_occurrences")?;
        let mut out = Vec::new();
        for item in self
            .db
            .iterator_cf(&cf, IteratorMode::From(txid.as_bytes(), Direction::Forward))
        {
            let (k, _) = item?;
            if k.len() != 40 || &k[..32] != txid.as_bytes() {
                break;
            }
            out.push((
                codec::parse_occurrence_key(&k)?.1,
                codec::parse_occurrence_key(&k)?.2,
            ));
        }
        out.sort();
        Ok(out)
    }

    // ----------------------------------------------------------------- history

    pub fn put_history(
        &self,
        batch: &mut WriteBatch,
        script: ScriptHash,
        height: u32,
        tx_pos: u32,
        effect: &HistoryEffect,
    ) -> Result<()> {
        let cf = self.cf("history")?;
        batch.put_cf(
            &cf,
            codec::history_key(script, height, tx_pos),
            codec::history_value(effect),
        );
        Ok(())
    }

    pub fn delete_history(
        &self,
        batch: &mut WriteBatch,
        script: ScriptHash,
        height: u32,
        tx_pos: u32,
    ) -> Result<()> {
        let cf = self.cf("history")?;
        batch.delete_cf(&cf, codec::history_key(script, height, tx_pos));
        Ok(())
    }

    pub fn history_for_script(&self, script: ScriptHash) -> Result<Vec<(u32, u32, HistoryEffect)>> {
        let cf = self.cf("history")?;
        let prefix = script.0.as_bytes();
        let mut out = Vec::new();
        for item in self
            .db
            .iterator_cf(&cf, IteratorMode::From(prefix, Direction::Forward))
        {
            let (k, v) = item?;
            if k.len() != 40 || &k[..32] != prefix {
                break;
            }
            let (_, height, tx_pos) = codec::parse_history_key(&k)?;
            out.push((height, tx_pos, codec::parse_history_value(&v)?));
        }
        Ok(out)
    }

    // -------------------------------------------------------------------- utxo

    pub fn put_utxo(&self, batch: &mut WriteBatch, op: &Outpoint, value: &UtxoValue) -> Result<()> {
        let cf = self.cf("utxo")?;
        batch.put_cf(&cf, codec::utxo_key(op), codec::utxo_value(value));
        let cf = self.cf("utxo_by_script")?;
        batch.put_cf(
            &cf,
            codec::script_utxo_key(value.script_hash, op),
            codec::script_utxo_value(value),
        );
        Ok(())
    }

    pub fn delete_utxo(
        &self,
        batch: &mut WriteBatch,
        op: &Outpoint,
        script: ScriptHash,
    ) -> Result<()> {
        let cf = self.cf("utxo")?;
        batch.delete_cf(&cf, codec::utxo_key(op));
        let cf = self.cf("utxo_by_script")?;
        batch.delete_cf(&cf, codec::script_utxo_key(script, op));
        Ok(())
    }

    pub fn get_utxo(&self, op: &Outpoint) -> Result<Option<UtxoValue>> {
        let cf = self.cf("utxo")?;
        match self.db.get_cf(&cf, codec::utxo_key(op))? {
            None => Ok(None),
            Some(raw) => Ok(Some(codec::parse_utxo_value(&raw)?)),
        }
    }

    /// Batch point lookup for all prevouts of a block. Missing outpoints are
    /// simply absent from the map; the caller decides whether that is fatal.
    pub fn get_utxos(&self, ops: &[Outpoint]) -> Result<HashMap<Outpoint, UtxoValue>> {
        if ops.is_empty() {
            return Ok(HashMap::new());
        }
        let cf = self.cf("utxo")?;
        let keys: Vec<(&rocksdb::ColumnFamily, [u8; 36])> =
            ops.iter().map(|op| (cf, codec::utxo_key(op))).collect();
        let values = self.db.multi_get_cf(keys);
        let mut out = HashMap::with_capacity(ops.len());
        for (op, value) in ops.iter().zip(values) {
            if let Some(raw) = value? {
                out.insert(*op, codec::parse_utxo_value(&raw)?);
            }
        }
        Ok(out)
    }

    pub fn utxos_for_script(&self, script: ScriptHash) -> Result<Vec<(Outpoint, u64, u32, bool)>> {
        let cf = self.cf("utxo_by_script")?;
        let prefix = script.0.as_bytes();
        let mut out = Vec::new();
        for item in self
            .db
            .iterator_cf(&cf, IteratorMode::From(prefix, Direction::Forward))
        {
            let (k, v) = item?;
            if k.len() != 68 || &k[..32] != prefix {
                break;
            }
            let (_, op) = codec::parse_script_utxo_key(&k)?;
            let (value, height, coinbase) = codec::parse_script_utxo_value(&v)?;
            out.push((op, value, height, coinbase));
        }
        Ok(out)
    }

    // -------------------------------------------------------------------- undo

    pub fn put_undo(&self, batch: &mut WriteBatch, rec: &UndoRecord) -> Result<()> {
        let cf = self.cf("undo")?;
        batch.put_cf(&cf, codec::block_key(rec.height), codec::undo_value(rec));
        Ok(())
    }

    pub fn get_undo(&self, height: u32) -> Result<Option<UndoRecord>> {
        let cf = self.cf("undo")?;
        match self.db.get_cf(&cf, codec::block_key(height))? {
            None => Ok(None),
            Some(raw) => Ok(Some(codec::parse_undo_value(&raw)?)),
        }
    }

    pub fn delete_undo(&self, batch: &mut WriteBatch, height: u32) -> Result<()> {
        let cf = self.cf("undo")?;
        batch.delete_cf(&cf, codec::block_key(height));
        Ok(())
    }

    /// Delete every undo record below `cutoff`. Undo keys are 4-byte
    /// big-endian heights, so the iterator stops at the first live height.
    /// Returns the number of records queued for deletion.
    pub fn delete_undo_below(&self, cutoff: u32, batch: &mut WriteBatch) -> Result<u32> {
        if cutoff == 0 {
            return Ok(0);
        }
        let cf = self.cf("undo")?;
        let mut deleted = 0u32;
        for item in self.db.iterator_cf(&cf, IteratorMode::Start) {
            let (key, _) = item?;
            if key.len() != 4 {
                return err("undo key is not a u32 height");
            }
            let height = u32::from_be_bytes(key[..4].try_into().unwrap());
            if height >= cutoff {
                break;
            }
            batch.delete_cf(&cf, key);
            deleted += 1;
        }
        Ok(deleted)
    }

    // ------------------------------------------------------------- txids file

    pub fn append_txids(&self, start_txnum: u64, txids: &[Hash32]) -> Result<()> {
        let mut buf = Vec::with_capacity(txids.len() * 32);
        for t in txids {
            buf.extend_from_slice(t.as_bytes());
        }
        self.txids.write_all_at(&buf, start_txnum * 32)?;
        self.txids.sync_data()?;
        Ok(())
    }

    pub fn truncate_txids(&self, txnum: u64) -> Result<()> {
        self.txids.set_len(txnum * 32)?;
        self.txids.sync_data()?;
        Ok(())
    }

    pub fn txid_at(&self, txnum: u64) -> Result<Hash32> {
        let mut buf = [0u8; 32];
        self.txids.read_exact_at(&mut buf, txnum * 32)?;
        Ok(Hash32(buf))
    }

    pub fn block_txids(&self, rec: &BlockRecord) -> Result<Vec<Hash32>> {
        let mut out = Vec::with_capacity(rec.tx_count as usize);
        let mut buf = vec![0u8; rec.tx_count as usize * 32];
        self.txids.read_exact_at(&mut buf, rec.txnum_base * 32)?;
        for chunk in buf.chunks_exact(32) {
            out.push(Hash32::from_slice(chunk)?);
        }
        Ok(out)
    }

    // ----------------------------------------------------------------- commit

    /// Append txids (fsync), then atomically commit the batch plus the new tip.
    pub fn commit(&self, batch: WriteBatch, tip: &IndexedTip, txids: &[Hash32]) -> Result<()> {
        if txids.len() as u64 > tip.txnum {
            return err(format!(
                "commit has {} txids but tip.txnum is {}",
                txids.len(),
                tip.txnum
            ));
        }
        let start = tip.txnum - txids.len() as u64;
        self.append_txids(start, txids)?;

        if failpoints::get() == failpoints::BEFORE_COMMIT {
            return err("failpoint: before_commit (txids appended, db unchanged)");
        }

        let mut batch = batch;
        self.put_tip(&mut batch, tip)?;
        self.write_batch(batch, true)?;

        if failpoints::get() == failpoints::AFTER_COMMIT {
            return err("failpoint: after_commit (db committed)");
        }
        Ok(())
    }

    /// Commit a batch that only changes state (reorg rewind), without txids.
    pub fn commit_tip_only(&self, batch: WriteBatch, tip: &IndexedTip) -> Result<()> {
        let mut batch = batch;
        self.put_tip(&mut batch, tip)?;
        self.write_batch(batch, true)
    }

    fn write_batch(&self, batch: WriteBatch, sync: bool) -> Result<()> {
        let mut wo = WriteOptions::default();
        wo.set_sync(sync);
        self.db.write_opt(batch, &wo)?;
        Ok(())
    }

    pub fn flush(&self) -> Result<()> {
        self.flush_all()
    }

    /// Flush every column family and rotate the WAL, so on-disk measurements
    /// and restarts see settled SSTs instead of a growing write-ahead log.
    pub fn flush_all(&self) -> Result<()> {
        for name in CF_NAMES {
            let cf = self.cf(name)?;
            self.db.flush_cf(cf)?;
        }
        self.db.flush_wal(true)?;
        Ok(())
    }

    // ---------------------------------------------------------------- exports

    /// Deterministic digest over every family and the flat txid file.
    pub fn canonical_digest(&self) -> Result<Hash32> {
        let mut hasher = Sha256::new();
        hasher.update(b"e-lite-canonical-v1\0");
        for name in CF_NAMES {
            hasher.update(name.as_bytes());
            hasher.update([0u8]);
            let cf = self.cf(name)?;
            for item in self.db.iterator_cf(&cf, IteratorMode::Start) {
                let (k, v) = item?;
                hasher.update((k.len() as u32).to_be_bytes());
                hasher.update(&k);
                hasher.update((v.len() as u32).to_be_bytes());
                hasher.update(&v);
            }
        }
        let txnum = self.committed_tip()?.map(|t| t.txnum).unwrap_or(0);
        hasher.update(b"txids\0");
        let mut remaining = txnum * 32;
        let mut off = 0u64;
        let mut buf = vec![0u8; 1 << 20];
        while remaining > 0 {
            let n = remaining.min(buf.len() as u64) as usize;
            self.txids.read_exact_at(&mut buf[..n], off)?;
            hasher.update(&buf[..n]);
            remaining -= n as u64;
            off += n as u64;
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&hasher.finalize());
        Ok(Hash32(out))
    }

    /// Per-family digests, for debugging and snapshot diagnostics.
    pub fn family_digests(&self) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for name in CF_NAMES {
            let mut hasher = Sha256::new();
            let cf = self.cf(name)?;
            for item in self.db.iterator_cf(&cf, IteratorMode::Start) {
                let (k, v) = item?;
                hasher.update((k.len() as u32).to_be_bytes());
                hasher.update(&k);
                hasher.update((v.len() as u32).to_be_bytes());
                hasher.update(&v);
            }
            out.push((name.to_string(), hex::encode(hasher.finalize())));
        }
        let mut hasher = Sha256::new();
        let mut remaining = self.committed_tip()?.map(|t| t.txnum).unwrap_or(0) * 32;
        let mut off = 0u64;
        let mut buf = vec![0u8; 1 << 20];
        while remaining > 0 {
            let n = remaining.min(buf.len() as u64) as usize;
            self.txids.read_exact_at(&mut buf[..n], off)?;
            hasher.update(&buf[..n]);
            remaining -= n as u64;
            off += n as u64;
        }
        out.push(("txids".to_string(), hex::encode(hasher.finalize())));
        Ok(out)
    }

    pub fn disk_usage_bytes(&self) -> u64 {
        fn walk(p: &Path, acc: &mut u64) {
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    if let Ok(md) = e.metadata() {
                        if md.is_dir() {
                            walk(&e.path(), acc);
                        } else {
                            *acc += md.len();
                        }
                    }
                }
            }
        }
        let mut acc = 0;
        walk(&self.path, &mut acc);
        acc
    }

    pub fn txids_path(&self) -> &Path {
        &self.txids_path
    }
}

impl Store {
    /// Convenience for tests and the indexer: a fresh empty batch.
    pub fn batch() -> WriteBatch {
        WriteBatch::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "e-lite-store-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn identity() -> NetworkIdentity {
        NetworkIdentity::new(Chain::Regtest, Chain::Regtest.known_genesis().unwrap())
    }

    #[test]
    fn reopen_keeps_tip_and_truncates_extra_txids() {
        failpoints::set(failpoints::NONE);
        let dir = tmp("reopen");
        let txid = Hash32::from_hex(&"aa".repeat(32)).unwrap();
        {
            let s = Store::open(&dir).unwrap();
            s.init_identity(&identity()).unwrap();
            let mut b = Store::batch();
            s.put_block(
                &mut b,
                &BlockRecord {
                    height: 0,
                    hash: Hash32::from_hex(&"bb".repeat(32)).unwrap(),
                    header: [1u8; 80],
                    tx_count: 1,
                    txnum_base: 0,
                },
            )
            .unwrap();
            s.commit(
                b,
                &IndexedTip {
                    height: 0,
                    block_hash: Hash32::from_hex(&"bb".repeat(32)).unwrap(),
                    schema_version: SCHEMA_VERSION,
                    txnum: 1,
                },
                &[txid],
            )
            .unwrap();
        }
        // Simulate a crash between append and commit: extra bytes in txids.dat.
        {
            let f = OpenOptions::new()
                .write(true)
                .open(dir.join("txids.dat"))
                .unwrap();
            f.write_all_at(&[0xEE; 32], 32).unwrap();
        }
        let s = Store::open(&dir).unwrap();
        assert_eq!(s.committed_tip().unwrap().unwrap().txnum, 1);
        assert_eq!(s.txid_at(0).unwrap(), txid);
        assert_eq!(s.txids.metadata().unwrap().len(), 32);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failpoint_before_commit_does_not_advance_tip() {
        let dir = tmp("failpoint");
        let s = Store::open(&dir).unwrap();
        s.init_identity(&identity()).unwrap();
        failpoints::set(failpoints::BEFORE_COMMIT);
        let res = s.commit(
            Store::batch(),
            &IndexedTip {
                height: 0,
                block_hash: Hash32::from_hex(&"cc".repeat(32)).unwrap(),
                schema_version: SCHEMA_VERSION,
                txnum: 0,
            },
            &[],
        );
        assert!(res.is_err());
        failpoints::set(failpoints::NONE);
        assert_eq!(s.committed_tip().unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn short_txids_file_is_fatal() {
        let dir = tmp("short");
        let txid = Hash32::from_hex(&"11".repeat(32)).unwrap();
        {
            let s = Store::open(&dir).unwrap();
            s.init_identity(&identity()).unwrap();
            s.commit(
                Store::batch(),
                &IndexedTip {
                    height: 0,
                    block_hash: Hash32::from_hex(&"22".repeat(32)).unwrap(),
                    schema_version: SCHEMA_VERSION,
                    txnum: 1,
                },
                &[txid],
            )
            .unwrap();
        }
        std::fs::OpenOptions::new()
            .write(true)
            .open(dir.join("txids.dat"))
            .unwrap()
            .set_len(0)
            .unwrap();
        assert!(Store::open(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_newer_schema() {
        let dir = tmp("schema");
        {
            let s = Store::open(&dir).unwrap();
            let mut b = Store::batch();
            s.put_meta(&mut b, "format", &(SCHEMA_VERSION + 1).to_be_bytes())
                .unwrap();
            s.write_batch(b, true).unwrap();
        }
        assert!(Store::open(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn utxo_and_history_roundtrip() {
        let dir = tmp("utxo");
        let s = Store::open(&dir).unwrap();
        let script = ScriptHash::from_script(b"\x51");
        let op = Outpoint {
            txid: Hash32::from_hex(&"33".repeat(32)).unwrap(),
            vout: 1,
        };
        let val = UtxoValue {
            value_sat: 5000,
            script_hash: script,
            creation_height: 3,
            coinbase: false,
        };
        let mut b = Store::batch();
        s.put_utxo(&mut b, &op, &val).unwrap();
        s.put_history(
            &mut b,
            script,
            3,
            0,
            &HistoryEffect {
                received_sat: 5000,
                spent_sat: 0,
                flags: 0,
            },
        )
        .unwrap();
        s.write_batch(b, true).unwrap();
        assert_eq!(s.get_utxo(&op).unwrap().unwrap(), val);
        let utxos = s.utxos_for_script(script).unwrap();
        assert_eq!(utxos.len(), 1);
        assert_eq!(utxos[0].1, 5000);
        let hist = s.history_for_script(script).unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].2.received_sat, 5000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn canonical_digest_is_stable_across_reopen() {
        let dir = tmp("digest");
        let script = ScriptHash::from_script(b"\x51");
        let d1 = {
            let s = Store::open(&dir).unwrap();
            s.init_identity(&identity()).unwrap();
            let mut b = Store::batch();
            s.put_history(
                &mut b,
                script,
                1,
                0,
                &HistoryEffect {
                    received_sat: 9,
                    spent_sat: 0,
                    flags: 0,
                },
            )
            .unwrap();
            s.write_batch(b, true).unwrap();
            s.canonical_digest().unwrap()
        };
        let s = Store::open(&dir).unwrap();
        assert_eq!(d1, s.canonical_digest().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
