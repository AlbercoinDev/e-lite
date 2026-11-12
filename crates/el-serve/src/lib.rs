//! Electrum JSON-RPC 1.4 server and local metrics.
//!
//! Each request reads the index through a shared read lock; the indexer commits
//! under the write lock, so no response mixes two tips.

#![recursion_limit = "256"]

use el_chain::ReaderTracker;
use el_mempool::Mempool;
use el_model::{merkle_branch, BlockRecord, Hash32, NetworkIdentity, Outpoint, ScriptHash};
use el_rawtx::Resolver;
use el_rpc::CoreClient;
use el_store::Store;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock, RwLockReadGuard};
use std::time::{Duration, Instant, SystemTime};

mod version;
pub use version::{
    ProtocolVersion, PRE_1_6_VERSIONS, SERVER_MAX, SERVER_MIN, SERVER_VERSIONS, V1_6, V1_7,
};

/// Electrum clients ask for a script's full history. A script with millions of
/// entries would hold the index lock for too long; keep a generous cap that
/// still protects the server from pathological scripts.
pub const MAX_HISTORY_ENTRIES: usize = 100_000;

pub const PROTOCOL_MIN: &str = "1.4";
pub const PROTOCOL_MAX: &str = "1.7";
/// Fee-estimate cache key: (confirmation target, Core estimate mode).
type FeeEstimateKey = (u32, Option<String>);
type FeeEstimateCache = HashMap<FeeEstimateKey, (f64, Instant)>;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const MAX_HEADERS_CHUNK: usize = 2016;
/// Hard limit for a single Electrum JSON-RPC line. Prevents an unauthenticated
/// client from growing the per-connection buffer without bound.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// Idle connections are polled at this interval so a TLS connection can be
/// shared between the request reader and the notifier writer without splitting
/// rustls' connection state. It also bounds notification latency.
const CLIENT_READ_POLL: Duration = Duration::from_millis(250);
/// A client that sends nothing for this long is disconnected.
const CLIENT_IDLE_TIMEOUT: Duration = Duration::from_secs(600);
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// A TLS client gets this long to finish the handshake.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the TLS listener checks the configured PEM files for renewal.
pub const TLS_CERT_CHECK_INTERVAL: Duration = Duration::from_secs(60);

pub mod state {
    pub const BUILDING: u8 = 0;
    pub const READY: u8 = 1;
    pub const DEGRADED: u8 = 2;
    pub const REORG: u8 = 3;
    pub const FATAL: u8 = 4;
    pub const CORE_UNAVAILABLE: u8 = 5;
    pub const PAUSE_DOWNLOAD: u8 = 6;
    pub const PRUNING: u8 = 7;
    pub const WAITING_FOR_CORE: u8 = 8;
    pub const INITIALIZING: u8 = 9;

    pub fn name(v: u8) -> &'static str {
        match v {
            BUILDING => "BUILDING_INDEX",
            READY => "READY",
            DEGRADED => "DEGRADED_RAWTX",
            REORG => "REORG_RECOVERY",
            FATAL => "FATAL_INTEGRITY",
            CORE_UNAVAILABLE => "CORE_UNAVAILABLE",
            PAUSE_DOWNLOAD => "PAUSE_DOWNLOAD",
            PRUNING => "PRUNING",
            WAITING_FOR_CORE => "WAITING_FOR_CORE",
            INITIALIZING => "INITIALIZING",
            _ => "UNKNOWN",
        }
    }
}

pub struct Stats {
    pub indexed_height: AtomicU64,
    pub core_height: AtomicU64,
    pub core_headers: AtomicU64,
    pub core_verification_bps: AtomicU64,
    pub core_pruneheight: AtomicU64,
    pub core_pruned: AtomicBool,
    pub core_network_active: AtomicBool,
    pub core_ibd: AtomicBool,
    pub core_connections: AtomicU64,
    pub txnum: AtomicU64,
    pub requests_total: AtomicU64,
    pub errors_total: AtomicU64,
    pub rate_limited: AtomicU64,
    pub clients: AtomicUsize,
    pub last_block_ms: AtomicU64,
    pub notifier_runs: AtomicU64,
    pub notifications_sent: AtomicU64,
    pub notifier_errors: AtomicU64,
    pub blocks_applied: AtomicU64,
    /// Commit groups completed and transactions committed (cumulative).
    pub commits: AtomicU64,
    pub committed_txs: AtomicU64,
    /// Cumulative milliseconds spent in each indexing stage.
    pub stage_read_us: AtomicU64,
    pub stage_decode_us: AtomicU64,
    pub stage_prefetch_us: AtomicU64,
    pub stage_wait_us: AtomicU64,
    pub stage_apply_us: AtomicU64,
    pub stage_commit_us: AtomicU64,
    pub commit_txids_us: AtomicU64,
    pub commit_db_us: AtomicU64,
    /// RocksDB state sampled at commit time (benchmark/diagnostics).
    pub rocksdb_pending_compaction_bytes: AtomicU64,
    pub rocksdb_running_compactions: AtomicU64,
    pub rocksdb_memtable_bytes: AtomicU64,
    pub rocksdb_block_cache_bytes: AtomicU64,
    pub rocksdb_sst_bytes: AtomicU64,
    pub rocksdb_table_readers_bytes: AtomicU64,
    /// Summed level-0 file count across column families (flush/compaction
    /// stall signal). Sampled at commit time.
    pub rocksdb_level0_files: AtomicU64,
    pub rocksdb_properties_ok: AtomicBool,
    /// Descriptor budget: requested/effective max_open_files and the process
    /// soft limit. Zero means "not measured".
    pub fd_soft_limit: AtomicU64,
    pub fd_reserve: AtomicU64,
    pub max_open_files_requested: AtomicU64,
    pub max_open_files_effective: AtomicU64,
    pub prune_requests: AtomicU64,
    pub prune_failures: AtomicU64,
    pub disk_low: AtomicBool,
    pub tuning_readers: AtomicU64,
    pub tuning_commit_ms: AtomicU64,
    pub tuning_batch_blocks: AtomicU64,
    pub tuning_cache_mib: AtomicU64,
    pub tuning_read_ms: AtomicU64,
    pub tuning_apply_ms: AtomicU64,
    pub tuning_decode_ms: AtomicU64,
    pub tuning_wait_ms: AtomicU64,
    pub tuning_prefetch_p50_ms: AtomicU64,
    pub tuning_prefetch_p95_ms: AtomicU64,
    pub tuning_prefetch_max_ms: AtomicU64,
    pub tuning_wait_p50_ms: AtomicU64,
    pub tuning_wait_p95_ms: AtomicU64,
    pub tuning_commit_ms_ewma: AtomicU64,
    pub tuning_commit_per_block_ms: AtomicU64,
    /// Wall-clock between the start of consecutive commits. `commit_share`
    /// must divide by this, not by the configured interval.
    pub tuning_commit_cycle_ms: AtomicU64,
    /// Last full commit group: duration, cycle, blocks, reason and cost split.
    pub tuning_commit_last_ms: AtomicU64,
    pub tuning_commit_last_cycle_ms: AtomicU64,
    pub tuning_commit_last_blocks: AtomicU64,
    pub tuning_commit_last_txs: AtomicU64,
    pub tuning_commit_last_bytes: AtomicU64,
    pub tuning_commit_last_txids_us: AtomicU64,
    pub tuning_commit_last_db_us: AtomicU64,
    pub tuning_commit_last_reason: AtomicU64,
    pub tuning_adapt_steps: AtomicU64,
    pub tuning_tx_per_sec: AtomicU64,
    pub tuning_blocks_per_sec: AtomicU64,
    pub tuning_profile: AtomicU64,
    pub tuning_adaptive: AtomicBool,
    pub tuning_background_jobs: AtomicU64,
    pub tls_handshakes: AtomicU64,
    pub tls_handshake_failures: AtomicU64,
    pub mem_total_mib: AtomicU64,
    pub mem_available_mib: AtomicU64,
    pub mem_cgroup_limit_mib: AtomicU64,
    pub mem_cgroup_used_mib: AtomicU64,
    pub mem_reclaimable_mib: AtomicU64,
    pub mem_cached_mib: AtomicU64,
    pub mem_cache_max_mib: AtomicU64,
    pub mem_memtables_mib: AtomicU64,
    pub mem_memtable_bytes_mib: AtomicU64,
    pub mem_table_readers_mib: AtomicU64,
    pub mem_effective_mib: AtomicU64,
    pub mem_dynamic_reserve_mib: AtomicU64,
    pub mem_reserve_mib: AtomicU64,
    pub mem_budget_mib: AtomicU64,
    pub tor_bootstrapped: AtomicBool,
    /// Managed Tor: effective loopback SOCKS port (0 = unknown/not managed)
    /// and whether a control port exists (e-lite does not create one).
    pub tor_managed: AtomicBool,
    pub tor_socks_port: AtomicU64,
    pub tor_control_port: AtomicU64,
    /// Pipeline work accounting: blocks prepared, applied and discarded when
    /// a catch-up call ends. Discarded should stay near zero.
    pub pipeline_prepared: AtomicU64,
    pub pipeline_applied: AtomicU64,
    pub pipeline_discarded: AtomicU64,
    /// Core/e-lite scheduler: backlog bytes and high/low watermarks.
    pub scheduler_alternate: AtomicBool,
    pub scheduler_index_paused: AtomicBool,
    pub scheduler_backlog_bytes: AtomicU64,
    pub scheduler_high_bytes: AtomicU64,
    pub scheduler_low_bytes: AtomicU64,
    pub scheduler_avg_block_bytes: AtomicU64,
    pub scheduler_phase: AtomicU64,
    pub scheduler_phase_blocks: AtomicU64,
    pub scheduler_phase_elapsed_ms: AtomicU64,
    /// Machine plan detected at startup (CPU/RAM/disk -> knobs).
    pub plan: Mutex<serde_json::Value>,
    /// Per-column-family RocksDB pressure, published at commit time.
    pub rocksdb_cf: Mutex<serde_json::Value>,
    pub started: Instant,
}

impl Default for Stats {
    fn default() -> Self {
        Stats {
            indexed_height: AtomicU64::new(0),
            core_height: AtomicU64::new(0),
            core_headers: AtomicU64::new(0),
            core_verification_bps: AtomicU64::new(0),
            core_pruneheight: AtomicU64::new(0),
            core_pruned: AtomicBool::new(false),
            core_network_active: AtomicBool::new(true),
            core_ibd: AtomicBool::new(false),
            core_connections: AtomicU64::new(0),
            txnum: AtomicU64::new(0),
            requests_total: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            rate_limited: AtomicU64::new(0),
            clients: AtomicUsize::new(0),
            last_block_ms: AtomicU64::new(0),
            notifier_runs: AtomicU64::new(0),
            notifications_sent: AtomicU64::new(0),
            notifier_errors: AtomicU64::new(0),
            blocks_applied: AtomicU64::new(0),
            commits: AtomicU64::new(0),
            committed_txs: AtomicU64::new(0),
            stage_read_us: AtomicU64::new(0),
            stage_decode_us: AtomicU64::new(0),
            stage_prefetch_us: AtomicU64::new(0),
            stage_wait_us: AtomicU64::new(0),
            stage_apply_us: AtomicU64::new(0),
            stage_commit_us: AtomicU64::new(0),
            commit_txids_us: AtomicU64::new(0),
            commit_db_us: AtomicU64::new(0),
            rocksdb_pending_compaction_bytes: AtomicU64::new(0),
            rocksdb_running_compactions: AtomicU64::new(0),
            rocksdb_memtable_bytes: AtomicU64::new(0),
            rocksdb_block_cache_bytes: AtomicU64::new(0),
            rocksdb_sst_bytes: AtomicU64::new(0),
            rocksdb_table_readers_bytes: AtomicU64::new(0),
            rocksdb_level0_files: AtomicU64::new(0),
            rocksdb_properties_ok: AtomicBool::new(false),
            fd_soft_limit: AtomicU64::new(0),
            fd_reserve: AtomicU64::new(0),
            max_open_files_requested: AtomicU64::new(0),
            max_open_files_effective: AtomicU64::new(0),
            prune_requests: AtomicU64::new(0),
            prune_failures: AtomicU64::new(0),
            disk_low: AtomicBool::new(false),
            tuning_readers: AtomicU64::new(0),
            tuning_commit_ms: AtomicU64::new(0),
            tuning_batch_blocks: AtomicU64::new(0),
            tuning_cache_mib: AtomicU64::new(0),
            tuning_read_ms: AtomicU64::new(0),
            tuning_apply_ms: AtomicU64::new(0),
            tuning_decode_ms: AtomicU64::new(0),
            tuning_wait_ms: AtomicU64::new(0),
            tuning_prefetch_p50_ms: AtomicU64::new(0),
            tuning_prefetch_p95_ms: AtomicU64::new(0),
            tuning_prefetch_max_ms: AtomicU64::new(0),
            tuning_wait_p50_ms: AtomicU64::new(0),
            tuning_wait_p95_ms: AtomicU64::new(0),
            tuning_commit_ms_ewma: AtomicU64::new(0),
            tuning_commit_per_block_ms: AtomicU64::new(0),
            tuning_commit_cycle_ms: AtomicU64::new(0),
            tuning_commit_last_ms: AtomicU64::new(0),
            tuning_commit_last_cycle_ms: AtomicU64::new(0),
            tuning_commit_last_blocks: AtomicU64::new(0),
            tuning_commit_last_txs: AtomicU64::new(0),
            tuning_commit_last_bytes: AtomicU64::new(0),
            tuning_commit_last_txids_us: AtomicU64::new(0),
            tuning_commit_last_db_us: AtomicU64::new(0),
            tuning_commit_last_reason: AtomicU64::new(0),
            tuning_adapt_steps: AtomicU64::new(0),
            tuning_tx_per_sec: AtomicU64::new(0),
            tuning_blocks_per_sec: AtomicU64::new(0),
            tuning_profile: AtomicU64::new(0),
            tuning_adaptive: AtomicBool::new(false),
            tuning_background_jobs: AtomicU64::new(0),
            tls_handshakes: AtomicU64::new(0),
            tls_handshake_failures: AtomicU64::new(0),
            mem_total_mib: AtomicU64::new(0),
            mem_available_mib: AtomicU64::new(0),
            mem_cgroup_limit_mib: AtomicU64::new(0),
            mem_cgroup_used_mib: AtomicU64::new(0),
            mem_reclaimable_mib: AtomicU64::new(0),
            mem_cached_mib: AtomicU64::new(0),
            mem_cache_max_mib: AtomicU64::new(0),
            mem_memtables_mib: AtomicU64::new(0),
            mem_memtable_bytes_mib: AtomicU64::new(0),
            mem_table_readers_mib: AtomicU64::new(0),
            mem_effective_mib: AtomicU64::new(0),
            mem_dynamic_reserve_mib: AtomicU64::new(0),
            mem_reserve_mib: AtomicU64::new(0),
            mem_budget_mib: AtomicU64::new(0),
            tor_bootstrapped: AtomicBool::new(false),
            tor_managed: AtomicBool::new(false),
            tor_socks_port: AtomicU64::new(0),
            tor_control_port: AtomicU64::new(0),
            pipeline_prepared: AtomicU64::new(0),
            pipeline_applied: AtomicU64::new(0),
            pipeline_discarded: AtomicU64::new(0),
            scheduler_alternate: AtomicBool::new(false),
            scheduler_index_paused: AtomicBool::new(false),
            scheduler_backlog_bytes: AtomicU64::new(0),
            scheduler_high_bytes: AtomicU64::new(0),
            scheduler_low_bytes: AtomicU64::new(0),
            scheduler_avg_block_bytes: AtomicU64::new(0),
            scheduler_phase: AtomicU64::new(0),
            scheduler_phase_blocks: AtomicU64::new(0),
            scheduler_phase_elapsed_ms: AtomicU64::new(0),
            plan: Mutex::new(serde_json::json!({})),
            rocksdb_cf: Mutex::new(serde_json::json!({})),
            started: Instant::now(),
        }
    }
}

/// Cached header hashes for checkpoint proofs. Header hashes are immutable
/// except across a reorg, which invalidates the cache from the fork point.
#[derive(Default)]
pub struct HeaderCache {
    hashes: Mutex<Vec<Hash32>>,
}

impl HeaderCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn invalidate_from(&self, height: u32) {
        if let Ok(mut v) = self.hashes.lock() {
            v.truncate(height as usize);
        }
    }

    pub fn chain(&self, store: &Store, cp_height: u32) -> Result<Vec<Hash32>, String> {
        let need = cp_height as usize + 1;
        let mut v = self
            .hashes
            .lock()
            .map_err(|_| "header cache lock poisoned".to_string())?;
        while v.len() < need {
            let h = v.len() as u32;
            let rec = store
                .get_block(h)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("height {h} not indexed"))?;
            v.push(rec.hash);
        }
        Ok(v[..need].to_vec())
    }
}

#[derive(Default)]
pub struct DbBytesCache {
    bytes: u64,
    at: Option<Instant>,
}

/// Scripts whose status changed since the notifier last ran. The indexer marks
/// confirmed changes and the mempool marks overlay changes, so the notifier
/// only recomputes the affected subscriptions.
#[derive(Default)]
pub struct DirtyScripts {
    inner: Mutex<HashSet<ScriptHash>>,
    force_all: AtomicBool,
}

impl DirtyScripts {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mark(&self, script: ScriptHash) {
        if let Ok(mut set) = self.inner.lock() {
            set.insert(script);
        }
    }

    pub fn mark_all(&self, scripts: impl IntoIterator<Item = ScriptHash>) {
        if let Ok(mut set) = self.inner.lock() {
            set.extend(scripts);
        }
    }

    /// A reorg can change scripts that are not in the replayed blocks' dirty
    /// set yet (for example, while the index is between rewind and replay).
    pub fn mark_all_subscriptions(&self) {
        self.force_all.store(true, Ordering::SeqCst);
    }

    pub fn take(&self) -> (HashSet<ScriptHash>, bool) {
        let force = self.force_all.swap(false, Ordering::SeqCst);
        let scripts = self
            .inner
            .lock()
            .map(|mut set| std::mem::take(&mut *set))
            .unwrap_or_default();
        (scripts, force)
    }
}

/// Transport for one Electrum client. Plain TCP and TLS share one read/write
/// path so the notifier can write to a TLS connection without splitting
/// rustls' connection state across threads.
pub enum Wire {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ServerConnection, TcpStream>>),
}

impl Wire {
    fn plain(stream: TcpStream) -> std::io::Result<Self> {
        configure_socket(&stream)?;
        Ok(Wire::Plain(stream))
    }

    fn tls(config: Arc<ServerConfig>, stream: TcpStream) -> std::io::Result<Self> {
        configure_socket(&stream)?;
        let conn = ServerConnection::new(config)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        Ok(Wire::Tls(Box::new(StreamOwned::new(conn, stream))))
    }

    pub fn is_tls(&self) -> bool {
        matches!(self, Wire::Tls(_))
    }

    /// Complete the TLS handshake with a bounded deadline. Plain connections
    /// return immediately.
    fn handshake(&mut self) -> std::io::Result<()> {
        let Wire::Tls(stream) = self else {
            return Ok(());
        };
        let deadline = Instant::now() + TLS_HANDSHAKE_TIMEOUT;
        while stream.conn.is_handshaking() {
            match stream.conn.complete_io(&mut stream.sock) {
                Ok((0, 0)) => std::thread::sleep(Duration::from_millis(5)),
                Ok(_) => {}
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => return Err(e),
            }
            if Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "TLS handshake timeout",
                ));
            }
        }
        Ok(())
    }
}

fn configure_socket(stream: &TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(CLIENT_READ_POLL))?;
    stream.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT))?;
    let _ = stream.set_nodelay(true);
    Ok(())
}

impl Read for Wire {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Wire::Plain(s) => s.read(buf),
            Wire::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Wire {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Wire::Plain(s) => s.write(buf),
            Wire::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Wire::Plain(s) => s.flush(),
            Wire::Tls(s) => s.flush(),
        }
    }
}

/// Build a rustls server configuration from PEM files. Any certificate chain
/// works, including a public CA certificate reused from a web server.
pub fn build_tls_config(cert: &Path, key: &Path) -> Result<ServerConfig, String> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .map_err(|e| format!("cannot read TLS certificate {}: {e}", cert.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("invalid TLS certificate {}: {e}", cert.display()))?;
    if certs.is_empty() {
        return Err(format!(
            "TLS certificate {} contains no certificates",
            cert.display()
        ));
    }
    let key_der = PrivateKeyDer::from_pem_file(key)
        .map_err(|e| format!("cannot read TLS private key {}: {e}", key.display()))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS configuration error: {e}"))?
        .with_no_client_auth()
        .with_single_cert(certs, key_der)
        .map_err(|e| format!("invalid TLS certificate/key: {e}"))
}

/// Reloads the TLS certificate when the PEM files change on disk. This lets a
/// Let's Encrypt renewal take effect without restarting the indexer.
pub struct CertWatcher {
    cert: PathBuf,
    key: PathBuf,
    interval: Duration,
    state: Mutex<CertState>,
}

struct CertState {
    config: Arc<ServerConfig>,
    cert_mtime: Option<SystemTime>,
    key_mtime: Option<SystemTime>,
    checked: Instant,
    error_logged: bool,
}

impl CertWatcher {
    pub fn load(cert: PathBuf, key: PathBuf, interval: Duration) -> Result<Self, String> {
        let config = build_tls_config(&cert, &key)?;
        let cert_mtime = file_mtime(&cert);
        let key_mtime = file_mtime(&key);
        Ok(Self {
            cert,
            key,
            interval,
            state: Mutex::new(CertState {
                config: Arc::new(config),
                cert_mtime,
                key_mtime,
                checked: Instant::now(),
                error_logged: false,
            }),
        })
    }

    /// Current server configuration, reloading it when the files changed.
    /// A failed reload keeps the previous certificate in use.
    pub fn config(&self) -> Arc<ServerConfig> {
        let mut state = match self.state.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        if state.checked.elapsed() >= self.interval {
            state.checked = Instant::now();
            let cert_mtime = file_mtime(&self.cert);
            let key_mtime = file_mtime(&self.key);
            if cert_mtime != state.cert_mtime || key_mtime != state.key_mtime {
                match build_tls_config(&self.cert, &self.key) {
                    Ok(config) => {
                        state.config = Arc::new(config);
                        state.cert_mtime = cert_mtime;
                        state.key_mtime = key_mtime;
                        state.error_logged = false;
                        eprintln!(
                            "[e-lite] TLS certificate reloaded from {}",
                            self.cert.display()
                        );
                    }
                    Err(e) => {
                        if !state.error_logged {
                            eprintln!(
                                "[e-lite] WARNING: TLS certificate reload failed ({e}); \
                                 keeping the previous certificate"
                            );
                            state.error_logged = true;
                        }
                    }
                }
            }
        }
        Arc::clone(&state.config)
    }
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

#[derive(Debug)]
pub struct RateLimit {
    tokens: f64,
    capacity: f64,
    refill_per_sec: f64,
    last: Instant,
}

impl RateLimit {
    fn new(capacity: u64, refill_per_sec: u64) -> Self {
        let capacity = capacity.max(1) as f64;
        Self {
            tokens: capacity,
            capacity,
            refill_per_sec: refill_per_sec as f64,
            last: Instant::now(),
        }
    }

    /// Consume one token. When the bucket is empty, wait for the next token
    /// instead of rejecting the request: an initial wallet scan must never be
    /// aborted by a burst. Returns `true` when the request had to wait.
    ///
    /// A zero refill rate disables the limiter.
    fn acquire(&mut self, now: Instant) -> bool {
        if self.refill_per_sec <= 0.0 {
            return false;
        }
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            return false;
        }
        let wait = (1.0 - self.tokens) / self.refill_per_sec;
        if wait > 0.0 {
            std::thread::sleep(Duration::from_secs_f64(wait));
        }
        // The sleep produced exactly the token consumed here.
        self.tokens = 0.0;
        self.last = Instant::now();
        true
    }
}

#[derive(Clone)]
pub struct OutpointSub {
    pub script: ScriptHash,
    pub status: Value,
}

pub struct ClientState {
    pub id: u64,
    pub writer: Mutex<Wire>,
    pub subs: Mutex<HashMap<ScriptHash, Option<String>>>,
    /// Scripts subscribed through `blockchain.scriptpubkey.subscribe`, with
    /// their last status. Kept separate from scripthash subscriptions so a
    /// client using both receives the correct method-specific notification.
    pub spk_subs: Mutex<HashMap<ScriptHash, Option<String>>>,
    /// Outpoint subscriptions from `blockchain.outpoint.subscribe`.
    pub outpoints: Mutex<HashMap<(Hash32, u32), OutpointSub>>,
    /// Per-connection request budget (rate limiting).
    pub budget: Mutex<RateLimit>,
    pub headers: AtomicBool,
    pub last_height: AtomicU64,
    pub alive: AtomicBool,
    pub tls: bool,
    /// Protocol version negotiated by the first `server.version` call.
    pub protocol: Mutex<Option<ProtocolVersion>>,
    /// True once any message other than `server.version` has been seen. 1.6+
    /// requires version negotiation to be the first message; older traffic
    /// caps the connection at 1.4.2.
    pub saw_message: AtomicBool,
}

impl ClientState {
    pub fn send(&self, value: &Value) -> bool {
        if !self.alive.load(Ordering::Relaxed) {
            return false;
        }
        let mut bytes = match serde_json::to_vec(value) {
            Ok(b) => b,
            Err(_) => return false,
        };
        bytes.push(b'\n');
        let mut writer = match self.writer.lock() {
            Ok(w) => w,
            Err(_) => return false,
        };
        if writer
            .write_all(&bytes)
            .and_then(|_| writer.flush())
            .is_err()
        {
            self.alive.store(false, Ordering::SeqCst);
            return false;
        }
        true
    }
}

#[derive(Default)]
pub struct ClientRegistry {
    next: AtomicU64,
    clients: Mutex<HashMap<u64, Arc<ClientState>>>,
    rate_limit_rps: AtomicU64,
    rate_burst: AtomicU64,
}

impl ClientRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure the per-connection budget. A zero rate disables it.
    pub fn set_rate_limit(&self, requests_per_sec: u64, burst: u64) {
        self.rate_limit_rps
            .store(requests_per_sec, Ordering::Relaxed);
        self.rate_burst.store(burst.max(1), Ordering::Relaxed);
    }

    pub fn rate_limit(&self) -> (u64, u64) {
        (
            self.rate_limit_rps.load(Ordering::Relaxed),
            self.rate_burst.load(Ordering::Relaxed),
        )
    }

    pub fn register(&self, wire: Wire) -> Arc<ClientState> {
        let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        let tls = wire.is_tls();
        let state = Arc::new(ClientState {
            id,
            writer: Mutex::new(wire),
            subs: Mutex::new(HashMap::new()),
            spk_subs: Mutex::new(HashMap::new()),
            outpoints: Mutex::new(HashMap::new()),
            budget: Mutex::new(RateLimit::new(
                self.rate_burst.load(Ordering::Relaxed),
                self.rate_limit_rps.load(Ordering::Relaxed),
            )),
            headers: AtomicBool::new(false),
            last_height: AtomicU64::new(0),
            alive: AtomicBool::new(true),
            tls,
            protocol: Mutex::new(None),
            saw_message: AtomicBool::new(false),
        });
        if let Ok(mut c) = self.clients.lock() {
            c.insert(id, Arc::clone(&state));
        }
        state
    }

    pub fn unregister(&self, id: u64) {
        if let Ok(mut c) = self.clients.lock() {
            if let Some(state) = c.remove(&id) {
                state.alive.store(false, Ordering::SeqCst);
            }
        }
    }

    pub fn snapshot(&self) -> Vec<Arc<ClientState>> {
        self.clients
            .lock()
            .map(|c| c.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.clients.lock().map(|c| c.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Background block prefetcher for confirmed history entries.
///
/// When a wallet asks for a script history, e-lite already knows the heights
/// (and one txid per height). Fetching those blocks in the background means the
/// subsequent `blockchain.transaction.get` calls hit the block cache instead of
/// waiting for a provider. Deduplicated by height, bounded queue, and only used
/// when external sources exist.
pub struct Prefetcher {
    tx: Option<mpsc::SyncSender<(Hash32, Hash32, u32)>>,
    seen: Arc<Mutex<HashSet<Hash32>>>,
    queued: Arc<AtomicU64>,
    done: Arc<AtomicU64>,
}

impl Prefetcher {
    pub fn disabled() -> Self {
        Self {
            tx: None,
            seen: Arc::new(Mutex::new(HashSet::new())),
            queued: Arc::new(AtomicU64::new(0)),
            done: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn new(
        resolver: Arc<Resolver>,
        _readers: Arc<ReaderTracker>,
        workers: usize,
        queue: usize,
    ) -> Self {
        let (tx, rx) = mpsc::sync_channel::<(Hash32, Hash32, u32)>(queue.max(1));
        let rx = Arc::new(Mutex::new(rx));
        let done = Arc::new(AtomicU64::new(0));
        let seen = Arc::new(Mutex::new(HashSet::new()));
        for _ in 0..workers.max(1) {
            let resolver = Arc::clone(&resolver);
            let rx = Arc::clone(&rx);
            let done = Arc::clone(&done);
            let seen = Arc::clone(&seen);
            std::thread::spawn(move || loop {
                let item = {
                    let guard = match rx.lock() {
                        Ok(g) => g,
                        Err(_) => return,
                    };
                    guard.recv()
                };
                let Ok((block_hash, txid, height)) = item else {
                    return;
                };
                // No reader guard: prefetch is an optimisation and may resolve
                // from external sources. It must never block pruning.
                if resolver.raw_tx_prefetch(txid, height).is_err() {
                    // Allow a later history request to retry this block.
                    if let Ok(mut seen) = seen.lock() {
                        seen.remove(&block_hash);
                    }
                }
                done.fetch_add(1, Ordering::Relaxed);
            });
        }
        Self {
            tx: Some(tx),
            seen: Arc::clone(&seen),
            queued: Arc::new(AtomicU64::new(0)),
            done,
        }
    }

    /// (queued, completed) background prefetches.
    pub fn stats(&self) -> (u64, u64) {
        (
            self.queued.load(Ordering::Relaxed),
            self.done.load(Ordering::Relaxed),
        )
    }

    /// Enqueue one txid per block height. Duplicates and a full queue are
    /// dropped silently: prefetch is an optimisation, never a correctness path.
    pub fn enqueue(&self, block_hash: Hash32, txid: Hash32, height: u32) -> bool {
        let Some(tx) = &self.tx else {
            return false;
        };
        let Ok(mut seen) = self.seen.lock() else {
            return false;
        };
        if seen.len() >= 20_000 {
            seen.clear();
        }
        if !seen.insert(block_hash) {
            return false;
        }
        if tx.try_send((block_hash, txid, height)).is_err() {
            // Queue full or worker gone: do not mark it as done.
            seen.remove(&block_hash);
            return false;
        }
        self.queued.fetch_add(1, Ordering::Relaxed);
        true
    }
}

pub struct Shared {
    pub store: Arc<Store>,
    pub core: CoreClient,
    pub resolver: Arc<Resolver>,
    /// Effective public Esplora bases for this network (from public-servers.toml
    /// or the [rawtx] override). Empty means local data only.
    pub public_servers: Vec<String>,
    /// Startup probe results, one JSON object per provider.
    pub provider_status: Mutex<Vec<Value>>,
    pub provider_ready: AtomicBool,
    pub tor_ready: AtomicBool,
    pub clearnet_fallback: AtomicBool,
    pub prefetch: Prefetcher,
    pub mempool: Arc<RwLock<Mempool>>,
    pub relayfee: Mutex<f64>,
    pub fee_estimates: Mutex<FeeEstimateCache>,
    pub error_log: Mutex<HashMap<String, Instant>>,
    pub clients: Arc<ClientRegistry>,
    pub readers: Arc<ReaderTracker>,
    pub silent_payments_enabled: bool,
    pub identity: RwLock<NetworkIdentity>,
    pub headers: HeaderCache,
    pub db_bytes: Mutex<DbBytesCache>,
    pub dirty_scripts: DirtyScripts,
    pub data_dir: std::path::PathBuf,
    pub core_datadir: std::path::PathBuf,
    pub log_file: PathBuf,
    pub listen: String,
    pub tls_listen: Option<String>,
    pub onion: Mutex<Option<String>>,
    pub onion_plain_port: u16,
    pub onion_tls_port: u16,
    pub banner_file: Option<PathBuf>,
    pub max_clients: usize,
    pub state: AtomicU8,
    pub shutdown: Arc<AtomicBool>,
    pub lock: Arc<RwLock<()>>,
    pub stats: Stats,
}

/// Coherent readiness view shared by the banner, `/status` and logs.
#[derive(Debug, Clone, Copy, Default)]
pub struct SyncStatus {
    pub ready: bool,
    pub at_core_tip: bool,
    pub fully_synced: bool,
    pub offline: bool,
    pub height: u64,
    pub headers: u64,
    pub core_height: u64,
    pub pruneheight: u64,
    pub core_ibd: bool,
    pub verification: f64,
    pub lag: u64,
    pub percent: f64,
    pub core_percent: f64,
    pub blocks_per_sec: u64,
}

impl Shared {
    pub fn sync_status(&self) -> SyncStatus {
        let height = self
            .store
            .committed_tip()
            .ok()
            .flatten()
            .map(|t| t.height as u64)
            .unwrap_or_else(|| self.stats.indexed_height.load(Ordering::Relaxed));
        let headers = self.stats.core_headers.load(Ordering::Relaxed);
        let core_height = self.stats.core_height.load(Ordering::Relaxed);
        let core_ibd = self.stats.core_ibd.load(Ordering::Relaxed);
        let verification =
            self.stats.core_verification_bps.load(Ordering::Relaxed) as f64 / 10_000.0;
        let offline = self.get_state() == state::CORE_UNAVAILABLE;
        // `height + 1 >= core_height` would call an index one block behind
        // "at tip". Require the exact tip, and full sync also requires Core's
        // headers to be at the same height.
        let at_core_tip = core_height > 0 && height == core_height;
        let fully_synced =
            !core_ibd && core_height > 0 && height == core_height && headers == core_height;
        // `ready` used to fire while Core was still in IBD whenever the index
        // briefly matched Core's short tip ("READY ... Core still in IBD").
        // Wallets are only served with a stable tip when Core itself is done.
        let ready = (at_core_tip && !core_ibd) || (offline && height > 0);
        let pct = |value: u64, total: u64| {
            if total == 0 {
                0.0
            } else {
                (value as f64 / total as f64 * 100.0).min(100.0)
            }
        };
        SyncStatus {
            ready,
            at_core_tip,
            fully_synced,
            offline,
            height,
            headers,
            core_height,
            pruneheight: self.stats.core_pruneheight.load(Ordering::Relaxed),
            core_ibd,
            verification,
            lag: core_height.saturating_sub(height),
            percent: pct(height, headers),
            core_percent: pct(core_height, headers),
            blocks_per_sec: self.stats.tuning_blocks_per_sec.load(Ordering::Relaxed),
        }
    }
}

impl Shared {
    pub fn set_state(&self, s: u8) {
        self.state.store(s, Ordering::SeqCst);
    }

    pub fn get_state(&self) -> u8 {
        self.state.load(Ordering::SeqCst)
    }

    pub fn identity(&self) -> NetworkIdentity {
        match self.identity.read() {
            Ok(g) => g.clone(),
            Err(e) => e.into_inner().clone(),
        }
    }

    pub fn set_identity(&self, id: NetworkIdentity) {
        match self.identity.write() {
            Ok(mut g) => *g = id,
            Err(e) => *e.into_inner() = id,
        }
    }

    fn db_bytes_cached(&self) -> u64 {
        if let Ok(mut cache) = self.db_bytes.lock() {
            let fresh = cache
                .at
                .map(|at| at.elapsed() < Duration::from_secs(60))
                .unwrap_or(false);
            if !fresh {
                cache.bytes = self.store.disk_usage_bytes();
                cache.at = Some(Instant::now());
            }
            cache.bytes
        } else {
            0
        }
    }

    fn disk_breakdown(&self) -> (u64, u64, u64, u64, u64) {
        let total = self.db_bytes_cached();
        let txids = std::fs::metadata(self.store.txids_path())
            .map(|m| m.len())
            .unwrap_or(0);
        let cache = self.resolver.cache_size_bytes();
        let log = if self.log_file.starts_with(&self.data_dir) {
            std::fs::metadata(&self.log_file)
                .map(|m| m.len())
                .unwrap_or(0)
        } else {
            0
        };
        let snapshots = top_level_snapshot_bytes(&self.data_dir);
        let cache_on_data = self.resolver.cache_dir().starts_with(&self.data_dir);
        let index =
            total.saturating_sub(txids + log + snapshots + if cache_on_data { cache } else { 0 });
        (index, txids, cache, log, snapshots)
    }

    pub fn status_json(&self) -> Value {
        let (height, txnum, hash) = match self.store.committed_tip() {
            Ok(Some(t)) => (t.height as u64, t.txnum, t.block_hash.to_hex()),
            _ => (0, 0, String::new()),
        };
        let core_height = self.stats.core_height.load(Ordering::Relaxed);
        let pruneheight = self.stats.core_pruneheight.load(Ordering::Relaxed);
        let sync = self.sync_status();
        let db_bytes = self.db_bytes_cached();
        let (index_bytes, txids_bytes, cache_bytes, log_bytes, snapshot_bytes) =
            self.disk_breakdown();
        let rawtx = self.resolver.stats();
        let id = self.identity();
        let mem = self.mempool.read();
        let mem_count = mem.as_ref().map(|m| m.len()).unwrap_or(0);
        let mem_version = mem.as_ref().map(|m| m.version()).unwrap_or(0);
        let mem_unresolved = mem.as_ref().map(|m| m.unresolved_inputs()).unwrap_or(0);
        let mem_buckets = mem.as_ref().map(|m| m.fee_histogram().len()).unwrap_or(0);
        drop(mem);
        let machine = self
            .stats
            .plan
            .lock()
            .map(|v| v.clone())
            .unwrap_or_else(|_| json!({}));
        let rocksdb_cf = self
            .stats
            .rocksdb_cf
            .lock()
            .map(|v| v.clone())
            .unwrap_or_else(|_| json!({}));
        json!({
            "version": VERSION,
            "network": id.chain.name(),
            "machine": machine,
            "genesis": id.genesis.to_hex(),
            "state": state::name(self.get_state()),
            "indexed_height": height,
            "core_height": core_height,
            "core_headers": self.stats.core_headers.load(Ordering::Relaxed),
            "core_rpc_loopback": self.core.is_loopback(),
            "core_verification_progress": self.stats.core_verification_bps.load(Ordering::Relaxed) as f64
                / 10_000.0,
            "ready": sync.ready,
            "at_core_tip": sync.at_core_tip,
            "fully_synced": sync.fully_synced,
            "offline": sync.offline,
            "blocks_per_sec": sync.blocks_per_sec,
            "lag_blocks": core_height.saturating_sub(height),
            "retention_blocks": core_height.saturating_sub(pruneheight),
            "txnum": txnum,
            "tip_hash": hash,
            "db_bytes": db_bytes,
            "disk": {
                "index_bytes": index_bytes,
                "txids_bytes": txids_bytes,
                "cache_bytes": cache_bytes,
                "cache_dir": self.resolver.cache_dir().display().to_string(),
                "log_bytes": log_bytes,
                "snapshot_bytes": snapshot_bytes,
                "total_bytes": db_bytes,
            },
            "disk_free_bytes": el_chain::disk_free_bytes(&self.data_dir).unwrap_or(0),
            "core_disk_free_bytes": el_chain::disk_free_bytes(&self.core_datadir).unwrap_or(0),
            "disk_low": self.stats.disk_low.load(Ordering::Relaxed),
            "blocks_applied": self.stats.blocks_applied.load(Ordering::Relaxed),
            "commits": self.stats.commits.load(Ordering::Relaxed),
            "committed_txs": self.stats.committed_txs.load(Ordering::Relaxed),
            "stage_totals_us": {
                "read": self.stats.stage_read_us.load(Ordering::Relaxed),
                "decode": self.stats.stage_decode_us.load(Ordering::Relaxed),
                "prefetch": self.stats.stage_prefetch_us.load(Ordering::Relaxed),
                "wait": self.stats.stage_wait_us.load(Ordering::Relaxed),
                "apply": self.stats.stage_apply_us.load(Ordering::Relaxed),
                "commit": self.stats.stage_commit_us.load(Ordering::Relaxed),
            },
            "commit_breakdown_us": {
                "txids_append_fsync": self.stats.commit_txids_us.load(Ordering::Relaxed),
                "rocksdb_write": self.stats.commit_db_us.load(Ordering::Relaxed),
            },
            "rocksdb": {
                "pending_compaction_bytes": self.stats.rocksdb_pending_compaction_bytes.load(Ordering::Relaxed),
                "running_compactions": self.stats.rocksdb_running_compactions.load(Ordering::Relaxed),
                "level0_files": self.stats.rocksdb_level0_files.load(Ordering::Relaxed),
                "memtable_bytes": self.stats.rocksdb_memtable_bytes.load(Ordering::Relaxed),
                "block_cache_bytes": self.stats.rocksdb_block_cache_bytes.load(Ordering::Relaxed),
                "sst_bytes": self.stats.rocksdb_sst_bytes.load(Ordering::Relaxed),
                "table_readers_bytes": self.stats.rocksdb_table_readers_bytes.load(Ordering::Relaxed),
                "properties_ok": self.stats.rocksdb_properties_ok.load(Ordering::Relaxed),
                "cf": rocksdb_cf,
            },
            "file_descriptors": {
                "soft_limit": self.stats.fd_soft_limit.load(Ordering::Relaxed),
                "reserve": self.stats.fd_reserve.load(Ordering::Relaxed),
                "max_open_files_requested": self.stats.max_open_files_requested.load(Ordering::Relaxed),
                "max_open_files_effective": self.stats.max_open_files_effective.load(Ordering::Relaxed),
            },
            "clients": self.stats.clients.load(Ordering::Relaxed) as u64,
            "max_clients": self.max_clients as u64,
            "requests_total": self.stats.requests_total.load(Ordering::Relaxed),
            "errors_total": self.stats.errors_total.load(Ordering::Relaxed),
            "rate_limited": self.stats.rate_limited.load(Ordering::Relaxed),
            "rate_limit_rps": self.clients.rate_limit().0,
            "rate_burst": self.clients.rate_limit().1,
            "last_block_ms": self.stats.last_block_ms.load(Ordering::Relaxed),
            "uptime_seconds": self.stats.started.elapsed().as_secs(),
            "listen": self.listen,
            "tls_listen": self.tls_listen,
            "onion": self
                .onion
                .lock()
                .ok()
                .and_then(|v| v.clone())
                .unwrap_or_default(),
            "onion_plain_port": self.onion_plain_port,
            "onion_tls_port": self.onion_tls_port,
            "tor_bootstrapped": self.stats.tor_bootstrapped.load(Ordering::Relaxed),
            "tor": {
                "managed": self.stats.tor_managed.load(Ordering::Relaxed),
                "socks_port": self.stats.tor_socks_port.load(Ordering::Relaxed),
                // e-lite's managed torrc has no ControlPort; Core's own onion
                // is disabled with -listenonion=0.
                "control_port": self.stats.tor_control_port.load(Ordering::Relaxed),
            },
            "pipeline": {
                "prepared": self.stats.pipeline_prepared.load(Ordering::Relaxed),
                "applied": self.stats.pipeline_applied.load(Ordering::Relaxed),
                "discarded": self.stats.pipeline_discarded.load(Ordering::Relaxed),
            },
            "scheduler": {
                "mode": if self.stats.scheduler_alternate.load(Ordering::Relaxed) {
                    "alternate"
                } else {
                    "overlap"
                },
                "index_paused": self.stats.scheduler_index_paused.load(Ordering::Relaxed),
                "backlog_bytes": self.stats.scheduler_backlog_bytes.load(Ordering::Relaxed),
                "high_bytes": self.stats.scheduler_high_bytes.load(Ordering::Relaxed),
                "low_bytes": self.stats.scheduler_low_bytes.load(Ordering::Relaxed),
                "avg_block_bytes": self.stats.scheduler_avg_block_bytes.load(Ordering::Relaxed),
                "phase": match self.stats.scheduler_phase.load(Ordering::Relaxed) {
                    1 => "draining",
                    2 => "core",
                    _ => "indexing",
                },
                "phase_blocks": self.stats.scheduler_phase_blocks.load(Ordering::Relaxed),
                "phase_elapsed_ms": self.stats.scheduler_phase_elapsed_ms.load(Ordering::Relaxed),
            },
            "tls_handshakes": self.stats.tls_handshakes.load(Ordering::Relaxed),
            "tls_handshake_failures": self
                .stats
                .tls_handshake_failures
                .load(Ordering::Relaxed),
            "protocol": PROTOCOL_MAX,
            "memory": {
                "total_mib": self.stats.mem_total_mib.load(Ordering::Relaxed),
                "available_mib": self.stats.mem_available_mib.load(Ordering::Relaxed),
                "cgroup_limit_mib": self.stats.mem_cgroup_limit_mib.load(Ordering::Relaxed),
                "cgroup_used_mib": self.stats.mem_cgroup_used_mib.load(Ordering::Relaxed),
                "reclaimable_mib": self.stats.mem_reclaimable_mib.load(Ordering::Relaxed),
                "cached_mib": self.stats.mem_cached_mib.load(Ordering::Relaxed),
                "cache_max_mib": self.stats.mem_cache_max_mib.load(Ordering::Relaxed),
                "memtables_mib": self.stats.mem_memtables_mib.load(Ordering::Relaxed),
                "memtable_bytes_mib": self.stats.mem_memtable_bytes_mib.load(Ordering::Relaxed),
                "table_readers_mib": self.stats.mem_table_readers_mib.load(Ordering::Relaxed),
                "effective_mib": self.stats.mem_effective_mib.load(Ordering::Relaxed),
                "dynamic_reserve_mib": self.stats.mem_dynamic_reserve_mib.load(Ordering::Relaxed),
                "reserve_mib": self.stats.mem_reserve_mib.load(Ordering::Relaxed),
                "budget_mib": self.stats.mem_budget_mib.load(Ordering::Relaxed),
            },
            "tuning": {
                "readers": self.stats.tuning_readers.load(Ordering::Relaxed),
                "commit_interval_ms": self.stats.tuning_commit_ms.load(Ordering::Relaxed),
                "batch_blocks": self.stats.tuning_batch_blocks.load(Ordering::Relaxed),
                "cache_mib": self.stats.tuning_cache_mib.load(Ordering::Relaxed),
                "read_ms_ewma": self.stats.tuning_read_ms.load(Ordering::Relaxed),
                "apply_ms_ewma": self.stats.tuning_apply_ms.load(Ordering::Relaxed),
                "decode_ms_ewma": self.stats.tuning_decode_ms.load(Ordering::Relaxed),
                "wait_ms_ewma": self.stats.tuning_wait_ms.load(Ordering::Relaxed),
                "prefetch_p50_ms": self.stats.tuning_prefetch_p50_ms.load(Ordering::Relaxed),
                "prefetch_p95_ms": self.stats.tuning_prefetch_p95_ms.load(Ordering::Relaxed),
                "prefetch_max_ms": self.stats.tuning_prefetch_max_ms.load(Ordering::Relaxed),
                "wait_p50_ms": self.stats.tuning_wait_p50_ms.load(Ordering::Relaxed),
                "wait_p95_ms": self.stats.tuning_wait_p95_ms.load(Ordering::Relaxed),
                "commit_ms_ewma": self.stats.tuning_commit_ms_ewma.load(Ordering::Relaxed),
                "commit_per_block_ms": self.stats.tuning_commit_per_block_ms.load(Ordering::Relaxed),
                "commit_cycle_ms": self.stats.tuning_commit_cycle_ms.load(Ordering::Relaxed),
                "commit_last_ms": self.stats.tuning_commit_last_ms.load(Ordering::Relaxed),
                "commit_last_cycle_ms": self.stats.tuning_commit_last_cycle_ms.load(Ordering::Relaxed),
                "commit_last_blocks": self.stats.tuning_commit_last_blocks.load(Ordering::Relaxed),
                "commit_last_txs": self.stats.tuning_commit_last_txs.load(Ordering::Relaxed),
                "commit_last_bytes": self.stats.tuning_commit_last_bytes.load(Ordering::Relaxed),
                "commit_last_txids_us": self.stats.tuning_commit_last_txids_us.load(Ordering::Relaxed),
                "commit_last_db_us": self.stats.tuning_commit_last_db_us.load(Ordering::Relaxed),
                "commit_last_reason": self.stats.tuning_commit_last_reason.load(Ordering::Relaxed),
                "adapt_steps": self.stats.tuning_adapt_steps.load(Ordering::Relaxed),
                "tx_per_sec": self.stats.tuning_tx_per_sec.load(Ordering::Relaxed),
                "blocks_per_sec": self.stats.tuning_blocks_per_sec.load(Ordering::Relaxed),
                "profile": match self.stats.tuning_profile.load(Ordering::Relaxed) {
                    1 => "hdd", 2 => "ssd", 3 => "nvme", 4 => "manual", _ => "auto"
                },
                "adaptive": self.stats.tuning_adaptive.load(Ordering::Relaxed),
                "background_jobs": self.stats.tuning_background_jobs.load(Ordering::Relaxed),
            },
            "core": {
                "pruneheight": pruneheight,
                "pruned": self.stats.core_pruned.load(Ordering::Relaxed),
                "network_active": self.stats.core_network_active.load(Ordering::Relaxed),
                "initial_block_download": self.stats.core_ibd.load(Ordering::Relaxed),
                "connections": self.stats.core_connections.load(Ordering::Relaxed),
                "prune_requests": self.stats.prune_requests.load(Ordering::Relaxed),
                "prune_failures": self.stats.prune_failures.load(Ordering::Relaxed),
            },
            "notifier": {
                "runs": self.stats.notifier_runs.load(Ordering::Relaxed),
                "sent": self.stats.notifications_sent.load(Ordering::Relaxed),
                "errors": self.stats.notifier_errors.load(Ordering::Relaxed),
            },
            "mempool": {
                "count": mem_count,
                "version": mem_version,
                "unresolved_inputs": mem_unresolved,
                "histogram_buckets": mem_buckets,
            },
            "public_servers": &self.public_servers,
            "provider_status": self
                .provider_status
                .lock()
                .map(|s| s.clone())
                .unwrap_or_default(),
            "provider_ready": self.provider_ready.load(Ordering::Relaxed),
            "providers_healthy": self
                .provider_status
                .lock()
                .map(|s| s.iter().filter(|p| p.get("healthy").and_then(Value::as_bool).unwrap_or(false)).count() as u64)
                .unwrap_or(0),
            "tor_ready": self.tor_ready.load(Ordering::Relaxed),
            "clearnet_fallback": self.clearnet_fallback.load(Ordering::Relaxed),
            "rawtx": {
                "cache_hits": rawtx.cache_hits.load(Ordering::Relaxed),
                "cache_puts": rawtx.cache_puts.load(Ordering::Relaxed),
                "fetches": rawtx.fetches.load(Ordering::Relaxed),
                "bytes": rawtx.bytes.load(Ordering::Relaxed),
                "verify_failures": rawtx.verify_failures.load(Ordering::Relaxed),
                "decoys": rawtx.decoys.load(Ordering::Relaxed),
                "decoys_inflight": self.resolver.decoys_inflight(),
                "unavailable": rawtx.unavailable.load(Ordering::Relaxed),
                "external_sources": self.resolver.external_sources(),
                "p2p_attempts": rawtx.p2p_attempts.load(Ordering::Relaxed),
                "p2p_failures": rawtx.p2p_failures.load(Ordering::Relaxed),
                "p2p_skipped": rawtx.p2p_skipped.load(Ordering::Relaxed),
                "p2p_tor_failures": rawtx.p2p_tor_failures.load(Ordering::Relaxed),
            },
            "prefetch": {
                "queued": self.prefetch.stats().0,
                "done": self.prefetch.stats().1,
            },
        })
    }
}

/// On-demand snapshots are user files; count the common top-level layout only.
fn top_level_snapshot_bytes(dir: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_string_lossy();
            if !path.is_file() || !name.ends_with(".tar.gz") {
                return None;
            }
            entry.metadata().ok().map(|m| m.len())
        })
        .sum()
}

/// Free bytes on the filesystem containing `path` (0 when it cannot be read).
pub fn disk_free_bytes(path: &std::path::Path) -> u64 {
    el_chain::disk_free_bytes(path).unwrap_or(0)
}

#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    /// Set when the connection must be closed after sending the error.
    pub close: bool,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        RpcError {
            code,
            message: message.into(),
            close: false,
        }
    }
    fn invalid(msg: impl Into<String>) -> Self {
        Self::new(-32602, msg)
    }
    fn method(msg: impl Into<String>) -> Self {
        Self::new(-32601, msg)
    }
    fn server(msg: impl Into<String>) -> Self {
        Self::new(-32000, msg)
    }
    /// Mark this error as fatal for the connection.
    fn closing(mut self) -> Self {
        self.close = true;
        self
    }
}

type RpcResult<T> = std::result::Result<T, RpcError>;

pub const METHOD_NOT_FOUND: i64 = -32601;

fn log_rpc_error(shared: &Shared, method: &str, code: i64) {
    let key = format!("{method}:{code}");
    let Ok(mut seen) = shared.error_log.lock() else {
        return;
    };
    if let Some(at) = seen.get(&key) {
        if at.elapsed() < Duration::from_secs(60) {
            return;
        }
    }
    seen.insert(key, Instant::now());
    eprintln!("[e-lite] rpc error: method={method} code={code}");
}

pub fn dispatch(shared: &Shared, method: &str, params: &Value) -> RpcResult<Value> {
    dispatch_for(shared, None, SERVER_MIN, method, params)
}

pub fn dispatch_for(
    shared: &Shared,
    client: Option<&Arc<ClientState>>,
    version: ProtocolVersion,
    method: &str,
    params: &Value,
) -> RpcResult<Value> {
    match method {
        "server.version" => server_version(params, client),
        "server.ping" => server_ping(params, version),
        "server.banner" => Ok(json!(banner_text(shared))),
        "server.donation_address" => Ok(json!("")),
        "server.peers.subscribe" => Ok(json!([])),
        "server.add_peer" => Ok(json!(false)),
        "server.features" => server_features(shared, version),
        "mempool.get_info" => mempool_info(shared),
        "mempool.recent" => mempool_recent(shared),
        _ => {
            // Everything else reads the index: enforce a consistent tip and a
            // non-misleading state. The read lock is taken per method and is
            // *never* held across Core or external network I/O.
            check_ready(shared)?;
            match method {
                "blockchain.headers.subscribe" => {
                    let _g = read_guard(shared)?;
                    headers_subscribe(shared, client)
                }
                "blockchain.block.header" => {
                    let _g = read_guard(shared)?;
                    block_header(shared, params)
                }
                "blockchain.block.headers" => {
                    let _g = read_guard(shared)?;
                    block_headers(shared, params, version)
                }
                "blockchain.scripthash.get_history" => {
                    let _g = read_guard(shared)?;
                    scripthash_history(shared, params)
                }
                "blockchain.scripthash.get_balance" => {
                    let _g = read_guard(shared)?;
                    scripthash_balance(shared, params)
                }
                "blockchain.scripthash.listunspent" => {
                    let _g = read_guard(shared)?;
                    scripthash_listunspent(shared, params)
                }
                "blockchain.scripthash.subscribe" => {
                    let _g = read_guard(shared)?;
                    scripthash_subscribe(shared, client, params)
                }
                "blockchain.scripthash.unsubscribe" => {
                    let script = param_scripthash(params)?;
                    let removed = client
                        .and_then(|c| {
                            c.subs
                                .lock()
                                .ok()
                                .map(|mut subs| subs.remove(&script).is_some())
                        })
                        .unwrap_or(false);
                    Ok(json!(removed))
                }
                "blockchain.scripthash.get_mempool" => scripthash_mempool(shared, params),
                "blockchain.scriptpubkey.get_history" => {
                    let _g = read_guard(shared)?;
                    scriptpubkey_history(shared, params)
                }
                "blockchain.scriptpubkey.get_balance" => {
                    let _g = read_guard(shared)?;
                    scriptpubkey_balance(shared, params)
                }
                "blockchain.scriptpubkey.listunspent" => {
                    let _g = read_guard(shared)?;
                    scriptpubkey_listunspent(shared, params)
                }
                "blockchain.scriptpubkey.get_mempool" => scriptpubkey_mempool(shared, params),
                "blockchain.scriptpubkey.subscribe" => {
                    let _g = read_guard(shared)?;
                    scriptpubkey_subscribe(shared, client, params)
                }
                "blockchain.scriptpubkey.unsubscribe" => scriptpubkey_unsubscribe(client, params),
                "blockchain.outpoint.get_status" => outpoint_get_status(shared, params),
                "blockchain.outpoint.subscribe" => outpoint_subscribe(shared, client, params),
                "blockchain.outpoint.unsubscribe" => outpoint_unsubscribe(client, params),
                "mempool.get_fee_histogram" => fee_histogram(shared),
                "blockchain.relayfee" => relay_fee(shared),
                "blockchain.estimatefee" => estimate_fee(shared, params),
                "blockchain.block.stats" => block_stats(shared, params),
                "blockchain.transaction.broadcast" => broadcast_transaction(shared, params),
                "blockchain.transaction.broadcast_package" => {
                    broadcast_package(shared, params, version)
                }
                "blockchain.transaction.testmempoolaccept" => test_mempool_accept(shared, params),
                "blockchain.silentpayments.subscribe" | "blockchain.silentpayments.unsubscribe" => {
                    silent_payments(shared, method)
                }
                "blockchain.transaction.get" => transaction_get(shared, params),
                "blockchain.transaction.get_merkle" => {
                    let _g = read_guard(shared)?;
                    transaction_merkle(shared, params)
                }
                "blockchain.transaction.id_from_pos" => {
                    let _g = read_guard(shared)?;
                    transaction_id_from_pos(shared, params)
                }
                _ => Err(RpcError::method(format!("unknown method: {method}"))),
            }
        }
    }
}

fn read_guard(shared: &Shared) -> RpcResult<RwLockReadGuard<'_, ()>> {
    shared
        .lock
        .read()
        .map_err(|_| RpcError::server("lock poisoned"))
}

fn check_ready(shared: &Shared) -> RpcResult<()> {
    let has_tip = shared.store.committed_tip().ok().flatten().is_some();
    match shared.get_state() {
        state::READY => Ok(()),
        state::DEGRADED | state::CORE_UNAVAILABLE | state::PAUSE_DOWNLOAD | state::PRUNING
            if has_tip =>
        {
            Ok(())
        }
        state::DEGRADED | state::CORE_UNAVAILABLE | state::PAUSE_DOWNLOAD | state::PRUNING => Err(
            RpcError::server("the index has no data yet; waiting for Bitcoin Core"),
        ),
        state::BUILDING => {
            let tip = shared
                .store
                .committed_tip()
                .ok()
                .flatten()
                .map(|t| t.height)
                .unwrap_or(0);
            Err(RpcError::server(format!(
                "index is building (indexed height {tip}); retry later"
            )))
        }
        state::WAITING_FOR_CORE => Err(RpcError::server(
            "waiting for Bitcoin Core; the index is not initialized yet",
        )),
        state::INITIALIZING => Err(RpcError::server(
            "initializing the index against Bitcoin Core; retry later",
        )),
        state::REORG => Err(RpcError::server("reorg recovery in progress")),
        state::FATAL => Err(RpcError::server(
            "fatal integrity error; operator action required",
        )),
        _ => Err(RpcError::server("server not ready")),
    }
}

fn server_version(params: &Value, client: Option<&Arc<ClientState>>) -> RpcResult<Value> {
    let arr = params
        .as_array()
        .ok_or_else(|| RpcError::invalid("server.version expects [client, protocol]"))?;
    // 1.6 mandates that this is the first message. If the connection already
    // sent something else, negotiate only up to 1.4.2 instead of disconnecting.
    let allow_1_6 = client
        .map(|c| !c.saw_message.load(Ordering::SeqCst))
        .unwrap_or(true);
    let negotiated = negotiate_protocol(arr.get(1), allow_1_6)?;
    if let Some(c) = client {
        let mut slot = c
            .protocol
            .lock()
            .map_err(|_| RpcError::server("client lock poisoned"))?;
        if slot.is_some() {
            return Err(RpcError::new(
                -32600,
                "server.version already negotiated on this connection",
            ));
        }
        *slot = Some(negotiated);
    }
    Ok(json!([format!("e-lite {VERSION}"), negotiated.to_string()]))
}

/// Resolve the requested client range against the versions this build serves.
/// A missing request means the protocol default, 1.4.
fn negotiate_protocol(requested: Option<&Value>, allow_1_6: bool) -> RpcResult<ProtocolVersion> {
    let (client_min, client_max) = match requested {
        None => (SERVER_MIN, SERVER_MIN),
        Some(Value::String(value)) => {
            let version = ProtocolVersion::parse(value).map_err(RpcError::invalid)?;
            (version, version)
        }
        Some(Value::Array(range)) => {
            let min = range
                .first()
                .and_then(|v| v.as_str())
                .ok_or_else(|| RpcError::invalid("protocol range expects [min, max]"))?;
            let min = ProtocolVersion::parse(min).map_err(RpcError::invalid)?;
            let max = match range.get(1) {
                Some(Value::String(value)) => {
                    ProtocolVersion::parse(value).map_err(RpcError::invalid)?
                }
                None => min,
                Some(_) => return Err(RpcError::invalid("protocol range expects string versions")),
            };
            (min, max)
        }
        Some(_) => {
            return Err(RpcError::invalid(
                "protocol must be a version string or [min, max]",
            ))
        }
    };
    if client_max < client_min {
        return Err(RpcError::invalid("protocol range is inverted"));
    }
    // Deliberately lenient: a client asking for a future or skipped version
    // (1.5, 1.7) is served the newest version we actually implement below it.
    // We only refuse clients that cannot speak 1.4 at all.  The negotiated
    // version is returned, so a compliant client knows which shape to expect.
    let available = if allow_1_6 {
        SERVER_VERSIONS
    } else {
        PRE_1_6_VERSIONS
    };
    ProtocolVersion::highest_available(client_max, available).ok_or_else(|| {
        RpcError::server(format!(
            "no supported protocol at or below {client_max}; server supports \
             {PROTOCOL_MIN}-{PROTOCOL_MAX}"
        ))
        .closing()
    })
}

pub const DEFAULT_BANNER_TEMPLATE: &str = r#"# e-lite wallet banner template
# Placeholders: {version} {network} {state} {height} {headers} {percent} {bar}
# {core_height} {core_percent} {lag} {blocks_per_sec} {tx_per_sec} {eta}
# {prune} {pruneheight} {listen} {tls} {log_file} {uptime} {ready} {ready_note}
# Sections: [indexing] while the index catches up, [ready] when usable.

[indexing]
e-lite {version} · {network} · {state}
{bar} {percent}
height {height} / {headers} · Core {core_height} ({core_percent})
lag {lag} · {blocks_per_sec} blk/s · ETA {eta}
Electrum {listen}{tls}
Some wallet queries may be unavailable until caught up.

[ready]
==============================================
   e-lite  ·  {state}  ·  {network}
   height {height} / {headers}  ({percent})
   Electrum {listen}{tls}
   {ready_note}
==============================================
"#;

const BANNER_MAX_BYTES: u64 = 32 * 1024;

/// Create the configured banner file with the default template if it does not
/// exist yet. A relative path is resolved by the caller.
pub fn ensure_banner_file(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, DEFAULT_BANNER_TEMPLATE)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644));
    }
    Ok(())
}

fn banner_text(shared: &Shared) -> String {
    let status = shared.sync_status();
    let (default_indexing, default_ready) = split_banner(DEFAULT_BANNER_TEMPLATE);
    let (custom_indexing, custom_ready) = shared
        .banner_file
        .as_ref()
        .and_then(|path| read_banner(path))
        .unwrap_or((None, None));
    let template = if status.ready {
        custom_ready.or(default_ready).unwrap_or_default()
    } else {
        custom_indexing.or(default_indexing).unwrap_or_default()
    };
    render_banner(&template, shared, &status)
}

fn read_banner(path: &Path) -> Option<(Option<String>, Option<String>)> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > BANNER_MAX_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    Some(split_banner(&text))
}

/// Split a template into `[indexing]` and `[ready]` sections. A file without
/// section headers is used for both states.
fn split_banner(text: &str) -> (Option<String>, Option<String>) {
    let mut indexing = String::new();
    let mut ready = String::new();
    let mut current: Option<u8> = None;
    let mut saw_section = false;
    for line in text.lines() {
        match line.trim() {
            "[indexing]" => {
                current = Some(0);
                saw_section = true;
            }
            "[ready]" => {
                current = Some(1);
                saw_section = true;
            }
            _ => match current {
                Some(0) => {
                    indexing.push_str(line);
                    indexing.push('\n');
                }
                Some(1) => {
                    ready.push_str(line);
                    ready.push('\n');
                }
                _ => {}
            },
        }
    }
    if !saw_section {
        return (Some(text.to_string()), Some(text.to_string()));
    }
    let out = |s: String| if s.trim().is_empty() { None } else { Some(s) };
    (out(indexing), out(ready))
}

fn render_banner(template: &str, shared: &Shared, status: &SyncStatus) -> String {
    let state = if status.fully_synced {
        "FULLY SYNCED"
    } else if status.ready && status.offline {
        "READY (offline)"
    } else if status.ready && status.core_ibd {
        "READY (Core verifying)"
    } else if status.ready {
        "READY"
    } else {
        "INDEXING"
    };
    let ready_note = if status.fully_synced {
        "Fully synced. Wallet ready to use."
    } else if status.offline {
        "Offline cached index; Core is unavailable."
    } else if status.core_ibd {
        "Ready for the indexed height; Core is still verifying."
    } else {
        "Wallet ready to use."
    };
    let tls = shared
        .tls_listen
        .as_ref()
        .map(|addr| format!(" · SSL {addr}"))
        .unwrap_or_default();
    let bar = progress_bar(status.percent);
    let eta = eta_text(status.lag, status.blocks_per_sec);
    let network = shared.identity().chain.name().to_string();
    let tx_per_sec = shared.stats.tuning_tx_per_sec.load(Ordering::Relaxed);
    template
        .replace("{version}", VERSION)
        .replace("{network}", &network)
        .replace("{state}", state)
        .replace("{height}", &format_num(status.height))
        .replace("{headers}", &format_num(status.headers))
        .replace("{percent}", &format!("{:.1}%", status.percent))
        .replace("{bar}", &bar)
        .replace("{core_height}", &format_num(status.core_height))
        .replace("{core_percent}", &format!("{:.1}%", status.core_percent))
        .replace("{lag}", &format_num(status.lag))
        .replace("{blocks_per_sec}", &status.blocks_per_sec.to_string())
        .replace("{tx_per_sec}", &tx_per_sec.to_string())
        .replace("{eta}", &eta)
        .replace("{pruneheight}", &format_num(status.pruneheight))
        .replace("{prune}", &format_num(status.pruneheight))
        .replace("{listen}", &shared.listen)
        .replace("{tls}", &tls)
        .replace("{log_file}", &shared.log_file.display().to_string())
        .replace("{uptime}", &uptime_text(shared.stats.started.elapsed()))
        .replace("{ready}", if status.ready { "yes" } else { "no" })
        .replace("{ready_note}", ready_note)
}

fn progress_bar(percent: f64) -> String {
    const WIDTH: usize = 30;
    let filled = ((percent.clamp(0.0, 100.0) / 100.0) * WIDTH as f64).round() as usize;
    format!("[{}{}]", "#".repeat(filled), "-".repeat(WIDTH - filled))
}

fn format_num(value: u64) -> String {
    let s = value.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn eta_text(lag: u64, blocks_per_sec: u64) -> String {
    if lag == 0 {
        return "0m".to_string();
    }
    if blocks_per_sec == 0 {
        return "unknown".to_string();
    }
    let secs = lag / blocks_per_sec.max(1);
    if secs >= 3600 {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

fn uptime_text(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    format!(
        "{}h {:02}m {:02}s",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn server_ping(params: &Value, version: ProtocolVersion) -> RpcResult<Value> {
    if version < V1_7 {
        return Ok(Value::Null);
    }
    // The 1.7 signature is (pong_len, data). `data` is ignored by the caller
    // and we return the requested number of hex characters.
    let pong_len = params
        .as_array()
        .and_then(|a| a.first())
        .and_then(Value::as_u64)
        .unwrap_or(0);
    const MAX_PONG_HEX: u64 = 1024 * 1024;
    if pong_len > MAX_PONG_HEX {
        return Err(RpcError::invalid(format!(
            "pong_len must be <= {MAX_PONG_HEX} hex characters"
        )));
    }
    Ok(json!({ "data": "0".repeat(pong_len as usize) }))
}

fn server_features(shared: &Shared, version: ProtocolVersion) -> RpcResult<Value> {
    if matches!(
        shared.get_state(),
        state::WAITING_FOR_CORE | state::INITIALIZING
    ) {
        return Err(RpcError::server(
            "waiting for Bitcoin Core; the network identity is not initialized yet",
        ));
    }
    let id = shared.identity();
    let mut features = json!({
        "genesis_hash": id.genesis.to_hex(),
        "hash_function": "sha256",
        "server_version": format!("e-lite {VERSION}"),
        "protocol_min": PROTOCOL_MIN,
        "protocol_max": PROTOCOL_MAX,
        "pruning": Value::Null,
        "hosts": {},
    });
    if version >= V1_7 {
        if let Some(obj) = features.as_object_mut() {
            obj.remove("hash_function");
        }
    }
    Ok(features)
}

fn tip_block(shared: &Shared) -> RpcResult<el_model::BlockRecord> {
    let tip = shared
        .store
        .committed_tip()
        .map_err(|e| RpcError::server(e.to_string()))?
        .ok_or_else(|| RpcError::server("index is empty"))?;
    shared
        .store
        .get_block(tip.height)
        .map_err(|e| RpcError::server(e.to_string()))?
        .ok_or_else(|| RpcError::server("tip block missing"))
}

fn headers_subscribe(shared: &Shared, client: Option<&Arc<ClientState>>) -> RpcResult<Value> {
    let rec = tip_block(shared)?;
    if let Some(c) = client {
        c.headers.store(true, Ordering::SeqCst);
        c.last_height.store(rec.height as u64, Ordering::SeqCst);
    }
    Ok(json!({
        "height": rec.height,
        "hex": hex::encode(rec.header),
    }))
}

fn param_u32(params: &Value, idx: usize) -> RpcResult<u32> {
    let value = params
        .as_array()
        .and_then(|a| a.get(idx))
        .and_then(|v| v.as_u64())
        .ok_or_else(|| RpcError::invalid("expected an integer parameter"))?;
    u32::try_from(value).map_err(|_| RpcError::invalid("integer parameter out of range"))
}

fn header_chain(shared: &Shared, cp_height: u32) -> RpcResult<Vec<Hash32>> {
    shared
        .headers
        .chain(&shared.store, cp_height)
        .map_err(RpcError::server)
}

fn checkpoint_proof(
    shared: &Shared,
    leaf_height: u32,
    cp_height: u32,
) -> RpcResult<(Hash32, Vec<Hash32>)> {
    if leaf_height > cp_height {
        return Err(RpcError::invalid(
            "height must be <= cp_height when cp_height is set",
        ));
    }
    let hashes = header_chain(shared, cp_height)?;
    merkle_branch(&hashes, leaf_height as usize)
        .ok_or_else(|| RpcError::server("cannot compute checkpoint branch"))
}

fn block_header(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let height = param_u32(params, 0)?;
    let cp_height = params
        .as_array()
        .and_then(|a| a.get(1))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let rec = shared
        .store
        .get_block(height)
        .map_err(|e| RpcError::server(e.to_string()))?
        .ok_or_else(|| RpcError::server(format!("height {height} not indexed")))?;
    let header = hex::encode(rec.header);
    if cp_height == 0 {
        return Ok(json!(header));
    }
    let cp_height =
        u32::try_from(cp_height).map_err(|_| RpcError::invalid("cp_height out of range"))?;
    let (root, branch) = checkpoint_proof(shared, height, cp_height)?;
    Ok(json!({
        "header": header,
        "branch": branch.iter().map(|h| h.to_hex()).collect::<Vec<_>>(),
        "root": root.to_hex(),
    }))
}

fn block_headers(shared: &Shared, params: &Value, version: ProtocolVersion) -> RpcResult<Value> {
    let start = param_u32(params, 0)?;
    let count = param_u32(params, 1)?.min(MAX_HEADERS_CHUNK as u32);
    let cp_height = params
        .as_array()
        .and_then(|a| a.get(2))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cp_height =
        u32::try_from(cp_height).map_err(|_| RpcError::invalid("cp_height out of range"))?;
    let mut headers = Vec::new();
    let mut got = 0u32;
    for h in start..start.saturating_add(count) {
        match shared
            .store
            .get_block(h)
            .map_err(|e| RpcError::server(e.to_string()))?
        {
            Some(rec) => {
                headers.push(hex::encode(rec.header));
                got += 1;
            }
            None => break,
        }
    }
    // 1.6 changed the response from one concatenated `hex` string to a list.
    let mut result = if version >= V1_6 {
        json!({
            "count": got,
            "headers": headers,
            "max": MAX_HEADERS_CHUNK,
        })
    } else {
        json!({
            "count": got,
            "hex": headers.concat(),
            "max": MAX_HEADERS_CHUNK,
        })
    };
    if cp_height != 0 && got > 0 {
        let last = start + got - 1;
        let (root, branch) = checkpoint_proof(shared, last, cp_height)?;
        result["root"] = json!(root.to_hex());
        result["branch"] = json!(branch.iter().map(|h| h.to_hex()).collect::<Vec<_>>());
    }
    Ok(result)
}

fn param_scripthash(params: &Value) -> RpcResult<ScriptHash> {
    let s = params
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .ok_or_else(|| RpcError::invalid("expected scripthash hex"))?;
    ScriptHash::from_electrum_hex(s).map_err(|e| RpcError::invalid(e.to_string()))
}

type HistoryRow = (u32, u32, String, u64, u64);

fn history_entries_with_code(
    shared: &Shared,
    script: ScriptHash,
    too_large_code: i64,
    prefetch: bool,
) -> RpcResult<Vec<HistoryRow>> {
    let entries = shared
        .store
        .history_for_script(script)
        .map_err(|e| RpcError::server(e.to_string()))?;
    if entries.len() > MAX_HISTORY_ENTRIES {
        return Err(RpcError::new(
            too_large_code,
            format!(
                "history for this script has {} entries (limit {}); use a script with fewer transactions",
                entries.len(),
                MAX_HISTORY_ENTRIES
            ),
        ));
    }
    let mut blocks: HashMap<u32, el_model::BlockRecord> = HashMap::new();
    let mut out = Vec::with_capacity(entries.len());
    let mut prefetched = 0usize;
    for (height, tx_pos, effect) in entries {
        let rec = match blocks.get(&height) {
            Some(r) => *r,
            None => {
                let r = shared
                    .store
                    .get_block(height)
                    .map_err(|e| RpcError::server(e.to_string()))?
                    .ok_or_else(|| RpcError::server(format!("block {height} missing")))?;
                blocks.insert(height, r);
                r
            }
        };
        let txnum = rec.txnum_base + tx_pos as u64;
        let txid = shared
            .store
            .txid_at(txnum)
            .map_err(|e| RpcError::server(e.to_string()))?;
        if prefetch && prefetched < 16 && shared.prefetch.enqueue(rec.hash, txid, height) {
            prefetched += 1;
        }
        out.push((
            height,
            tx_pos,
            txid.to_hex(),
            effect.received_sat,
            effect.spent_sat,
        ));
    }
    Ok(out)
}

fn history_value(shared: &Shared, script: ScriptHash, too_large_code: i64) -> RpcResult<Value> {
    let entries = history_entries_with_code(shared, script, too_large_code, true)?;
    let mem = shared
        .mempool
        .read()
        .map_err(|_| RpcError::server("mempool lock poisoned"))?;
    let mem_entries = mem.history(script);
    let mut arr = Vec::with_capacity(entries.len() + mem_entries.len());
    for (height, _, txid, _, _) in entries {
        arr.push(json!({"tx_hash": txid, "height": height}));
    }
    // Electrum history appends mempool entries after the confirmed ones.
    for (txid, fee, height) in mem_entries {
        arr.push(json!({
            "tx_hash": txid.to_hex(),
            "height": height,
            "fee": fee,
        }));
    }
    Ok(Value::Array(arr))
}

fn balance_value(shared: &Shared, script: ScriptHash) -> RpcResult<Value> {
    let utxos = shared
        .store
        .utxos_for_script(script)
        .map_err(|e| RpcError::server(e.to_string()))?;
    let mem = shared
        .mempool
        .read()
        .map_err(|_| RpcError::server("mempool lock poisoned"))?;
    // Confirmed balance ignores mempool events; the unconfirmed field carries
    // the mempool delta (which already subtracts spent confirmed UTXOs).
    let confirmed: u64 = utxos.iter().map(|(_, v, _, _)| *v).sum();
    Ok(json!({
        "confirmed": confirmed,
        "unconfirmed": mem.balance_delta(script),
    }))
}

fn listunspent_value(shared: &Shared, script: ScriptHash) -> RpcResult<Value> {
    let utxos = shared
        .store
        .utxos_for_script(script)
        .map_err(|e| RpcError::server(e.to_string()))?;
    let mem = shared
        .mempool
        .read()
        .map_err(|_| RpcError::server("mempool lock poisoned"))?;
    // Electrum expects confirmed UTXOs in blockchain order, even though the
    // store index is keyed by (script, outpoint).
    let mut rows: Vec<(u32, String, u32, u64)> = Vec::with_capacity(utxos.len());
    for (op, value, height, _coinbase) in utxos {
        if mem.is_spent(&op) {
            continue;
        }
        rows.push((height, op.txid.to_hex(), op.vout, value));
    }
    rows.sort_by(|a, b| (a.0, &a.1, a.2).cmp(&(b.0, &b.1, b.2)));
    let mut arr: Vec<Value> = rows
        .into_iter()
        .map(|(height, tx_hash, tx_pos, value)| {
            json!({
                "tx_hash": tx_hash,
                "tx_pos": tx_pos,
                "height": height,
                "value": value,
            })
        })
        .collect();
    for (op, value) in mem.utxos(script) {
        arr.push(json!({
            "tx_hash": op.txid.to_hex(),
            "tx_pos": op.vout,
            "height": 0,
            "value": value,
        }));
    }
    Ok(Value::Array(arr))
}

fn status_value(
    shared: &Shared,
    script: ScriptHash,
    too_large_code: i64,
) -> RpcResult<Option<String>> {
    let entries = history_entries_with_code(shared, script, too_large_code, false)?;
    let mem = shared
        .mempool
        .read()
        .map_err(|_| RpcError::server("mempool lock poisoned"))?;
    let mem_entries = mem.history(script);
    if entries.is_empty() && mem_entries.is_empty() {
        return Ok(None);
    }
    let mut hasher = Sha256::new();
    for (height, _, txid, _, _) in entries {
        hasher.update(format!("{txid}:{height}:").as_bytes());
    }
    for (txid, _, height) in mem_entries {
        hasher.update(format!("{txid}:{height}:").as_bytes());
    }
    Ok(Some(hex::encode(hasher.finalize())))
}

fn script_status(shared: &Shared, script: ScriptHash) -> RpcResult<Option<String>> {
    status_value(shared, script, -32000)
}

fn mempool_value(shared: &Shared, script: ScriptHash) -> RpcResult<Value> {
    let mem = shared
        .mempool
        .read()
        .map_err(|_| RpcError::server("mempool lock poisoned"))?;
    let arr: Vec<Value> = mem
        .history(script)
        .into_iter()
        .map(|(txid, fee, height)| {
            json!({
                "tx_hash": txid.to_hex(),
                "height": height,
                "fee": fee,
            })
        })
        .collect();
    Ok(Value::Array(arr))
}

fn scripthash_history(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let script = param_scripthash(params)?;
    // Never log the script hash: it identifies a wallet address (AUD-132).
    history_value(shared, script, -32000).inspect_err(|e| {
        eprintln!(
            "[e-lite] get_history failed: {}",
            el_rawtx::redact_hashes(&e.message)
        );
    })
}

fn scripthash_balance(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let script = param_scripthash(params)?;
    balance_value(shared, script)
}

fn scripthash_listunspent(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let script = param_scripthash(params)?;
    listunspent_value(shared, script)
}

fn scripthash_subscribe(
    shared: &Shared,
    client: Option<&Arc<ClientState>>,
    params: &Value,
) -> RpcResult<Value> {
    let script = param_scripthash(params)?;
    let status = script_status(shared, script)?;
    if let Some(c) = client {
        if let Ok(mut subs) = c.subs.lock() {
            subs.insert(script, status.clone());
        }
    }
    match status {
        Some(status) => Ok(json!(status)),
        None => Ok(Value::Null),
    }
}

fn param_scriptpubkey(params: &Value) -> RpcResult<ScriptHash> {
    let s = params
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .ok_or_else(|| RpcError::invalid("expected scriptPubKey hex"))?;
    let bytes =
        hex::decode(s).map_err(|e| RpcError::invalid(format!("invalid scriptPubKey: {e}")))?;
    Ok(ScriptHash::from_script(&bytes))
}

fn scriptpubkey_history(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let script = param_scriptpubkey(params)?;
    Ok(json!({ "history": history_value(shared, script, 10001)? }))
}

fn scriptpubkey_balance(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let script = param_scriptpubkey(params)?;
    balance_value(shared, script)
}

fn scriptpubkey_listunspent(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let script = param_scriptpubkey(params)?;
    Ok(json!({ "utxos": listunspent_value(shared, script)? }))
}

fn scriptpubkey_mempool(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let script = param_scriptpubkey(params)?;
    Ok(json!({ "history": mempool_value(shared, script)? }))
}

fn scriptpubkey_subscribe(
    shared: &Shared,
    client: Option<&Arc<ClientState>>,
    params: &Value,
) -> RpcResult<Value> {
    let script = param_scriptpubkey(params)?;
    let status = status_value(shared, script, 10001)?;
    if let Some(c) = client {
        if let Ok(mut spk) = c.spk_subs.lock() {
            spk.insert(script, status.clone());
        }
    }
    match status {
        Some(status) => Ok(json!(status)),
        None => Ok(Value::Null),
    }
}

fn scriptpubkey_unsubscribe(client: Option<&Arc<ClientState>>, params: &Value) -> RpcResult<Value> {
    let script = param_scriptpubkey(params)?;
    let removed = client
        .and_then(|c| {
            c.spk_subs
                .lock()
                .ok()
                .map(|mut subs| subs.remove(&script).is_some())
        })
        .unwrap_or(false);
    Ok(json!(removed))
}
fn param_outpoint(params: &Value) -> RpcResult<(Hash32, u32, ScriptHash)> {
    let arr = params
        .as_array()
        .ok_or_else(|| RpcError::invalid("expected [tx_hash, txout_idx, spk_hint]"))?;
    let txid = arr
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid("expected tx_hash hex"))?;
    let txid = Hash32::from_hex(txid).map_err(RpcError::invalid)?;
    let vout = u32::try_from(
        arr.get(1)
            .and_then(Value::as_u64)
            .ok_or_else(|| RpcError::invalid("expected txout_idx"))?,
    )
    .map_err(|_| RpcError::invalid("txout_idx out of range"))?;
    let script = param_scriptpubkey(&json!([arr.get(2).cloned().unwrap_or(Value::Null)]))?;
    Ok((txid, vout, script))
}

fn decode_raw_tx(raw: &str) -> RpcResult<bitcoin::Transaction> {
    let bytes = hex::decode(raw).map_err(|e| RpcError::server(format!("rawtx hex: {e}")))?;
    bitcoin::consensus::encode::deserialize::<bitcoin::Transaction>(&bytes)
        .map_err(|e| RpcError::server(format!("rawtx decode: {e}")))
}

/// Only blocks Core can still serve need a reader guard. A block below
/// Core's prune height is resolved from cache/P2P/Esplora; holding a guard for
/// it would block pruning without any benefit.
fn rawtx_reader_guard(shared: &Shared, height: u32) -> Option<el_chain::ReaderGuard> {
    let pruneheight = shared.stats.core_pruneheight.load(Ordering::Relaxed);
    if (height as u64) < pruneheight {
        None
    } else {
        Some(shared.readers.enter(height))
    }
}

fn resolve_raw(shared: &Shared, txid: Hash32, height: u32) -> RpcResult<String> {
    let _guard = rawtx_reader_guard(shared, height);
    shared
        .resolver
        .raw_tx(txid, height)
        .map_err(|e| RpcError::server(e.to_string()))
}

/// Resolve the Electrum 1.7 outpoint status without a persistent spend index.
/// The fast path is a live UTXO (no raw-tx fetch); spent outputs are resolved
/// from the script history, and only the candidate raw transactions are read.
fn outpoint_status(
    shared: &Shared,
    txid: Hash32,
    vout: u32,
    script: ScriptHash,
) -> RpcResult<Value> {
    let op = Outpoint { txid, vout };

    // Funder in the mempool: outputs are already in memory and verify the hint.
    {
        let mem = shared
            .mempool
            .read()
            .map_err(|_| RpcError::server("mempool lock poisoned"))?;
        if let Some(tx) = mem.tx(&txid) {
            let Some((_, out_script, _)) = tx.outputs.iter().find(|(op, _, _)| op.vout == vout)
            else {
                return Ok(json!({}));
            };
            if *out_script != script {
                return Ok(json!({}));
            }
            let mut status = json!({ "funder_height": tx.height });
            if let Some((spender, height)) = mem.spender(&op) {
                status["spender_txhash"] = json!(spender.to_hex());
                status["spender_height"] = json!(height);
            }
            return Ok(status);
        }
    }

    // Confirmed funder height.
    let funder_height = {
        let _g = read_guard(shared)?;
        let occurrences = shared
            .store
            .occurrences(txid)
            .map_err(|e| RpcError::server(e.to_string()))?;
        let Some((height, _)) = occurrences.first().copied() else {
            return Ok(json!({}));
        };
        height
    };

    // Mempool spender of a confirmed output.
    if let Some((spender, height)) = shared
        .mempool
        .read()
        .map_err(|_| RpcError::server("mempool lock poisoned"))?
        .spender(&op)
    {
        return Ok(json!({
            "funder_height": funder_height,
            "spender_txhash": spender.to_hex(),
            "spender_height": height,
        }));
    }

    // Unspent fast path: the UTXO also carries the authoritative script.
    let utxo = {
        let _g = read_guard(shared)?;
        shared
            .store
            .get_utxo(&op)
            .map_err(|e| RpcError::server(e.to_string()))?
    };
    if let Some(utxo) = utxo {
        if utxo.script_hash != script {
            return Ok(json!({}));
        }
        return Ok(json!({ "funder_height": funder_height }));
    }

    // Spent: verify the funding output against the client's hint, then scan the
    // spending candidates. The funding block is normally already cached.
    let funding_raw = resolve_raw(shared, txid, funder_height)?;
    let funding = decode_raw_tx(&funding_raw)?;
    let Some(output) = funding.output.get(vout as usize) else {
        return Ok(json!({}));
    };
    if ScriptHash::from_script(output.script_pubkey.as_bytes()) != script {
        return Ok(json!({}));
    }

    let candidates = {
        let _g = read_guard(shared)?;
        let entries = shared
            .store
            .history_for_script(script)
            .map_err(|e| RpcError::server(e.to_string()))?;
        if entries.len() > MAX_HISTORY_ENTRIES {
            return Err(RpcError::server(format!(
                "history for this script has {} entries (limit {}); cannot resolve the spender",
                entries.len(),
                MAX_HISTORY_ENTRIES
            )));
        }
        let mut candidates = Vec::new();
        for (height, tx_pos, effect) in entries {
            if height < funder_height || effect.spent_sat == 0 {
                continue;
            }
            let rec = shared
                .store
                .get_block(height)
                .map_err(|e| RpcError::server(e.to_string()))?
                .ok_or_else(|| RpcError::server(format!("block {height} missing")))?;
            let candidate = shared
                .store
                .txid_at(rec.txnum_base + tx_pos as u64)
                .map_err(|e| RpcError::server(e.to_string()))?;
            candidates.push((candidate, height));
        }
        candidates
    };

    for (spender, height) in candidates {
        if spender == txid {
            continue;
        }
        let raw = resolve_raw(shared, spender, height)?;
        let tx = decode_raw_tx(&raw)?;
        if tx.input.iter().any(|input| {
            input.previous_output.txid.to_string() == txid.to_hex()
                && input.previous_output.vout == vout
        }) {
            return Ok(json!({
                "funder_height": funder_height,
                "spender_txhash": spender.to_hex(),
                "spender_height": height,
            }));
        }
    }

    Ok(json!({ "funder_height": funder_height }))
}

fn outpoint_get_status(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let (txid, vout, script) = param_outpoint(params)?;
    outpoint_status(shared, txid, vout, script)
}

fn outpoint_subscribe(
    shared: &Shared,
    client: Option<&Arc<ClientState>>,
    params: &Value,
) -> RpcResult<Value> {
    let (txid, vout, script) = param_outpoint(params)?;
    let status = outpoint_status(shared, txid, vout, script)?;
    if let Some(c) = client {
        let mut subs = c
            .outpoints
            .lock()
            .map_err(|_| RpcError::server("outpoint lock poisoned"))?;
        if subs.len() >= 1000 && !subs.contains_key(&(txid, vout)) {
            return Err(RpcError::server(
                "too many outpoint subscriptions on this connection (limit 1000)",
            ));
        }
        subs.insert(
            (txid, vout),
            OutpointSub {
                script,
                status: status.clone(),
            },
        );
    }
    Ok(status)
}

fn outpoint_unsubscribe(client: Option<&Arc<ClientState>>, params: &Value) -> RpcResult<Value> {
    let arr = params
        .as_array()
        .ok_or_else(|| RpcError::invalid("expected [tx_hash, txout_idx]"))?;
    let txid = arr
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid("expected tx_hash hex"))?;
    let txid = Hash32::from_hex(txid).map_err(RpcError::invalid)?;
    let vout = u32::try_from(
        arr.get(1)
            .and_then(Value::as_u64)
            .ok_or_else(|| RpcError::invalid("expected txout_idx"))?,
    )
    .map_err(|_| RpcError::invalid("txout_idx out of range"))?;
    let removed = client
        .and_then(|c| {
            c.outpoints
                .lock()
                .ok()
                .map(|mut subs| subs.remove(&(txid, vout)).is_some())
        })
        .unwrap_or(false);
    Ok(json!(removed))
}

/// Turn Core's `decoderawtransaction` result into the verbose object wallets
/// expect from `blockchain.transaction.get(txid, true)`: Core's decoder lacks
/// the chain context that `getrawtransaction` adds.
fn decorate_verbose_tx(
    decoded: &mut Value,
    raw: &str,
    block: &BlockRecord,
    tip: u32,
) -> RpcResult<()> {
    let header: bitcoin::block::Header = bitcoin::consensus::encode::deserialize(&block.header)
        .map_err(|e| {
            RpcError::server(format!(
                "invalid indexed header at height {}: {e}",
                block.height
            ))
        })?;
    let obj = decoded
        .as_object_mut()
        .ok_or_else(|| RpcError::server("decoderawtransaction returned a non-object"))?;
    obj.insert("hex".to_string(), json!(raw));
    obj.insert("blockhash".to_string(), json!(block.hash.to_hex()));
    obj.insert(
        "confirmations".to_string(),
        json!(u64::from(tip.saturating_sub(block.height)) + 1),
    );
    obj.insert("time".to_string(), json!(header.time));
    obj.insert("blocktime".to_string(), json!(header.time));
    obj.insert("in_active_chain".to_string(), json!(true));
    Ok(())
}

fn transaction_get(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let txid = params
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .ok_or_else(|| RpcError::invalid("expected txid"))?;
    let verbose = params
        .as_array()
        .and_then(|a| a.get(1))
        .map(|v| v.as_bool().unwrap_or(false))
        .unwrap_or(false);
    let txid_hash = Hash32::from_hex(txid).map_err(RpcError::invalid)?;
    let (occ, blockhash) = {
        let _g = read_guard(shared)?;
        let occ = shared
            .store
            .occurrences(txid_hash)
            .map_err(|e| RpcError::server(e.to_string()))?;
        let blockhash = match occ.first() {
            Some((height, _)) => shared
                .store
                .get_block(*height)
                .ok()
                .flatten()
                .map(|r| r.hash.to_hex()),
            None => None,
        };
        (occ, blockhash)
    };

    if verbose {
        // For a confirmed transaction use the verified resolver and decode the
        // raw bytes locally through Core. This works on a pruned node, where
        // getrawtransaction(txid, true, blockhash) would fail.
        if let Some((height, _)) = occ.first().copied() {
            let raw = {
                let _guard = rawtx_reader_guard(shared, height);
                shared
                    .resolver
                    .raw_tx(txid_hash, height)
                    .map_err(|e| RpcError::server(e.to_string()))?
            };
            let mut decoded = shared
                .core
                .decode_raw_transaction(&raw)
                .map_err(|e| RpcError::server(format!("rawtx decode: {e}")))?;
            let (block, tip) = {
                let _g = read_guard(shared)?;
                let block = shared
                    .store
                    .get_block(height)
                    .map_err(|e| RpcError::server(e.to_string()))?
                    .ok_or_else(|| RpcError::server(format!("block {height} missing")))?;
                let tip = shared
                    .store
                    .committed_tip()
                    .map_err(|e| RpcError::server(e.to_string()))?
                    .map(|t| t.height)
                    .unwrap_or(height);
                (block, tip)
            };
            decorate_verbose_tx(&mut decoded, &raw, &block, tip)?;
            return Ok(decoded);
        }
        let mut value = shared
            .core
            .raw_transaction_json(txid, blockhash.as_deref())
            .map_err(|e| RpcError::server(format!("rawtx unavailable: {e}")))?;
        if let Some(obj) = value.as_object_mut() {
            if !obj.contains_key("confirmations") {
                obj.insert("confirmations".to_string(), json!(0));
            }
        }
        return Ok(value);
    }

    match occ.first().copied() {
        Some((height, _)) => {
            // Keep the prune gate away from this height only when Core can
            // still serve it; external-only heights must not block pruning.
            let _guard = rawtx_reader_guard(shared, height);
            shared
                .resolver
                .raw_tx(txid_hash, height)
                .map(|raw| json!(raw))
                .map_err(|e| RpcError::server(e.to_string()))
        }
        None => shared
            .core
            .raw_transaction_hex_opt(txid, None)
            .map(|raw| json!(raw))
            .map_err(|e| RpcError::server(format!("transaction not found: {e}"))),
    }
}

fn mempool_info(shared: &Shared) -> RpcResult<Value> {
    let info = shared
        .core
        .mempool_info()
        .map_err(|e| RpcError::server(format!("mempool info: {e}")))?;
    Ok(json!({
        "mempoolminfee": info.mempoolminfee,
        "minrelaytxfee": info.minrelaytxfee,
        "incrementalrelayfee": info.incrementalrelayfee,
    }))
}

fn mempool_recent(shared: &Shared) -> RpcResult<Value> {
    let mem = shared
        .mempool
        .read()
        .map_err(|_| RpcError::server("mempool lock poisoned"))?;
    Ok(Value::Array(
        mem.recent(10)
            .into_iter()
            .map(|(txid, fee, vsize)| {
                json!({
                    "txid": txid.to_hex(),
                    "fee": fee,
                    "vsize": vsize,
                })
            })
            .collect(),
    ))
}

fn fee_histogram(shared: &Shared) -> RpcResult<Value> {
    let mem = shared
        .mempool
        .read()
        .map_err(|_| RpcError::server("mempool lock poisoned"))?;
    Ok(Value::Array(
        mem.fee_histogram()
            .into_iter()
            .map(|(rate, size)| json!([rate, size]))
            .collect(),
    ))
}

fn relay_fee(shared: &Shared) -> RpcResult<Value> {
    let mut fee = *shared
        .relayfee
        .lock()
        .map_err(|_| RpcError::server("fee lock poisoned"))?;
    if fee <= 0.0 {
        if let Ok(info) = shared.core.network_info() {
            if info.relayfee > 0.0 {
                fee = info.relayfee;
                *shared
                    .relayfee
                    .lock()
                    .map_err(|_| RpcError::server("fee lock poisoned"))? = fee;
            }
        }
    }
    if fee <= 0.0 {
        fee = 0.00001;
    }
    Ok(json!(fee))
}

fn estimate_fee(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let target = param_u32(params, 0)?.clamp(1, 1008);
    let mode = params
        .as_array()
        .and_then(|a| a.get(1))
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(mode) = &mode {
        if !matches!(mode.as_str(), "unset" | "economical" | "conservative") {
            return Err(RpcError::invalid(
                "estimatefee mode must be unset, economical or conservative",
            ));
        }
    }
    let key = (target, mode.clone());
    {
        let cache = shared
            .fee_estimates
            .lock()
            .map_err(|_| RpcError::server("fee lock poisoned"))?;
        if let Some((value, at)) = cache.get(&key) {
            if at.elapsed() < Duration::from_secs(30) {
                return Ok(json!(value));
            }
        }
    }

    let mut value = None;
    if let Ok(estimate) = shared.core.estimate_smart_fee(target, mode.as_deref()) {
        if let Some(rate) = estimate.feerate {
            if rate > 0.0 {
                value = Some(rate);
            }
        }
    }
    let value = value.unwrap_or_else(|| {
        // Never return -1: clients (Sparrow) treat it as a failure when sending.
        let mut fallback = *shared.relayfee.lock().unwrap_or_else(|e| e.into_inner());
        if fallback <= 0.0 {
            fallback = 0.00001;
        }
        if let Ok(info) = shared.core.mempool_info() {
            fallback = fallback.max(info.mempoolminfee);
        }
        fallback
    });
    shared
        .fee_estimates
        .lock()
        .map_err(|_| RpcError::server("fee lock poisoned"))?
        .insert(key, (value, Instant::now()));
    Ok(json!(value))
}

fn block_stats(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let height = param_u32(params, 0)? as u64;
    shared
        .core
        .block_stats(height)
        .map_err(|e| RpcError::server(e.to_string()))
}

fn broadcast_transaction(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let hex = params
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .ok_or_else(|| RpcError::invalid("expected raw transaction hex"))?;
    let txid = shared
        .core
        .send_raw_transaction(hex)
        .map_err(|e| RpcError::server(e.to_string()))?;
    Ok(json!(txid))
}

fn broadcast_package(
    shared: &Shared,
    params: &Value,
    version: ProtocolVersion,
) -> RpcResult<Value> {
    if version < V1_6 {
        return Err(RpcError::method(
            "blockchain.transaction.broadcast_package requires protocol 1.6",
        ));
    }
    let args = params
        .as_array()
        .ok_or_else(|| RpcError::invalid("expected [raw_txs, verbose]"))?;
    let raw = args
        .first()
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid("expected an array of raw transactions"))?;
    let mut raw_txs = Vec::with_capacity(raw.len());
    for tx in raw {
        raw_txs.push(
            tx.as_str()
                .ok_or_else(|| RpcError::invalid("raw transactions must be hex strings"))?
                .to_string(),
        );
    }
    if raw_txs.is_empty() {
        return Err(RpcError::invalid(
            "broadcast_package expects a non-empty array of raw transactions",
        ));
    }
    let verbose = args.get(1).and_then(Value::as_bool).unwrap_or(false);
    let result = shared
        .core
        .submit_package(&raw_txs)
        .map_err(|e| RpcError::server(format!("submitpackage: {e}")))?;
    Ok(if verbose {
        result
    } else {
        submit_package_summary(&result)
    })
}

/// Convert Core's `submitpackage` response to the stable 1.6 shape:
/// `{"success": bool, "errors": [{"txid", "error"}, ...]}`.
fn submit_package_summary(core: &Value) -> Value {
    let success = core.get("package_msg").and_then(Value::as_str) == Some("success");
    let mut errors = Vec::new();
    if let Some(results) = core.get("tx-results").and_then(Value::as_object) {
        for result in results.values() {
            if let Some(error) = result.get("error").and_then(Value::as_str) {
                errors.push(json!({
                    "txid": result.get("txid").cloned().unwrap_or(Value::Null),
                    "error": error,
                }));
            }
        }
    }
    if errors.is_empty() {
        json!({ "success": success })
    } else {
        json!({ "success": success, "errors": errors })
    }
}

fn test_mempool_accept(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let raw = params
        .as_array()
        .and_then(|a| a.first())
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid("expected an array of raw transactions"))?;
    let mut raw_txs = Vec::with_capacity(raw.len());
    for tx in raw {
        raw_txs.push(
            tx.as_str()
                .ok_or_else(|| RpcError::invalid("raw transactions must be hex strings"))?
                .to_string(),
        );
    }
    if raw_txs.is_empty() {
        return Err(RpcError::invalid(
            "testmempoolaccept expects a non-empty array of raw transactions",
        ));
    }
    let value = shared
        .core
        .test_mempool_accept(&raw_txs)
        .map_err(|e| RpcError::server(format!("testmempoolaccept: {e}")))?;
    // Electrum documents `reason`; Core names it `reject-reason`.
    let Some(items) = value.as_array() else {
        return Ok(value);
    };
    let mapped = items
        .iter()
        .map(|item| {
            let mut obj = item.as_object().cloned().unwrap_or_default();
            if let Some(reason) = obj.remove("reject-reason") {
                obj.insert("reason".to_string(), reason);
            }
            Value::Object(obj)
        })
        .collect();
    Ok(Value::Array(mapped))
}

fn silent_payments(shared: &Shared, method: &str) -> RpcResult<Value> {
    if !shared.silent_payments_enabled {
        return Err(RpcError::server(
            "silent payments are disabled by configuration ([silent_payments] enabled = false)",
        ));
    }
    // The opt-in gate exists so a future BIP352 indexer can be wired without
    // changing the protocol surface. Until then, fail explicitly.
    Err(RpcError::server(format!(
        "{method}: silent payments are enabled but this build has no BIP352 indexer (see docs/ADR-018)"
    )))
}

fn scripthash_mempool(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let script = param_scripthash(params)?;
    mempool_value(shared, script)
}

fn transaction_merkle(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let txid = params
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .ok_or_else(|| RpcError::invalid("expected txid"))?;
    let height = param_u32(params, 1)?;
    let txid_hash = Hash32::from_hex(txid).map_err(RpcError::invalid)?;
    let rec = shared
        .store
        .get_block(height)
        .map_err(|e| RpcError::server(e.to_string()))?
        .ok_or_else(|| RpcError::server(format!("height {height} not indexed")))?;
    let txids = shared
        .store
        .block_txids(&rec)
        .map_err(|e| RpcError::server(e.to_string()))?;
    let pos = txids
        .iter()
        .position(|t| *t == txid_hash)
        .ok_or_else(|| RpcError::server("transaction not in that block"))?;
    let (_, branch) = merkle_branch(&txids, pos)
        .ok_or_else(|| RpcError::server("cannot compute merkle branch"))?;
    Ok(json!({
        "block_height": height,
        "merkle": branch.iter().map(|h| h.to_hex()).collect::<Vec<_>>(),
        "pos": pos,
    }))
}

fn transaction_id_from_pos(shared: &Shared, params: &Value) -> RpcResult<Value> {
    let height = param_u32(params, 0)?;
    let pos = param_u32(params, 1)?;
    let want_merkle = params
        .as_array()
        .and_then(|a| a.get(2))
        .map(|v| v.as_bool().unwrap_or(false))
        .unwrap_or(false);
    let rec = shared
        .store
        .get_block(height)
        .map_err(|e| RpcError::server(e.to_string()))?
        .ok_or_else(|| RpcError::server(format!("height {height} not indexed")))?;
    let txids = shared
        .store
        .block_txids(&rec)
        .map_err(|e| RpcError::server(e.to_string()))?;
    let txid = txids
        .get(pos as usize)
        .ok_or_else(|| RpcError::server("position out of range"))?;
    if !want_merkle {
        return Ok(json!(txid.to_hex()));
    }
    let (_, branch) = merkle_branch(&txids, pos as usize)
        .ok_or_else(|| RpcError::server("cannot compute merkle branch"))?;
    Ok(json!({
        "tx_hash": txid.to_hex(),
        "merkle": branch.iter().map(|h| h.to_hex()).collect::<Vec<_>>(),
    }))
}

/// Poll Core and keep the mempool overlay fresh. Core remains the source of
/// truth; nothing is persisted.
pub fn start_mempool(shared: Arc<Shared>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut last_err = String::new();
        let mut last_fee = Instant::now()
            .checked_sub(Duration::from_secs(600))
            .unwrap_or_else(Instant::now);
        let mut last_tip = u64::MAX;
        let mut last_refresh = Instant::now()
            .checked_sub(Duration::from_secs(60))
            .unwrap_or_else(Instant::now);
        let mut ever_ok = false;
        loop {
            if shared.shutdown.load(Ordering::Relaxed) {
                break;
            }
            // Reconcile immediately after a new block; otherwise poll every 3 s.
            let tip = shared
                .store
                .committed_tip()
                .ok()
                .flatten()
                .map(|t| t.height as u64)
                .unwrap_or(0);
            let tip_changed = tip != last_tip;
            if !tip_changed && last_refresh.elapsed() < Duration::from_secs(3) {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            // Build the candidate outside the write lock: Core RPC and the
            // per-input UTXO lookups can take seconds on a large mempool, and
            // holding `mempool.write()` would block every history/balance read.
            let mut candidate = match shared.mempool.read() {
                Ok(m) => m.clone(),
                Err(_) => break,
            };
            let result = candidate.refresh(&shared.core, &shared.store);
            last_tip = tip;
            last_refresh = Instant::now();
            match result {
                Ok(changed) => {
                    ever_ok = true;
                    if changed {
                        let touched: Vec<ScriptHash> =
                            candidate.last_touched().iter().copied().collect();
                        if let Ok(mut mem) = shared.mempool.write() {
                            *mem = candidate;
                        }
                        shared.dirty_scripts.mark_all(touched);
                    }
                    if !last_err.is_empty() {
                        eprintln!("[e-lite] mempool refresh recovered");
                        last_err.clear();
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    let starting = matches!(
                        shared.get_state(),
                        state::WAITING_FOR_CORE | state::INITIALIZING
                    );
                    let connection_noise = msg.contains("Connection refused")
                        || msg.contains("io:")
                        || msg.contains("Loading block index")
                        || msg.contains("Verifying blocks");
                    // Before the first successful refresh (serve offline, Core
                    // still loading), connection errors are expected and must
                    // not look like failures. After a successful refresh they
                    // are logged once, so a real outage is visible.
                    let expected = !ever_ok && connection_noise;
                    if msg != last_err && !starting && !expected {
                        eprintln!("[e-lite] mempool refresh: {msg}");
                    }
                    last_err = msg;
                }
            }
            if last_fee.elapsed() >= Duration::from_secs(60) {
                if let Ok(info) = shared.core.network_info() {
                    if info.relayfee > 0.0 {
                        if let Ok(mut fee) = shared.relayfee.lock() {
                            *fee = info.relayfee;
                        }
                    }
                }
                last_fee = Instant::now();
            }
            std::thread::sleep(Duration::from_secs(3));
        }
    })
}

#[derive(Clone)]
struct OutpointJob {
    client: Arc<ClientState>,
    txid: Hash32,
    vout: u32,
    force: bool,
}

type OutpointKey = (u64, Hash32, u32);

/// Bounded worker pool for outpoint status computation. Outpoint status may
/// need to resolve a raw funding/spending transaction, which can touch Core or
/// external providers; doing that in the notifier thread would delay every
/// other wallet's notifications.
struct OutpointNotifier {
    tx: mpsc::SyncSender<OutpointJob>,
    pending: Arc<Mutex<HashMap<OutpointKey, (OutpointJob, bool)>>>,
}

impl OutpointNotifier {
    fn new(shared: Arc<Shared>, workers: usize, queue: usize) -> Self {
        let (tx, rx) = mpsc::sync_channel::<OutpointJob>(queue.max(1));
        let rx = Arc::new(Mutex::new(rx));
        let pending = Arc::new(Mutex::new(HashMap::new()));
        for _ in 0..workers.max(1) {
            let shared = Arc::clone(&shared);
            let rx = Arc::clone(&rx);
            let pending = Arc::clone(&pending);
            std::thread::spawn(move || loop {
                let job = {
                    let guard = match rx.lock() {
                        Ok(g) => g,
                        Err(_) => return,
                    };
                    guard.recv()
                };
                let Ok(job) = job else {
                    return;
                };
                let key = (job.client.id, job.txid, job.vout);
                if let Ok(mut pending) = pending.lock() {
                    pending.remove(&key);
                }
                if !job.client.alive.load(Ordering::SeqCst) {
                    continue;
                }
                let sub = job
                    .client
                    .outpoints
                    .lock()
                    .ok()
                    .and_then(|subs| subs.get(&(job.txid, job.vout)).cloned());
                let Some(sub) = sub else {
                    continue;
                };
                let Ok(new) = outpoint_status(&shared, job.txid, job.vout, sub.script) else {
                    continue;
                };
                if new != sub.status || job.force {
                    let notification = json!({
                        "jsonrpc": "2.0",
                        "method": "blockchain.outpoint.subscribe",
                        "params": [job.txid.to_hex(), job.vout, new],
                    });
                    if job.client.send(&notification) {
                        shared
                            .stats
                            .notifications_sent
                            .fetch_add(1, Ordering::Relaxed);
                        if let Ok(mut subs) = job.client.outpoints.lock() {
                            if let Some(entry) = subs.get_mut(&(job.txid, job.vout)) {
                                entry.status = new;
                            }
                        }
                    } else {
                        shared.stats.notifier_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
        Self { tx, pending }
    }

    fn enqueue(&self, job: OutpointJob) {
        let key = (job.client.id, job.txid, job.vout);
        if let Ok(mut pending) = self.pending.lock() {
            if pending.contains_key(&key) {
                return;
            }
            pending.insert(key, (job, false));
        } else {
            return;
        }
        self.flush();
    }

    /// Retry jobs that did not fit in the bounded queue.
    fn flush(&self) {
        let mut pending = match self.pending.lock() {
            Ok(p) => p,
            Err(_) => return,
        };
        for (key, (job, queued)) in pending.iter_mut() {
            if *queued {
                continue;
            }
            if self.tx.try_send(job.clone()).is_ok() {
                *queued = true;
            } else {
                // Keep the job for the next loop.
                let _ = key;
                break;
            }
        }
    }
}

/// Push notifications for subscribed clients (headers and scripthash status).
/// Recomputed only when the tip or the mempool overlay changes.
pub fn start_notifications(shared: Arc<Shared>) -> std::thread::JoinHandle<()> {
    let outpoints = OutpointNotifier::new(Arc::clone(&shared), 2, 256);
    std::thread::spawn(move || {
        let mut last_height = u64::MAX;
        let mut last_hash: Option<String> = None;
        loop {
            if shared.shutdown.load(Ordering::Relaxed) {
                break;
            }
            shared.stats.notifier_runs.fetch_add(1, Ordering::Relaxed);
            let (height, hash, header) = {
                let Ok(_g) = shared.lock.read() else {
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                };
                let tip = shared.store.committed_tip().ok().flatten();
                match tip {
                    Some(t) => (
                        t.height as u64,
                        Some(t.block_hash.to_hex()),
                        shared
                            .store
                            .get_block(t.height)
                            .ok()
                            .flatten()
                            .map(|r| hex::encode(r.header)),
                    ),
                    None => (0, None, None),
                }
            };
            let new_block = height != last_height || hash != last_hash;
            let (dirty, force_all) = shared.dirty_scripts.take();
            // A rewind or a same-height fork can change scripts that are not in
            // the new block's dirty set, so recompute every subscription.
            let reorg =
                (new_block && height < last_height) || (height == last_height && hash != last_hash);
            let scan_scripts = force_all || reorg || !dirty.is_empty();
            // Retry outpoint jobs that did not fit in the queue, even when no
            // new block arrived.
            outpoints.flush();
            if !new_block && !scan_scripts {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
            for client in shared.clients.snapshot() {
                if !client.alive.load(Ordering::SeqCst) {
                    continue;
                }
                if new_block && client.headers.load(Ordering::SeqCst) {
                    if let Some(h) = &header {
                        let notification = json!({
                            "jsonrpc": "2.0",
                            "method": "blockchain.headers.subscribe",
                            "params": [{"height": height, "hex": h}],
                        });
                        if client.send(&notification) {
                            shared
                                .stats
                                .notifications_sent
                                .fetch_add(1, Ordering::Relaxed);
                            client.last_height.store(height, Ordering::SeqCst);
                        } else {
                            shared.stats.notifier_errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                if !scan_scripts {
                    continue;
                }
                // Scripthash and scriptpubkey subscriptions are independent:
                // a client may use both for the same script and must receive
                // one notification per method.
                let (sh_subs, spk_subs) = {
                    let sh = client
                        .subs
                        .lock()
                        .map(|s| s.iter().map(|(k, v)| (*k, v.clone())).collect::<Vec<_>>())
                        .unwrap_or_default();
                    let spk = client
                        .spk_subs
                        .lock()
                        .map(|s| s.iter().map(|(k, v)| (*k, v.clone())).collect::<Vec<_>>())
                        .unwrap_or_default();
                    (sh, spk)
                };
                for (script, old, method, force_reorg) in sh_subs
                    .into_iter()
                    .map(|(s, o)| (s, o, "blockchain.scripthash.subscribe", false))
                    .chain(
                        spk_subs
                            .into_iter()
                            .map(|(s, o)| (s, o, "blockchain.scriptpubkey.subscribe", reorg)),
                    )
                {
                    if !(force_all || reorg || dirty.contains(&script)) {
                        continue;
                    }
                    let new = {
                        let Ok(_g) = shared.lock.read() else {
                            continue;
                        };
                        match script_status(&shared, script) {
                            Ok(v) => v,
                            Err(_) => continue,
                        }
                    };
                    if new != old || force_reorg {
                        let notification = json!({
                            "jsonrpc": "2.0",
                            "method": method,
                            "params": [script.to_electrum_hex(), new],
                        });
                        if client.send(&notification) {
                            shared
                                .stats
                                .notifications_sent
                                .fetch_add(1, Ordering::Relaxed);
                            let store = if method == "blockchain.scripthash.subscribe" {
                                &client.subs
                            } else {
                                &client.spk_subs
                            };
                            if let Ok(mut subs) = store.lock() {
                                subs.insert(script, new);
                            }
                        } else {
                            shared.stats.notifier_errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }

                let outpoint_subs: Vec<((Hash32, u32), OutpointSub)> = client
                    .outpoints
                    .lock()
                    .map(|s| s.iter().map(|(k, v)| (*k, v.clone())).collect())
                    .unwrap_or_default();
                for ((txid, vout), sub) in outpoint_subs {
                    if !(force_all || reorg || dirty.contains(&sub.script)) {
                        continue;
                    }
                    // Status resolution may need Core or an external provider;
                    // keep it off the notifier thread.
                    outpoints.enqueue(OutpointJob {
                        client: Arc::clone(&client),
                        txid,
                        vout,
                        force: reorg,
                    });
                }
            }
            last_height = height;
            last_hash = hash;
            std::thread::sleep(Duration::from_secs(2));
        }
    })
}

// ------------------------------------------------------------------- transports

pub fn start_server(shared: Arc<Shared>) -> std::io::Result<std::thread::JoinHandle<()>> {
    let listener = TcpListener::bind(&shared.listen)?;
    Ok(start_listener_on(listener, shared, None))
}

/// Serve an already-bound listener. When `tls` is set, every accepted
/// connection is wrapped in TLS before it reaches the JSON-RPC loop.
pub fn start_listener_on(
    listener: TcpListener,
    shared: Arc<Shared>,
    tls: Option<Arc<CertWatcher>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let stream = match incoming {
                Ok(s) => s,
                Err(_) => continue,
            };
            // Reserve the slot before spawning: checking and incrementing
            // inside the child thread lets a burst exceed max_clients.
            let before = shared.stats.clients.fetch_add(1, Ordering::SeqCst);
            if before >= shared.max_clients {
                shared.stats.clients.fetch_sub(1, Ordering::SeqCst);
                let _ = stream.shutdown(std::net::Shutdown::Both);
                continue;
            }
            let wire = match &tls {
                Some(watcher) => Wire::tls(watcher.config(), stream),
                None => Wire::plain(stream),
            };
            let wire = match wire {
                Ok(w) => w,
                Err(_) => {
                    if tls.is_some() {
                        shared
                            .stats
                            .tls_handshake_failures
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    shared.stats.clients.fetch_sub(1, Ordering::SeqCst);
                    continue;
                }
            };
            let client = shared.clients.register(wire);
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                let _ = handle_client(&shared, client);
                shared.stats.clients.fetch_sub(1, Ordering::SeqCst);
            });
        }
    })
}

/// Reads from the client while releasing the connection lock between attempts,
/// so the notifier thread can write to the same TLS connection.
struct WireReader {
    client: Arc<ClientState>,
    idle_deadline: Instant,
}

impl WireReader {
    fn new(client: Arc<ClientState>) -> Self {
        Self {
            client,
            idle_deadline: Instant::now() + CLIENT_IDLE_TIMEOUT,
        }
    }
}

impl Read for WireReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            {
                let mut wire = self
                    .client
                    .writer
                    .lock()
                    .map_err(|_| std::io::Error::other("connection lock poisoned"))?;
                match wire.read(buf) {
                    Ok(n) => {
                        self.idle_deadline = Instant::now() + CLIENT_IDLE_TIMEOUT;
                        return Ok(n);
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(e) => return Err(e),
                }
            }
            if Instant::now() >= self.idle_deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "client idle timeout",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Read one newline-terminated line without allowing the buffer to exceed
/// `max` bytes. `Ok(false)` means clean EOF.
fn read_line_limited<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<bool> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(!buf.is_empty());
        }
        if let Some(pos) = available.iter().position(|b| *b == b'\n') {
            let take = pos + 1;
            if buf.len().saturating_add(take) > max {
                reader.consume(take);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "request line too large",
                ));
            }
            buf.extend_from_slice(&available[..take]);
            reader.consume(take);
            return Ok(true);
        }
        let take = available.len();
        if buf.len().saturating_add(take) > max {
            reader.consume(take);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request line too large",
            ));
        }
        buf.extend_from_slice(available);
        reader.consume(take);
    }
}

fn handle_client(shared: &Shared, client: Arc<ClientState>) -> std::io::Result<()> {
    let result = client_loop(shared, &client);
    client.alive.store(false, Ordering::SeqCst);
    shared.clients.unregister(client.id);
    result
}

fn client_loop(shared: &Shared, client: &Arc<ClientState>) -> std::io::Result<()> {
    if client.tls {
        let handshake = {
            let mut wire = client
                .writer
                .lock()
                .map_err(|_| std::io::Error::other("connection lock poisoned"))?;
            wire.handshake()
        };
        if let Err(e) = handshake {
            shared
                .stats
                .tls_handshake_failures
                .fetch_add(1, Ordering::Relaxed);
            return Err(e);
        }
        shared.stats.tls_handshakes.fetch_add(1, Ordering::Relaxed);
    }

    let mut reader = BufReader::new(WireReader::new(Arc::clone(client)));
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match read_line_limited(&mut reader, &mut buf, MAX_REQUEST_BYTES) {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                let _ = client.send(&json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {"code": -32600, "message": "request too large"},
                }));
                break;
            }
            Err(_) => break,
        }
        let line = String::from_utf8_lossy(&buf);
        if line.trim().is_empty() {
            continue;
        }
        let outcome = process_line_for(shared, Some(client), &line);
        if let Some(response) = outcome.response {
            if !client.send(&response) {
                break;
            }
        }
        if outcome.close {
            break;
        }
    }
    Ok(())
}

/// Result of one line from the wire. Notifications have no response; a fatal
/// protocol error asks the caller to close the connection after replying.
pub struct LineOutcome {
    pub response: Option<Value>,
    pub close: bool,
}

pub fn process_line(shared: &Shared, line: &str) -> Value {
    process_line_for(shared, None, line)
        .response
        .unwrap_or(Value::Null)
}

fn negotiated_client_version(client: Option<&Arc<ClientState>>) -> ProtocolVersion {
    if let Some(c) = client {
        if let Ok(protocol) = c.protocol.lock() {
            if let Some(version) = *protocol {
                return version;
            }
        }
    }
    SERVER_MIN
}

pub fn process_line_for(
    shared: &Shared,
    client: Option<&Arc<ClientState>>,
    line: &str,
) -> LineOutcome {
    shared.stats.requests_total.fetch_add(1, Ordering::Relaxed);
    let request: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            shared.stats.errors_total.fetch_add(1, Ordering::Relaxed);
            return LineOutcome {
                response: Some(json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {"code": -32700, "message": format!("parse error: {e}")},
                })),
                close: false,
            };
        }
    };
    let is_notification = request
        .as_object()
        .map(|obj| !obj.contains_key("id"))
        .unwrap_or(false);
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = match request.get("method").and_then(|m| m.as_str()) {
        Some(m) => m,
        None => {
            shared.stats.errors_total.fetch_add(1, Ordering::Relaxed);
            return LineOutcome {
                response: Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32600, "message": "invalid request"},
                })),
                close: false,
            };
        }
    };
    let params = request.get("params").cloned().unwrap_or(json!([]));
    let throttled = client
        .map(|c| {
            c.budget
                .lock()
                .map(|mut budget| budget.acquire(Instant::now()))
                .unwrap_or(false)
        })
        .unwrap_or(false);
    if throttled {
        // Soft throttle: the request already waited for its token. Keep the
        // counter for observability, but never turn a wallet burst into an
        // error.
        shared.stats.rate_limited.fetch_add(1, Ordering::Relaxed);
    }
    if method != "server.version" {
        if let Some(c) = client {
            c.saw_message.store(true, Ordering::SeqCst);
        }
    }
    let version = negotiated_client_version(client);
    match dispatch_for(shared, client, version, method, &params) {
        Ok(result) => LineOutcome {
            response: if is_notification {
                None
            } else {
                Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            },
            close: false,
        },
        Err(e) => {
            shared.stats.errors_total.fetch_add(1, Ordering::Relaxed);
            log_rpc_error(shared, method, e.code);
            LineOutcome {
                response: if is_notification {
                    None
                } else {
                    Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": e.code, "message": e.message},
                    }))
                },
                close: e.close,
            }
        }
    }
}

/// Local /status and /metrics endpoint. Loopback only by default.
pub fn start_metrics(
    shared: Arc<Shared>,
    addr: String,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let listener = TcpListener::bind(&addr)?;
    let handle = std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let mut stream = match incoming {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut buf = [0u8; 2048];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("/");
            let (ctype, body) = match path {
                "/status" => ("application/json", shared.status_json().to_string()),
                "/healthz" => ("text/plain", "ok\n".to_string()),
                "/metrics" => ("text/plain; version=0.0.4", metrics_text(&shared)),
                _ => (
                    "text/plain",
                    "e-lite: try /status, /metrics or /healthz\n".to_string(),
                ),
            };
            let resp = format!(
                "HTTP/1.0 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    Ok(handle)
}

fn metrics_text(shared: &Shared) -> String {
    let st = shared.status_json();
    let g = |k: &str| st[k].as_u64().unwrap_or(0);
    let mut out = String::new();
    let mut line = |name: &str, help: &str, kind: &str, value: u64| {
        out.push_str(&format!(
            "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}\n"
        ));
    };
    line(
        "e_lite_indexed_height",
        "Indexed tip height",
        "gauge",
        g("indexed_height"),
    );
    line(
        "e_lite_core_height",
        "Bitcoin Core tip height",
        "gauge",
        g("core_height"),
    );
    line(
        "e_lite_lag_blocks",
        "Blocks behind Core",
        "gauge",
        g("lag_blocks"),
    );
    line(
        "e_lite_core_pruneheight",
        "Lowest block still available in Core",
        "gauge",
        st["core"]["pruneheight"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_core_retention_blocks",
        "Blocks Core can still serve before pruning",
        "gauge",
        g("retention_blocks"),
    );
    line(
        "e_lite_blocks_applied",
        "Blocks applied to the index",
        "counter",
        g("blocks_applied"),
    );
    line(
        "e_lite_tuning_readers",
        "Effective parallel fetch readers",
        "gauge",
        st["tuning"]["readers"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_tuning_commit_interval_ms",
        "Effective commit interval",
        "gauge",
        st["tuning"]["commit_interval_ms"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_tuning_decode_ms",
        "Block decode/hash latency (EWMA)",
        "gauge",
        st["tuning"]["decode_ms_ewma"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_tuning_wait_ms",
        "Pipeline wait latency (EWMA)",
        "gauge",
        st["tuning"]["wait_ms_ewma"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_tuning_cache_mib",
        "Effective RocksDB block cache",
        "gauge",
        st["tuning"]["cache_mib"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_rawtx_cache_hits",
        "Raw transactions served from the validated block cache",
        "counter",
        st["rawtx"]["cache_hits"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_rawtx_fetches",
        "Whole blocks fetched from external sources",
        "counter",
        st["rawtx"]["fetches"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_rawtx_unavailable",
        "Raw transactions that could not be resolved",
        "counter",
        st["rawtx"]["unavailable"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_rawtx_decoys_inflight",
        "Decoy block downloads currently in flight",
        "gauge",
        st["rawtx"]["decoys_inflight"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_rawtx_p2p_failures",
        "P2P peer fetch failures",
        "counter",
        st["rawtx"]["p2p_failures"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_rawtx_p2p_skipped",
        "P2P fetches skipped because every peer was cooling down",
        "counter",
        st["rawtx"]["p2p_skipped"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_prefetch_queued",
        "History blocks queued for background prefetch",
        "counter",
        st["prefetch"]["queued"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_prefetch_done",
        "History blocks processed by the prefetcher",
        "counter",
        st["prefetch"]["done"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_memory_available_mib",
        "Memory available to e-lite (cgroup aware)",
        "gauge",
        st["memory"]["available_mib"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_memory_total_mib",
        "Memory visible to e-lite (cgroup aware)",
        "gauge",
        st["memory"]["total_mib"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_tx_per_sec",
        "Indexed transactions per second (EWMA)",
        "gauge",
        st["tuning"]["tx_per_sec"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_blocks_per_sec",
        "Indexed blocks per second (EWMA)",
        "gauge",
        st["tuning"]["blocks_per_sec"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_ready",
        "1 when the index has caught up to Core's validated blocks",
        "gauge",
        if st["ready"].as_bool().unwrap_or(false) {
            1
        } else {
            0
        },
    );
    line(
        "e_lite_fully_synced",
        "1 when Core is out of IBD and the index is at the network tip",
        "gauge",
        if st["fully_synced"].as_bool().unwrap_or(false) {
            1
        } else {
            0
        },
    );
    line(
        "e_lite_commit_ms",
        "Commit latency (EWMA)",
        "gauge",
        st["tuning"]["commit_ms_ewma"].as_u64().unwrap_or(0),
    );
    line(
        "e_lite_disk_low",
        "1 when the disk guard has paused indexing",
        "gauge",
        if st["disk_low"].as_bool().unwrap_or(false) {
            1
        } else {
            0
        },
    );
    line(
        "e_lite_txnum",
        "Indexed transaction occurrences",
        "counter",
        g("txnum"),
    );
    line(
        "e_lite_db_bytes",
        "Index size on disk",
        "gauge",
        g("db_bytes"),
    );
    line(
        "e_lite_disk_free_bytes",
        "Free disk on the data directory",
        "gauge",
        g("disk_free_bytes"),
    );
    line(
        "e_lite_server_clients",
        "Connected Electrum clients",
        "gauge",
        g("clients"),
    );
    line(
        "e_lite_server_requests_total",
        "Electrum requests served",
        "counter",
        g("requests_total"),
    );
    line(
        "e_lite_tls_handshakes_total",
        "Completed Electrum TLS handshakes",
        "counter",
        g("tls_handshakes"),
    );
    line(
        "e_lite_tls_handshake_failures_total",
        "Failed Electrum TLS handshakes",
        "counter",
        g("tls_handshake_failures"),
    );
    line(
        "e_lite_errors_total",
        "Protocol and internal errors",
        "counter",
        g("errors_total"),
    );
    line(
        "e_lite_rate_limited_total",
        "Requests delayed by the per-client rate limiter",
        "counter",
        g("rate_limited"),
    );
    line(
        "e_lite_providers_healthy",
        "Public providers that answered the startup probe",
        "gauge",
        g("providers_healthy"),
    );
    line(
        "e_lite_last_block_ms",
        "Duration of the last applied block",
        "gauge",
        g("last_block_ms"),
    );
    line(
        "e_lite_uptime_seconds",
        "Process uptime",
        "gauge",
        g("uptime_seconds"),
    );
    let state = shared.get_state();
    out.push_str("# HELP e_lite_state Server state (0 blocking values)\n");
    out.push_str("# TYPE e_lite_state gauge\n");
    out.push_str(&format!(
        "e_lite_state{{state=\"{}\"}} {}\n",
        state::name(state),
        { state }
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_negotiation() {
        assert_eq!(
            server_version(&json!(["client", "1.4"]), None).unwrap(),
            json!([format!("e-lite {VERSION}"), "1.4"])
        );
        assert_eq!(
            server_version(&json!(["client", ["1.4", "1.4.2"]]), None).unwrap()[1],
            json!("1.4.2")
        );
        assert!(server_version(&json!(["client", ["1.4", "1.4"]]), None).is_ok());
        assert!(server_version(&json!(["client", "1.2"]), None).is_err());
        assert!(server_version(&json!(["client", ["1.0", "1.2"]]), None).is_err());
        // Gaps and future versions fall back to the previous available one.
        for requested in ["1.4.3", "1.5"] {
            assert_eq!(
                server_version(&json!(["client", requested]), None).unwrap()[1],
                json!("1.4.2"),
                "{requested} must fall back to 1.4.2"
            );
        }
        assert_eq!(
            server_version(&json!(["client", "1.6"]), None).unwrap()[1],
            json!("1.6"),
            "1.6 must negotiate 1.6"
        );
        for requested in ["1.7", "1.8"] {
            assert_eq!(
                server_version(&json!(["client", requested]), None).unwrap()[1],
                json!("1.7"),
                "{requested} must negotiate the newest available version below it"
            );
        }
        assert_eq!(
            server_version(&json!(["client", ["1.5", "1.8"]]), None).unwrap()[1],
            json!("1.7")
        );
        // An inverted range is a parameter error, not a version mismatch.
        assert_eq!(
            server_version(&json!(["client", ["1.6", "1.4"]]), None)
                .unwrap_err()
                .code,
            -32602
        );
        // Extraneous arguments are tolerated for future protocol extensions.
        assert!(server_version(&json!(["client", "1.4", "junk", 9]), None).is_ok());
    }

    #[test]
    fn verbose_transaction_gets_chain_context() {
        let block = BlockRecord {
            height: 7,
            hash: Hash32([0x11; 32]),
            header: [0u8; 80],
            tx_count: 1,
            txnum_base: 0,
        };
        let mut decoded = json!({"txid": "aa", "vout": []});
        decorate_verbose_tx(&mut decoded, "deadbeef", &block, 9).unwrap();
        assert_eq!(decoded["hex"], json!("deadbeef"));
        assert_eq!(decoded["blockhash"], json!(block.hash.to_hex()));
        assert_eq!(decoded["confirmations"], json!(3));
        assert_eq!(decoded["time"], json!(0));
        assert_eq!(decoded["blocktime"], json!(0));
        assert_eq!(decoded["in_active_chain"], json!(true));
    }

    #[test]
    fn submit_package_summary_maps_core_errors() {
        let success = json!({
            "package_msg": "success",
            "tx-results": {"wtxid": {"txid": "aa"}}
        });
        assert_eq!(submit_package_summary(&success), json!({"success": true}));

        let failure = json!({
            "package_msg": "transaction failed",
            "tx-results": {
                "wtxid": {"txid": "aa", "error": "min relay fee not met"}
            }
        });
        assert_eq!(
            submit_package_summary(&failure),
            json!({
                "success": false,
                "errors": [{"txid": "aa", "error": "min relay fee not met"}]
            })
        );
    }

    #[test]
    fn rate_limit_waits_instead_of_rejecting() {
        let start = Instant::now();
        let mut limit = RateLimit::new(1, 10);
        assert!(!limit.acquire(start), "first request is immediate");
        let before = Instant::now();
        assert!(limit.acquire(start), "second request waits for a token");
        assert!(
            before.elapsed() >= Duration::from_millis(80),
            "the limiter must delay, not reject"
        );

        let mut disabled = RateLimit::new(1, 0);
        for _ in 0..10 {
            assert!(!disabled.acquire(start), "rate 0 never throttles");
        }
    }

    #[test]
    fn dirty_scripts_dedupe_and_force_all() {
        let dirty = DirtyScripts::new();
        let a = ScriptHash::from_script(b"a");
        let b = ScriptHash::from_script(b"b");
        dirty.mark(a);
        dirty.mark(a);
        dirty.mark_all([b]);
        let (set, force) = dirty.take();
        assert_eq!(set.len(), 2);
        assert!(set.contains(&a));
        assert!(set.contains(&b));
        assert!(!force);
        let (set, force) = dirty.take();
        assert!(set.is_empty());
        assert!(!force);

        dirty.mark_all_subscriptions();
        let (set, force) = dirty.take();
        assert!(set.is_empty());
        assert!(force);
    }

    #[test]
    fn request_line_is_bounded() {
        use std::io::Cursor;
        let mut r = Cursor::new(b"{\"a\":1}\nnext\n".to_vec());
        let mut buf = Vec::new();
        assert!(read_line_limited(&mut r, &mut buf, 64).unwrap());
        assert_eq!(buf, b"{\"a\":1}\n");
        buf.clear();
        assert!(read_line_limited(&mut r, &mut buf, 64).unwrap());
        assert_eq!(buf, b"next\n");

        let mut r = Cursor::new(vec![b'x'; 100]);
        let mut buf = Vec::new();
        assert!(read_line_limited(&mut r, &mut buf, 16).is_err());
    }

    #[test]
    fn scripthash_param_is_reversed() {
        let sh = ScriptHash::from_script(b"\x51");
        let v = json!([sh.to_electrum_hex()]);
        assert_eq!(param_scripthash(&v).unwrap(), sh);
        assert!(param_scripthash(&json!(["zz"])).is_err());
    }

    #[test]
    fn banner_sections_and_formatting() {
        let (indexing, ready) =
            split_banner("[indexing]\nINDEX {height}\n[ready]\nREADY {percent}\n");
        assert_eq!(indexing.as_deref(), Some("INDEX {height}\n"));
        assert_eq!(ready.as_deref(), Some("READY {percent}\n"));
        let (both, _) = split_banner("plain {state}\n");
        assert_eq!(both.as_deref(), Some("plain {state}\n"));
        assert_eq!(progress_bar(50.0), "[###############---------------]");
        assert_eq!(format_num(974_584), "974,584");
        assert_eq!(eta_text(0, 10), "0m");
        assert_eq!(eta_text(100, 0), "unknown");
    }

    #[test]
    fn tls_config_rejects_missing_and_invalid_files() {
        assert!(build_tls_config(
            Path::new("/nonexistent/e-lite-cert.pem"),
            Path::new("/nonexistent/e-lite-key.pem"),
        )
        .is_err());

        let dir = std::env::temp_dir().join(format!(
            "e-lite-tls-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cert = dir.join("cert.pem");
        std::fs::write(&cert, b"not a certificate").unwrap();
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let key = dir.join("key.pem");
        std::fs::copy(fixtures.join("tls-key.pem"), &key).unwrap();
        assert!(build_tls_config(&cert, &key).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tls_watcher_reloads_changed_pem_and_keeps_old_on_error() {
        let dir = std::env::temp_dir().join(format!(
            "e-lite-tls-watch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        std::fs::copy(fixtures.join("tls-cert.pem"), &cert).unwrap();
        std::fs::copy(fixtures.join("tls-key.pem"), &key).unwrap();

        let watcher = CertWatcher::load(cert.clone(), key, Duration::ZERO).unwrap();
        let first = watcher.config();

        // A new mtime with the same valid certificate rebuilds the config.
        filetime::set_file_mtime(&cert, filetime::FileTime::from_unix_time(2_000_000_000, 0))
            .unwrap();
        let second = watcher.config();
        assert!(!Arc::ptr_eq(&first, &second));

        // A corrupt certificate keeps the previous configuration in use.
        std::fs::write(&cert, b"not a certificate").unwrap();
        filetime::set_file_mtime(&cert, filetime::FileTime::from_unix_time(2_000_000_001, 0))
            .unwrap();
        let third = watcher.config();
        assert!(Arc::ptr_eq(&second, &third));

        // Restoring a valid certificate recovers.
        std::fs::copy(fixtures.join("tls-cert.pem"), &cert).unwrap();
        filetime::set_file_mtime(&cert, filetime::FileTime::from_unix_time(2_000_000_002, 0))
            .unwrap();
        let fourth = watcher.config();
        assert!(!Arc::ptr_eq(&third, &fourth));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
