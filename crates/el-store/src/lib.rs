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
    Options, WriteBatch, WriteBufferManager, WriteOptions, DB,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

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

#[derive(Debug, Clone, Copy, Default)]
pub struct CompactionStats {
    pub pending_compaction_bytes: u64,
    pub running_compactions: u64,
    pub memtable_bytes: u64,
    pub block_cache_bytes: u64,
    pub sst_files_bytes: u64,
    pub table_readers_bytes: u64,
    /// Summed L0 file count across every column family. A growing value with
    /// one running compaction is the real flush/compaction stall signal.
    pub level0_files: u64,
    /// False when any per-CF property was unsupported or failed; a zero is
    /// then "unknown", not "empty".
    pub properties_ok: bool,
}

/// Descriptor plan for the RocksDB instance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FdPlan {
    pub requested: i32,
    /// Value actually handed to RocksDB.
    pub effective: i32,
    /// Process soft `RLIMIT_NOFILE`, if it is finite.
    pub soft_limit: Option<u64>,
    /// Descriptors kept free for sockets, logs, RPC, providers and threads.
    pub reserve: u64,
    /// True when the requested value had to be reduced.
    pub capped: bool,
}

impl FdPlan {
    pub fn below_recommended(&self) -> bool {
        self.capped && self.effective < 2048
    }
}

/// Cap RocksDB's open-file target to what the process can really open.
///
/// `RLIMIT_NOFILE` can be `RLIM_INFINITY` (u64::MAX); casting that to a signed
/// integer yields -1 and used to collapse the target to the minimum. The
/// reserve covers client sockets, RPC, provider connections and log files, so
/// it must never be subtracted twice or overridden by a floor.
pub fn fd_plan(requested: i32, reserve: u64) -> FdPlan {
    fd_plan_with(requested, reserve, process_soft_nofile())
}

/// Process `RLIMIT_NOFILE`, or `None` when it is unlimited or unreadable.
fn process_soft_nofile() -> Option<u64> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // `rlim_t` is u64 on 64-bit and u32 on 32-bit; the conversion is only
    // redundant on the former.
    #[allow(clippy::useless_conversion)]
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
        Some(u64::from(limit.rlim_cur))
    } else {
        None
    }
}

/// Pure form of [`fd_plan`], testable without touching process limits.
pub fn fd_plan_with(requested: i32, reserve: u64, soft: Option<u64>) -> FdPlan {
    let requested = requested.max(20);
    let (effective, capped) = match soft {
        // RLIM_INFINITY or an absurdly large limit: keep the request.
        None => (requested, false),
        Some(soft) if soft >= u64::MAX / 2 => (requested, false),
        Some(soft) => {
            let room = soft.saturating_sub(reserve);
            // RocksDB itself reserves 10 descriptors; never exceed the limit.
            let target = room.min(requested as u64).min(soft.saturating_sub(10));
            let target = target as i32;
            // RocksDB clamps anything below 20 up to 20, which would exceed a
            // tiny limit; in that case fail loudly instead of lying.
            (target.max(1), target < requested)
        }
    };
    FdPlan {
        requested,
        effective,
        soft_limit: soft.filter(|v| *v < u64::MAX / 2),
        reserve,
        capped,
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

/// Relative write pressure of each family. `utxo` and `history` dominate an
/// IBD; giving them a larger memtable avoids frequent flushes and commit
/// stalls. The total is still bounded by the shared WriteBufferManager.
const CF_WEIGHTS: [u32; 7] = [1, 2, 1, 4, 6, 4, 1];

fn weighted_write_buffer(total_bytes: usize, name: &str, max_buffers: usize) -> usize {
    let idx = CF_NAMES.iter().position(|n| *n == name).unwrap_or(0);
    let sum: u32 = CF_WEIGHTS.iter().sum();
    let budget = total_bytes / max_buffers.max(1);
    // The shared WriteBufferManager is the real total cap; per-CF sizes are
    // flush thresholds. The old 64 MiB clamp made `memtables_mib` a promise
    // the store could not keep (7 CFs x 64 MiB < 2 GiB).
    (budget.saturating_mul(CF_WEIGHTS[idx] as usize) / sum as usize).max(1)
}

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
    /// Kept so the block-cache capacity can be changed at runtime.
    cache: Cache,
    path: PathBuf,
    txids_path: PathBuf,
    txids: File,
    /// Keeps the memtable budget alive for the lifetime of the DB.
    write_buffer_manager: WriteBufferManager,
    /// Memtables per CF, used to recompute per-CF write_buffer_size on resize.
    max_write_buffers: usize,
    fd_plan: FdPlan,
    /// Paths that `disk_usage_bytes` must skip (for example a Core datadir
    /// nested inside the data directory).
    exclude: Vec<PathBuf>,
    /// Last commit split: txids append+fsync and RocksDB write.
    commit_txids_us: AtomicU64,
    commit_db_us: AtomicU64,
}

/// Tunables for the RocksDB instance. `Store::open` uses conservative
/// defaults; the daemon can raise them from the config.
#[derive(Debug, Clone)]
pub struct StoreOptions {
    pub cache_bytes: usize,
    /// Total memtable budget across every column family (not per CF).
    pub write_buffer_total_bytes: usize,
    /// Memtables per column family.
    pub max_write_buffer_number: usize,
    pub max_total_wal_bytes: usize,
    pub max_open_files: i32,
    /// Descriptors kept free for everything that is not RocksDB.
    pub fd_reserve: u64,
    pub background_jobs: i32,
    /// Paths skipped by `disk_usage_bytes` (Core datadir, logs, snapshots).
    pub exclude_paths: Vec<PathBuf>,
}

impl Default for StoreOptions {
    fn default() -> Self {
        StoreOptions {
            cache_bytes: 256 * 1024 * 1024,
            write_buffer_total_bytes: 256 * 1024 * 1024,
            max_write_buffer_number: 3,
            max_total_wal_bytes: 256 * 1024 * 1024,
            // Keep every SST reader open: table-cache churn shows up as random
            // read latency in prevout prefetch on large indexes.
            max_open_files: 8192,
            fd_reserve: 320,
            background_jobs: 4,
            exclude_paths: Vec::new(),
        }
    }
}

/// Per-column-family memtable size for a total budget. RocksDB applies the
/// write-buffer size to each CF, so multiplying by the number of families and
/// memtables is what actually consumes memory.
pub fn per_cf_write_buffer_bytes(
    total_bytes: usize,
    cf_count: usize,
    max_write_buffer_number: usize,
) -> usize {
    let slots = cf_count
        .max(1)
        .saturating_mul(max_write_buffer_number.max(1));
    let ideal = (total_bytes / slots).max(1);
    // The shared WriteBufferManager enforces the total; a per-CF cap here
    // would make the configured budget unreachable.
    ideal.max(1)
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
        let max_buffers = store_opts.max_write_buffer_number.clamp(1, 6);
        let per_cf_write = per_cf_write_buffer_bytes(
            store_opts.write_buffer_total_bytes,
            CF_NAMES.len(),
            max_buffers,
        );
        opts.set_max_total_wal_size(store_opts.max_total_wal_bytes.max(32 * 1024 * 1024) as u64);
        let plan = fd_plan(store_opts.max_open_files, store_opts.fd_reserve);
        if plan.effective < 20 {
            return err(format!(
                "process file limit ({:?}) leaves no room for RocksDB after reserving {} \
                 descriptors; raise `ulimit -n`",
                plan.soft_limit, plan.reserve
            ));
        }
        opts.set_max_open_files(plan.effective);
        // A shared WriteBufferManager enforces the *total* memtable budget
        // instead of relying on per-CF arithmetic that ignored the CF count.
        let write_buffer_manager = WriteBufferManager::new_write_buffer_manager(
            store_opts.write_buffer_total_bytes,
            false,
        );
        opts.set_write_buffer_manager(&write_buffer_manager);
        opts.set_max_background_jobs(store_opts.background_jobs.max(1));
        opts.set_bytes_per_sync(1024 * 1024);
        opts.set_wal_bytes_per_sync(1024 * 1024);
        opts.set_write_buffer_size(per_cf_write);
        opts.set_max_write_buffer_number(max_buffers as i32);
        // Every block writes to most column families in one batch. Flushing them
        // together produces fewer, larger L0 files and much less compaction
        // write amplification than seven independent flushes.
        opts.set_atomic_flush(true);
        let mut descriptors = Vec::new();
        for name in CF_NAMES {
            let mut cf_opts = Options::default();
            cf_opts.set_compression_type(DBCompressionType::Lz4);
            cf_opts.set_write_buffer_size(weighted_write_buffer(
                store_opts.write_buffer_total_bytes,
                name,
                max_buffers,
            ));
            cf_opts.set_max_write_buffer_number(max_buffers as i32);
            cf_opts.set_write_buffer_manager(&write_buffer_manager);
            // Bulky sequential ingest: read compaction input in large chunks and
            // keep level 1 big enough that compaction does not run constantly.
            cf_opts.set_compaction_readahead_size(2 * 1024 * 1024);
            cf_opts.set_max_bytes_for_level_base(512 * 1024 * 1024);
            let mut table = BlockBasedOptions::default();
            table.set_block_cache(&cache);
            table.set_bloom_filter(10.0, true);
            // Index and bloom filters stay in the table readers instead of
            // competing with data blocks in the block cache. When the cache
            // shrinks under pressure, evicting filters would turn every
            // prevout lookup into an extra disk read.
            table.set_cache_index_and_filter_blocks(false);
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
            cache,
            path,
            txids_path,
            txids,
            write_buffer_manager,
            max_write_buffers: max_buffers,
            fd_plan: plan,
            exclude: store_opts.exclude_paths.clone(),
            commit_txids_us: AtomicU64::new(0),
            commit_db_us: AtomicU64::new(0),
        };
        store.check_schema()?;
        store.reconcile_txids()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Change the shared RocksDB block-cache capacity without reopening.
    pub fn set_cache_bytes(&self, bytes: usize) {
        let mut cache = self.cache.clone();
        cache.set_capacity(bytes.max(8 * 1024 * 1024));
    }

    /// Emergency/hot memtable budget. Shrinking can force flush of current
    /// memtables; callers use it only when the OS is already critical or with
    /// an explicit adaptive-memtables opt-in.
    pub fn set_write_buffer_bytes(&self, bytes: usize) -> Result<()> {
        let total = bytes.max(8 * 1024 * 1024);
        self.write_buffer_manager.set_buffer_size(total);
        for name in CF_NAMES {
            let per = weighted_write_buffer(total, name, self.max_write_buffers);
            let value = per.to_string();
            let cf = self.cf(name)?;
            self.db
                .set_options_cf(&cf, &[("write_buffer_size", value.as_str())])?;
        }
        Ok(())
    }

    /// Change mutable DB options at runtime. RocksDB rejects options it
    /// considers immutable; the caller logs and keeps the old value.
    pub fn set_db_option(&self, name: &str, value: &str) -> Result<()> {
        self.db
            .set_options(&[(name, value)])
            .map_err(|e| StoreError(format!("rocksdb set_options {name}: {e}")))
    }

    pub fn set_background_jobs(&self, jobs: usize) -> Result<()> {
        let jobs = jobs.clamp(1, 32);
        self.set_db_option("max_background_jobs", &jobs.to_string())
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
        // `batched_multi_get_cf` processes point reads in batches for the
        // block-based table (one CF, sorted keys). Duplicates are removed so a
        // block spending the same prevout twice does not read it twice.
        let mut keys: Vec<[u8; 36]> = ops.iter().map(codec::utxo_key).collect();
        keys.sort_unstable();
        keys.dedup();
        let values = self.db.batched_multi_get_cf(cf, keys.iter(), true);
        let mut by_key: HashMap<[u8; 36], UtxoValue> = HashMap::with_capacity(keys.len());
        for (key, value) in keys.iter().zip(values) {
            if let Some(raw) = value? {
                by_key.insert(*key, codec::parse_utxo_value(raw.as_ref())?);
            }
        }
        let mut out = HashMap::with_capacity(ops.len());
        for op in ops {
            if let Some(v) = by_key.get(&codec::utxo_key(op)) {
                out.insert(*op, *v);
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
        self.commit_with_sync(batch, tip, txids, true)
    }

    /// Durability policy for bulk catch-up: `txids.dat` is always fsynced
    /// before the tip, so a crash can lose the last RocksDB batch but never
    /// corrupt the index. Startup truncates extra txids to the recovered tip.
    pub fn commit_with_sync(
        &self,
        batch: WriteBatch,
        tip: &IndexedTip,
        txids: &[Hash32],
        sync: bool,
    ) -> Result<()> {
        if txids.len() as u64 > tip.txnum {
            return err(format!(
                "commit has {} txids but tip.txnum is {}",
                txids.len(),
                tip.txnum
            ));
        }
        let start = tip.txnum - txids.len() as u64;
        let txids_started = Instant::now();
        self.append_txids(start, txids)?;
        self.commit_txids_us.store(
            txids_started.elapsed().as_micros() as u64,
            Ordering::Relaxed,
        );

        if failpoints::get() == failpoints::BEFORE_COMMIT {
            return err("failpoint: before_commit (txids appended, db unchanged)");
        }

        let mut batch = batch;
        self.put_tip(&mut batch, tip)?;
        let db_started = Instant::now();
        self.write_batch(batch, sync)?;
        self.commit_db_us
            .store(db_started.elapsed().as_micros() as u64, Ordering::Relaxed);

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

    /// Make every write accepted so far durable in the WAL without forcing
    /// memtables to SST files. A single `WriteBatch` is atomic across column
    /// families and `txids.dat` is fsynced before the batch, so after a
    /// successful `SyncWAL` the committed tip is recoverable. This is the
    /// barrier the prune gate needs; `flush_all` additionally settles SSTs
    /// for size measurements and is not required for durability.
    pub fn sync_wal_barrier(&self) -> Result<()> {
        self.db.flush_wal(true)?;
        Ok(())
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

    /// Sum a property across every column family. Some RocksDB properties are
    /// per-CF (`estimate-*`, `total-sst-files-size`, memtables,
    /// `num-files-at-level0`); reading them through the DB handle only
    /// reports the default family.
    fn sum_cf_property(&self, name: &str) -> (u64, bool) {
        let mut total = 0u64;
        let mut ok = true;
        for cf_name in CF_NAMES {
            match self.cf(cf_name) {
                Ok(cf) => match self.db.property_int_value_cf(cf, name) {
                    Ok(Some(v)) => total = total.saturating_add(v),
                    Ok(None) | Err(_) => ok = false,
                },
                Err(_) => ok = false,
            }
        }
        (total, ok)
    }

    /// Shared block cache usage. Not summed over the families: with a shared
    /// cache every family reports the usage of the whole cache, so the sum
    /// multiplied the real value by seven (the 29.8.6 log showed
    /// `block_cache 18854 MiB` with a 2705 MiB capacity; 18854/7 = 2693).
    /// Read once through the first data family that answers; the empty
    /// default family reports 0, which is why the DB-handle read used to
    /// return 0 while the memory line showed a live cache.
    fn whole_cache_usage(&self) -> (u64, bool) {
        for cf_name in CF_NAMES {
            if let Ok(cf) = self.cf(cf_name) {
                if let Ok(Some(v)) = self
                    .db
                    .property_int_value_cf(cf, "rocksdb.block-cache-usage")
                {
                    return (v, true);
                }
            }
        }
        (0, false)
    }

    /// RocksDB compaction/memory counters used by the indexing benchmark and
    /// `/status`. Property reads are cheap and only sampled at commit time.
    ///
    /// Per-CF properties are summed across all seven families; DB-wide
    /// properties (`num-running-compactions`) are read once so they are not
    /// multiplied by the CF count. `block-cache-usage` is also read once, but
    /// for the opposite reason: with a shared cache **every** column family
    /// reports the usage of the whole cache ("the same for every column
    /// family" in the RocksDB property docs), so summing it multiplied the
    /// real value by seven. It must be read through a data family: the empty
    /// default family reports 0, which is why the DB-handle read always
    /// showed `block_cache 0 MiB` before.
    pub fn compaction_stats(&self) -> CompactionStats {
        let (pending, ok_pending) =
            self.sum_cf_property("rocksdb.estimate-pending-compaction-bytes");
        let (memtables, ok_memtables) = self.sum_cf_property("rocksdb.cur-size-all-mem-tables");
        let (sst, ok_sst) = self.sum_cf_property("rocksdb.total-sst-files-size");
        let (readers, ok_readers) = self.sum_cf_property("rocksdb.estimate-table-readers-mem");
        let (cache, ok_cache) = self.whole_cache_usage();
        let (l0, ok_l0) = self.sum_cf_property("rocksdb.num-files-at-level0");
        let running = self
            .db
            .property_int_value("rocksdb.num-running-compactions")
            .ok()
            .flatten();
        CompactionStats {
            pending_compaction_bytes: pending,
            running_compactions: running.unwrap_or(0),
            memtable_bytes: memtables,
            block_cache_bytes: cache,
            sst_files_bytes: sst,
            table_readers_bytes: readers,
            level0_files: l0,
            properties_ok: ok_pending
                && ok_memtables
                && ok_sst
                && ok_readers
                && ok_cache
                && ok_l0
                && running.is_some(),
        }
    }

    /// Memory held by table indexes and bloom filters. They live outside the
    /// data block cache, so a budget that ignores them is not a real budget.
    /// Summed over every CF; zero means "not measured" when the property is
    /// unavailable.
    pub fn table_readers_bytes(&self) -> u64 {
        self.sum_cf_property("rocksdb.estimate-table-readers-mem").0
    }

    /// Per-column-family pressure for `/status`. The sum hides which family is
    /// causing write stalls; this returns pending compaction, memtable, SST and
    /// table-reader bytes plus a "all properties were read" flag for each CF.
    pub fn cf_stats(&self) -> Vec<(&'static str, u64, u64, u64, u64, bool)> {
        let mut out = Vec::with_capacity(CF_NAMES.len());
        for name in CF_NAMES {
            let mut ok = true;
            let mut read = |prop: &str| -> u64 {
                match self.cf(name) {
                    Ok(cf) => match self.db.property_int_value_cf(cf, prop) {
                        Ok(Some(v)) => v,
                        Ok(None) | Err(_) => {
                            ok = false;
                            0
                        }
                    },
                    Err(_) => {
                        ok = false;
                        0
                    }
                }
            };
            let pending = read("rocksdb.estimate-pending-compaction-bytes");
            let memtable = read("rocksdb.cur-size-all-mem-tables");
            let sst = read("rocksdb.total-sst-files-size");
            let readers = read("rocksdb.estimate-table-readers-mem");
            out.push((name, pending, memtable, sst, readers, ok));
        }
        out
    }

    /// Live memtables as accounted by the shared WriteBufferManager.
    pub fn memtable_budget_usage(&self) -> usize {
        self.write_buffer_manager.get_usage()
    }

    pub fn fd_plan(&self) -> FdPlan {
        self.fd_plan
    }

    /// Paths excluded from `disk_usage_bytes`.
    pub fn excludes(&self) -> &[PathBuf] {
        &self.exclude
    }

    /// Microseconds spent in the last commit's txids append+fsync and in the
    /// RocksDB write. Used by the indexing benchmark to separate fsync cost
    /// from compaction/write stalls.
    pub fn last_commit_timings_us(&self) -> (u64, u64) {
        (
            self.commit_txids_us.load(Ordering::Relaxed),
            self.commit_db_us.load(Ordering::Relaxed),
        )
    }

    /// Logical bytes used by the index, skipping excluded subtrees.
    ///
    /// With the default layout the Core datadir is `<data-dir>/bitcoin`, so a
    /// plain recursive walk counted Core as part of the index and the status
    /// breakdown double-counted it.
    pub fn disk_usage_bytes(&self) -> u64 {
        fn walk(p: &Path, acc: &mut u64, exclude: &[PathBuf]) {
            if exclude.iter().any(|x| p == x || p.starts_with(x)) {
                return;
            }
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    let path = e.path();
                    if exclude.iter().any(|x| path == *x || path.starts_with(x)) {
                        continue;
                    }
                    if let Ok(md) = e.metadata() {
                        if md.is_dir() {
                            walk(&path, acc, exclude);
                        } else {
                            *acc += md.len();
                        }
                    }
                }
            }
        }
        let mut acc = 0;
        walk(&self.path, &mut acc, &self.exclude);
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

    /// Failpoints are process-global; serialize the tests that commit so one
    /// test cannot inject a failure into another running in parallel.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn weighted_memtables_favor_hot_families() {
        let total = 1024 * 1024 * 1024usize;
        let max_buffers = 3;
        let utxo = weighted_write_buffer(total, "utxo", max_buffers);
        let meta = weighted_write_buffer(total, "meta", max_buffers);
        assert!(utxo > meta, "utxo={utxo} meta={meta}");
        let sum: usize = CF_NAMES
            .iter()
            .map(|n| weighted_write_buffer(total, n, max_buffers))
            .sum();
        assert!(sum <= total / max_buffers + CF_NAMES.len());
    }

    #[test]
    fn write_buffer_budget_is_split_across_column_families() {
        // 1 GiB total over 7 CFs and 3 memtables = ~48 MiB per CF.
        let per_cf = per_cf_write_buffer_bytes(1024 * 1024 * 1024, 7, 3);
        assert_eq!(per_cf, 1024 * 1024 * 1024 / 21);
        // A small budget must never be padded above what was requested:
        // 64 MiB / 21 slots is ~3 MiB, not the old 8 MiB floor (168 MiB).
        let small = per_cf_write_buffer_bytes(64 * 1024 * 1024, 7, 3);
        assert_eq!(small, 64 * 1024 * 1024 / 21);
        assert!(small * 21 <= 64 * 1024 * 1024);
        // The shared WriteBufferManager is the real cap; large per-CF
        // thresholds are allowed so the configured total is reachable.
        assert_eq!(
            per_cf_write_buffer_bytes(64 * 1024 * 1024 * 1024, 7, 3),
            64 * 1024 * 1024 * 1024 / 21
        );
    }

    #[test]
    fn fd_plan_respects_the_process_limit() {
        // Unlimited: keep the request.
        let plan = fd_plan_with(8192, 320, None);
        assert_eq!(plan.effective, 8192);
        assert!(!plan.capped);
        // A 1024-soft-limit container reserves 320 and keeps most of the gain.
        let plan = fd_plan_with(8192, 320, Some(1024));
        assert_eq!(plan.effective, 704);
        assert!(plan.capped);
        assert!(plan.below_recommended());
        assert!(plan.effective as u64 + plan.reserve <= 1024);
        // A generous limit keeps the full request.
        let plan = fd_plan_with(8192, 320, Some(16384));
        assert_eq!(plan.effective, 8192);
        assert!(!plan.capped);
        // A tiny limit cannot be "fixed" by a floor that exceeds it.
        let tiny = fd_plan_with(8192, 320, Some(64));
        assert!(
            tiny.effective < 20,
            "a 64-descriptor limit cannot fit RocksDB"
        );
        assert!(tiny.capped);
        // RLIM_INFINITY must not wrap into a negative number and collapse to 256.
        let inf = fd_plan_with(8192, 320, Some(u64::MAX));
        assert_eq!(inf.effective, 8192);
    }

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
    fn hot_memtable_resize_is_applied() {
        let _guard = serial();
        let dir = tmp("memtable-resize");
        let s = Store::open(&dir).unwrap();
        s.set_write_buffer_bytes(512 * 1024 * 1024).unwrap();
        s.set_write_buffer_bytes(256 * 1024 * 1024).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compaction_properties_are_available_across_column_families() {
        let _guard = serial();
        let dir = tmp("stats");
        let s = Store::open(&dir).unwrap();
        let stats = s.compaction_stats();
        // Every per-CF property must be supported; otherwise a zero would be
        // indistinguishable from an empty database.
        assert!(stats.properties_ok, "{stats:?}");
        let cf = s.cf_stats();
        assert_eq!(cf.len(), CF_NAMES.len());
        assert!(cf.iter().all(|row| row.5), "{cf:?}");
    }

    #[test]
    fn batched_utxo_lookup_deduplicates_and_preserves_values() {
        let _guard = serial();
        let dir = tmp("batched-utxo");
        let s = Store::open(&dir).unwrap();
        let op = Outpoint {
            txid: Hash32::from_hex(&"44".repeat(32)).unwrap(),
            vout: 7,
        };
        let val = UtxoValue {
            value_sat: 12_345,
            script_hash: ScriptHash::from_script(b"\x52"),
            creation_height: 9,
            coinbase: false,
        };
        let mut b = Store::batch();
        s.put_utxo(&mut b, &op, &val).unwrap();
        s.write_batch(b, true).unwrap();
        // The same outpoint appears twice in one block; the batch must read it
        // once and return one entry.
        let map = s.get_utxos(&[op, op]).unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map[&op], val);
    }

    #[test]
    fn reopen_keeps_tip_and_truncates_extra_txids() {
        let _guard = serial();
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
        let _guard = serial();
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
        let _guard = serial();
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
        let _guard = serial();
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
        let _guard = serial();
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
        let _guard = serial();
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
