//! Raw transaction resolution for confirmed history.
//!
//! Order: local Core (zero leak) -> validated disk cache -> external sources
//! (P2P and/or Esplora) fetched as **whole blocks**, verified against the local
//! header chain, then cached. External fetches must go through Tor unless the
//! operator explicitly allows clearnet.

pub mod cache;
pub mod cachelayout;
pub mod core_source;
pub mod decoy_wallets;
pub mod electrum;
pub mod esplora;
pub(crate) mod health;
pub mod p2p;
pub mod privacy;
mod race;
mod socks;
pub mod txbatch;
pub mod txcache;
pub mod txsource;
pub mod verify;

use el_model::sync::Mutex;
use el_model::{Chain, Hash32};
use el_store::Store;
use rand::seq::SliceRandom;
use rand::thread_rng;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::net::TcpStream;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar};
use std::time::{Duration, Instant};

/// Extra `debug:` lines (per-block failures of the background prefetch),
/// mirrored from the binary's `[logging] level`.
static DEBUG_LOGS: AtomicBool = AtomicBool::new(false);

/// Enable/disable the rawtx `debug:` lines.
pub fn set_debug_logs(on: bool) {
    DEBUG_LOGS.store(on, Ordering::Relaxed);
}

fn debug_logs() -> bool {
    DEBUG_LOGS.load(Ordering::Relaxed)
}

/// HTTP `User-Agent` for every external request. A generic value: the
/// project name and version would put the operator in a tiny anonymity set
/// for every provider (AUD-133).
pub const HTTP_USER_AGENT: &str = "Mozilla/5.0";

/// P2P `version` user agent. A common Core subversion instead of an
/// e-lite-specific one, so spy nodes cannot single out e-lite clients by
/// the cheapest filter (AUD-133). Behaviour (one getdata, then disconnect)
/// can still be fingerprinted.
pub const P2P_USER_AGENT: &[u8] = b"/Satoshi:29.0.0/";

pub use cache::BlockCache;
pub use cachelayout::{CacheInventory, CACHE_DIR_NAME};
pub use txcache::TxCache;

/// Verified blocks kept in memory (external and local Core): a burst of
/// `transaction.get` from one block costs one download or one `getblock`.
/// A mainnet block decodes to a few MiB, so 24 stay below ~100 MiB.
const RECENT_BLOCKS: usize = 24;

/// Where a verified block came from; decoys follow external fetches only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    Recent,
    BlockCache,
    Local,
    External,
}

/// Split the `cache_mib` disk budget: `(block_mib, tx_mib)`. Whole blocks
/// get a quarter, at least 1 and at most 64 MiB; transactions the rest.
pub fn cache_split(cache_mib: u64) -> (u64, u64) {
    if cache_mib == 0 {
        return (0, 0);
    }
    let block = (cache_mib / 4).clamp(1, 64);
    (block, cache_mib - block)
}

/// Full verification of a block against the indexed record.
fn verify_full(bytes: &[u8], rec: &el_model::BlockRecord) -> Result<bitcoin::Block> {
    let block = verify_block(bytes, &rec.hash, &rec.header)?;
    verify_witness_commitment(&block)?;
    Ok(block)
}

/// A cached transaction must reproduce the indexed header's merkle root.
fn verify_cached_tx(
    entry: &txcache::CachedTx,
    rec: &el_model::BlockRecord,
    txid: Hash32,
) -> Result<()> {
    if entry.height != rec.height || entry.tx_pos >= rec.tx_count {
        return Err(RawtxError::Verify(
            "cached position does not match the index".into(),
        ));
    }
    let tx: bitcoin::Transaction = bitcoin::consensus::deserialize(&entry.raw)
        .map_err(|e| RawtxError::Verify(format!("cannot decode cached tx: {e}")))?;
    let computed = Hash32::from_hex(&tx.compute_txid().to_string())
        .map_err(|e| RawtxError::Verify(format!("bad txid: {e}")))?;
    if computed != txid {
        return Err(RawtxError::Verify("cached txid mismatch".into()));
    }
    let header: bitcoin::block::Header = bitcoin::consensus::deserialize(&rec.header)
        .map_err(|e| RawtxError::Verify(format!("bad indexed header: {e}")))?;
    let root = el_model::merkle_root_from_branch(txid, &entry.branch, entry.tx_pos as usize);
    if root.to_hex() != header.merkle_root.to_string() {
        return Err(RawtxError::Verify("cached merkle branch mismatch".into()));
    }
    Ok(())
}
pub use core_source::CoreBlockSource;
pub use decoy_wallets::TxRef;
pub use electrum::{ElectrumServer, ElectrumTxSource};
pub use esplora::{isolated_tor_client, EsploraSource};
pub use p2p::{P2pSource, PeerHint};
pub use privacy::{ExternalMode, PrivacyParams};
pub use txsource::{EsploraTxSource, TxBatch, TxSource};
pub use verify::{extract_tx, verify_block, verify_block_by_hash, verify_witness_commitment};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawtxError {
    /// No source could even be asked: none is configured, or Tor is required
    /// and unavailable.
    NoSource(String),
    /// Every source that was asked failed.
    Unavailable(String),
    Verify(String),
    Config(String),
    Io(String),
}

impl fmt::Display for RawtxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RawtxError::NoSource(m) | RawtxError::Unavailable(m) => {
                write!(f, "rawtx unavailable: {m}")
            }
            RawtxError::Verify(m) => write!(f, "rawtx verification failed: {m}"),
            RawtxError::Config(m) => write!(f, "rawtx config: {m}"),
            RawtxError::Io(m) => write!(f, "rawtx io: {m}"),
        }
    }
}

impl std::error::Error for RawtxError {}

impl RawtxError {
    /// Error class without details. Details can carry a txid or the block
    /// hash of a wallet transaction, which must not reach the log (AUD-132);
    /// the wallet still receives the full message in its RPC error.
    pub fn kind(&self) -> &'static str {
        match self {
            RawtxError::NoSource(_) => "no source",
            RawtxError::Unavailable(_) => "unavailable",
            RawtxError::Verify(_) => "verification failed",
            RawtxError::Config(_) => "config",
            RawtxError::Io(_) => "io",
        }
    }
}

impl From<el_store::StoreError> for RawtxError {
    fn from(e: el_store::StoreError) -> Self {
        RawtxError::Io(e.to_string())
    }
}

impl From<el_model::CodecError> for RawtxError {
    fn from(e: el_model::CodecError) -> Self {
        RawtxError::Io(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, RawtxError>;

/// Replace every 64-hex-digit run (txid, block hash, script hash) with
/// `<hash>`, so an error that mentions one can be logged (AUD-132).
pub fn redact_hashes(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let run = bytes[i..]
            .iter()
            .take_while(|b| b.is_ascii_hexdigit())
            .count();
        if run >= 64 {
            out.push_str("<hash>");
            i += run;
        } else if run > 0 {
            out.push_str(&text[i..i + run]);
            i += run;
        } else {
            let ch = text[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Time one external block download may spend on the network once it has a
/// slot. Background (prefetch) downloads may take this long; a wallet request
/// uses `WALLET_BLOCK_NET` instead.
pub const EXTERNAL_BUDGET: Duration = Duration::from_secs(60);
/// Longest a background download waits for a free download slot before its
/// network budget starts (AUD-160).
pub const EXTERNAL_QUEUE_MAX: Duration = Duration::from_secs(120);
/// Budget of the per-txid path for one wallet request. Measured over Tor, a
/// 50-tx batch takes 0.3-1.2 s; 8 s leaves room for two 3 s attempts and one
/// switch. The old 60 s made a single slow provider hold the wallet.
pub const WALLET_TX_BUDGET: Duration = Duration::from_secs(8);
/// Total budget of a wallet block request (queue + network). A 1.9 MB block
/// takes ~3.2 s over Tor (581 KiB/s measured) and a first load queues ~55 of
/// them behind 16 slots, so the queue needs room; 20 s still beats the old
/// 60 s network + 120 s queue and never returns a transient error for a
/// confirmed transaction (AUD-045).
pub const WALLET_BLOCK_BUDGET: Duration = Duration::from_secs(20);
/// Network part of a wallet block request, once it holds a download slot.
/// Eight seconds covers a 1.9 MB block on a slow circuit and leaves the race
/// engine room to retry a recovering endpoint (MIN_ATTEMPT * 3).
pub const WALLET_BLOCK_NET: Duration = Duration::from_secs(8);
/// Head start given to a block download already in flight before the per-txid
/// path starts (`rawtx.txid_race`): both run concurrently and the first
/// verified answer wins.
pub const RACE_HEAD_START: Duration = Duration::from_millis(250);
/// Extra time a follower waits beyond the leader's deadline.
pub const FOLLOWER_GRACE: Duration = Duration::from_secs(5);
/// A block whose download just failed is answered from that failure for this
/// long instead of asking every source again (AUD-158).
pub const FAILURE_MEMORY: Duration = Duration::from_secs(10);
/// An attempt with less time left than this is not started.
pub(crate) const MIN_ATTEMPT: Duration = Duration::from_secs(2);

/// Attempts of one block download running at the same time (AUD-160). The
/// first starts at once; another starts when one fails, or when the newest
/// one has received nothing for the first-byte limit or is still running
/// at the slow limit. The first verified block wins and the rest stop.
pub const RACE_WIDTH: usize = 3;
/// Hedge timings through Tor: no byte after this long (a circuit or peer that
/// is not answering), or still running after the slow limit.
pub const HEDGE_FIRST_BYTE_TOR: Duration = Duration::from_secs(6);
pub const HEDGE_SLOW_TOR: Duration = Duration::from_secs(15);
/// A transfer still below this average rate after `HEDGE_RATE_AFTER_*` is
/// hedged: circuits differ by an order of magnitude (60-450 KB/s measured on
/// mainnet for the same provider), so a second circuit usually finishes
/// first. The slow attempt keeps running; the first verified block wins.
pub const HEDGE_RATE_AFTER_TOR: Duration = Duration::from_secs(5);
pub const HEDGE_MIN_RATE_TOR: u64 = 96 * 1024;
pub const HEDGE_RATE_AFTER_DIRECT: Duration = Duration::from_secs(2);
pub const HEDGE_MIN_RATE_DIRECT: u64 = 256 * 1024;
/// Hedge timings for a direct (clearnet) attempt.
pub const HEDGE_FIRST_BYTE_DIRECT: Duration = Duration::from_secs(2);
pub const HEDGE_SLOW_DIRECT: Duration = Duration::from_secs(6);
/// Extra attempts one Tor endpoint may get inside a race, each on a new
/// circuit, while its provider is not cooling down.
pub const TOR_RETRIES_PER_ENDPOINT: u32 = 1;
/// Attempts one block download may start in all (hedges, retries and
/// endpoints that came back from a pause included).
pub const MAX_RACE_ATTEMPTS: usize = 12;
/// Once every endpoint failed or is paused, how long a race waits for one
/// to come back before it reports the failure.
pub const RACE_REFILL_WAIT: Duration = Duration::from_secs(15);
/// How often a running race re-evaluates its hedges.
const RACE_TICK: Duration = Duration::from_millis(200);

/// One way to ask a source for a block: one provider over one transport, or
/// one peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// Source-private identifier (`tor <base>`, `direct <base>`, a peer).
    pub key: String,
    /// 0: primary. 1: clearnet fallback, started only once no primary
    /// attempt is left running or waiting (AUD-128).
    pub tier: u8,
    /// Whether the attempt goes through Tor (selects the hedge timings).
    pub via_tor: bool,
}

impl Endpoint {
    /// The whole source as a single endpoint (sources without endpoints).
    pub fn whole() -> Self {
        Endpoint {
            key: String::new(),
            tier: 0,
            via_tor: false,
        }
    }
}

#[derive(Default)]
struct CtlInner {
    aborted: AtomicBool,
    bytes: AtomicU64,
    sockets: Mutex<Vec<TcpStream>>,
    retry: AtomicBool,
}

/// Shared by one attempt and the race that started it: progress (bytes
/// received) flows up, an abort flows down. Aborting shuts down the
/// attempt's sockets, so a losing attempt stops at once instead of holding a
/// circuit until its own timeout.
#[derive(Clone, Default)]
pub struct AttemptCtl(Arc<CtlInner>);

impl AttemptCtl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn aborted(&self) -> bool {
        self.0.aborted.load(Ordering::Relaxed)
    }

    pub fn abort(&self) {
        self.0.aborted.store(true, Ordering::Relaxed);
        if let Ok(sockets) = self.0.sockets.lock() {
            for s in sockets.iter() {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
        }
    }

    /// The attempt failed in a way a new Tor circuit may fix (timeout, stall,
    /// truncated body, 5xx): the race may try this endpoint once more. A
    /// wrong answer or an endpoint that cannot be reached never asks for it.
    pub fn retry_on_new_circuit(&self) {
        self.0.retry.store(true, Ordering::Relaxed);
    }

    pub fn wants_retry(&self) -> bool {
        self.0.retry.load(Ordering::Relaxed)
    }

    pub fn add_bytes(&self, n: usize) {
        self.0.bytes.fetch_add(n as u64, Ordering::Relaxed);
    }

    pub fn bytes(&self) -> u64 {
        self.0.bytes.load(Ordering::Relaxed)
    }

    /// Register a socket so an abort can shut it down.
    pub fn watch(&self, stream: &TcpStream) {
        if let Ok(clone) = stream.try_clone() {
            if let Ok(mut sockets) = self.0.sockets.lock() {
                sockets.push(clone);
            }
        }
        if self.aborted() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// Whether a download may change a source's view of its endpoints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchMode {
    /// A wallet or prefetch download: failures cool endpoints down.
    Normal,
    /// A decoy: its failures are not the wallet's problem and must not cool
    /// down a provider or peer that real requests need (AUD-157).
    Quiet,
}

/// A source that can return a **whole raw block** by hash.
pub trait BlockSource: Send + Sync {
    fn name(&self) -> &str;
    fn fetch_block(&self, hash: &Hash32) -> Result<Vec<u8>>;
    /// Like `fetch_block`, bounded by `deadline` and in `mode`. Sources that
    /// talk to the network override it; the default ignores both.
    fn fetch_block_until(
        &self,
        hash: &Hash32,
        deadline: Instant,
        mode: FetchMode,
    ) -> Result<Vec<u8>> {
        let _ = (deadline, mode);
        self.fetch_block(hash)
    }
    /// Endpoints a download may race over now, in random order, without
    /// the ones cooling down. A source with nothing to split is one endpoint.
    fn endpoints(&self, mode: FetchMode) -> Vec<Endpoint> {
        let _ = mode;
        vec![Endpoint::whole()]
    }
    /// One attempt on one endpoint returned by `endpoints`. It updates the
    /// endpoint's health itself (never after `ctl` was aborted) and reports
    /// bytes received to `ctl`.
    fn fetch_endpoint(
        &self,
        endpoint: &Endpoint,
        hash: &Hash32,
        deadline: Instant,
        mode: FetchMode,
        ctl: &AttemptCtl,
    ) -> Result<Vec<u8>> {
        let _ = (endpoint, ctl);
        self.fetch_block_until(hash, deadline, mode)
    }
    /// Whether the source is being rate limited right now (decoys wait).
    fn under_pressure(&self) -> bool {
        false
    }
    /// Local sources never leak anything to third parties.
    fn is_local(&self) -> bool {
        false
    }
}

/// How a pruned transaction is resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalPath {
    /// Race the block download against the per-txid path (default).
    Auto,
    /// Try the loose transaction first and fall back to the block.
    Tx,
    /// Whole block only; no txid leaves the node.
    Block,
}

impl ExternalPath {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "tx" => Some(Self::Tx),
            "block" => Some(Self::Block),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Tx => "tx",
            Self::Block => "block",
        }
    }
}

/// When the decoy wallets' sessions run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecoySessions {
    /// In the same dispatch round as the real wallet (default).
    SameRound,
    /// Only when the real queue is empty, or one wallet if it is not.
    Background,
}

impl DecoySessions {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "same_round" | "same-round" => Some(Self::SameRound),
            "background" => Some(Self::Background),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::SameRound => "same_round",
            Self::Background => "background",
        }
    }
}

#[derive(Default)]
pub struct RawtxStats {
    pub cache_hits: AtomicU64,
    pub cache_puts: AtomicU64,
    pub fetches: AtomicU64,
    pub bytes: AtomicU64,
    pub verify_failures: AtomicU64,
    /// Full verifications of an externally downloaded block against the
    /// indexed header. One per download, however many requests share it.
    pub verifications: AtomicU64,
    pub decoys: AtomicU64,
    pub unavailable: AtomicU64,
    pub p2p_attempts: AtomicU64,
    pub p2p_failures: AtomicU64,
    pub p2p_skipped: AtomicU64,
    pub p2p_tor_failures: AtomicU64,
    /// Attempts started by block races (AUD-160), hedges among them, and
    /// the races won by each kind of source.
    pub race_attempts: AtomicU64,
    pub race_hedges: AtomicU64,
    pub race_wins_esplora: AtomicU64,
    pub race_wins_p2p: AtomicU64,
    pub race_wins_other: AtomicU64,
    /// Sum of the time from the first attempt to the winning block, in ms.
    pub race_win_ms: AtomicU64,
    /// Per-txid mode (AUD-163): sessions run, real and decoy txids sent,
    /// real transactions verified against the index, rejected answers, and
    /// transactions that fell back to the whole-block path.
    pub tx_sessions: AtomicU64,
    pub tx_real: AtomicU64,
    pub tx_decoys: AtomicU64,
    pub tx_verified: AtomicU64,
    pub tx_verify_failed: AtomicU64,
    pub tx_fallback: AtomicU64,
    /// A wallet transaction was served from a block download already in
    /// flight instead of starting the per-txid path.
    pub block_joins: AtomicU64,
    /// Wallet requests resolved by the block path in `auto` (it was racing a
    /// per-txid attempt) and by the txid path while a block was in flight.
    pub race_wins_block: AtomicU64,
    pub race_wins_txid: AtomicU64,
}

#[derive(Clone, Debug)]
pub struct RawtxConfig {
    /// When false, only the local Core block source and the disk cache are used.
    pub enabled: bool,
    pub cache_mib: u64,
    pub decoys: u32,
    pub tor_socks: Option<String>,
    pub allow_clearnet: bool,
    /// If Tor is configured but a request fails, retry the clearnet endpoint
    /// directly. This exposes the operator's IP to the provider.
    pub fallback_clearnet: bool,
    /// Timeout for a single direct (clearnet) Esplora block request.
    pub request_timeout_seconds: u64,
    /// Absolute limit of one Esplora attempt (Tor or direct); a transfer
    /// that keeps receiving bytes may use all of it.
    pub tor_request_timeout_seconds: u64,
    /// Circuit + connection time allowed for an Esplora request through Tor.
    pub tor_connect_timeout_seconds: u64,
    /// No response header or body byte for this long through Tor: stalled.
    pub tor_stall_timeout_seconds: u64,
    /// Skip a provider (Tor or direct) after a failed attempt.
    pub provider_cooldown_seconds: u64,
    /// Maximum concurrent external block downloads.
    pub max_parallel_fetches: usize,
    /// Cache directory override. `None` uses `<data-dir>/rawtx-cache`.
    pub cache_dir: Option<std::path::PathBuf>,
    pub use_p2p: bool,
    pub use_esplora: bool,
    pub p2p_v2: bool,
    pub esplora_urls: Vec<String>,
    pub p2p_peers: Vec<String>,
    pub p2p_timeout: Duration,
    /// Per-attempt limit of a P2P connection made through Tor.
    pub p2p_tor_timeout: Duration,
    pub chain: Chain,
    /// Effective `privacy.toml` values (AUD-163).
    pub privacy: PrivacyParams,
    /// Public Electrum servers (`host:port:s|t`) for the per-txid mode.
    pub electrum_servers: Vec<String>,
    /// How a pruned transaction is resolved (auto races the block).
    pub path: ExternalPath,
    /// Total wallet request deadline (queue + network).
    pub wallet_deadline_seconds: u64,
    /// Budget of one per-txid attempt.
    pub txid_attempt_seconds: u64,
    /// Give an in-flight block a short head start before the txid path.
    pub txid_race: bool,
    /// Decoy sessions in the same round or only in the background.
    pub decoy_sessions: DecoySessions,
    /// Global concurrent per-txid sessions.
    pub sessions_total: usize,
    /// Random delay before a per-txid session, milliseconds (0-250).
    pub jitter_ms: u64,
    /// History blocks queued per history response. 0 = auto (block mode
    /// prefetches, per-txid mode only on demand).
    pub prefetch_history_blocks: usize,
}

impl RawtxConfig {
    /// A configuration that never contacts anything outside this host: no
    /// P2P, no Esplora, no Electrum servers, no Tor, no cache. The starting
    /// point for tests and offline tools; change what a case needs with
    /// struct update syntax (`RawtxConfig { enabled: true, ..local_only(c) }`).
    pub fn local_only(chain: Chain) -> Self {
        RawtxConfig {
            enabled: false,
            cache_mib: 0,
            decoys: 0,
            tor_socks: None,
            allow_clearnet: true,
            fallback_clearnet: false,
            request_timeout_seconds: 15,
            tor_request_timeout_seconds: 15,
            tor_connect_timeout_seconds: 15,
            tor_stall_timeout_seconds: 15,
            provider_cooldown_seconds: 120,
            max_parallel_fetches: 4,
            cache_dir: None,
            use_p2p: false,
            use_esplora: false,
            p2p_v2: false,
            esplora_urls: Vec::new(),
            p2p_peers: Vec::new(),
            p2p_timeout: Duration::from_secs(1),
            p2p_tor_timeout: Duration::from_secs(1),
            chain,
            privacy: PrivacyParams::default(),
            electrum_servers: Vec::new(),
            path: ExternalPath::Auto,
            wallet_deadline_seconds: 12,
            txid_attempt_seconds: 3,
            txid_race: true,
            decoy_sessions: DecoySessions::SameRound,
            sessions_total: 16,
            jitter_ms: 50,
            prefetch_history_blocks: 0,
        }
    }
}

/// Who asks for an external block. A wallet request may take the last free
/// download slot; the background prefetch always leaves one for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Priority {
    Wallet,
    Prefetch,
}

/// Longest a decoy waits for running wallet downloads (AUD-160): decoys
/// still happen during a long load, just not in front of the wallet.
const DECOY_YIELD: Duration = Duration::from_secs(30);

/// Counts a running download for as long as it lives.
struct Busy(Arc<AtomicU64>);

impl Busy {
    fn new(counter: &Arc<AtomicU64>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Busy(Arc::clone(counter))
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Decoys owed for real external downloads, at most this many.
const MAX_DECOY_DEBT: usize = 32;

/// Decoy downloads owed for real external downloads (wallet or prefetch),
/// served one at a time by a background thread outside the wallet's
/// download slots. A decoy never changes a provider's or peer's health and
/// waits while a provider rate-limits (AUD-157).
struct DecoyLane {
    queue: Mutex<VecDeque<Hash32>>,
    cv: Condvar,
    closed: std::sync::atomic::AtomicBool,
    started: std::sync::Once,
}

impl DecoyLane {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            closed: std::sync::atomic::AtomicBool::new(false),
            started: std::sync::Once::new(),
        })
    }

    fn pending(&self) -> usize {
        self.queue.lock().map(|q| q.len()).unwrap_or(0)
    }

    /// Next owed decoy, or `None` once the resolver is gone.
    fn next(&self) -> Option<Hash32> {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if self.closed.load(Ordering::Relaxed) {
                return None;
            }
            if let Some(hash) = queue.front().copied() {
                return Some(hash);
            }
            queue = self
                .cv
                .wait_timeout(queue, Duration::from_secs(1))
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }

    fn done(&self) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.pop_front();
        }
    }

    /// Sleep up to `d`, returning early (false) once the resolver is gone.
    fn pause(&self, d: Duration) -> bool {
        let end = Instant::now() + d;
        while Instant::now() < end {
            if self.closed.load(Ordering::Relaxed) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50).min(end - Instant::now()));
        }
        !self.closed.load(Ordering::Relaxed)
    }
}

/// Small counting semaphore: bounds total external downloads without pulling
/// in an async runtime.
pub(crate) struct Semaphore {
    permits: Mutex<usize>,
    total: usize,
    cv: Condvar,
}

impl Semaphore {
    pub(crate) fn new(permits: usize) -> Arc<Self> {
        let total = permits.max(1);
        Arc::new(Self {
            permits: Mutex::new(total),
            total,
            cv: Condvar::new(),
        })
    }

    /// Take a permit while more than `keep_free` are free, waiting until
    /// `deadline` at most. `keep_free` never blocks a single-permit pool.
    pub(crate) fn acquire_until(
        self: &Arc<Self>,
        deadline: Instant,
        keep_free: usize,
    ) -> Option<Permit> {
        let keep_free = keep_free.min(self.total - 1);
        let mut permits = self
            .permits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *permits <= keep_free {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            permits = self
                .cv
                .wait_timeout(permits, left)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        *permits -= 1;
        Some(Permit {
            sem: Arc::clone(self),
        })
    }
}

pub(crate) struct Permit {
    sem: Arc<Semaphore>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut permits = self
            .sem
            .permits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *permits += 1;
        // Every waiter re-checks: a prefetch may not take what a wallet
        // request is waiting for.
        self.sem.cv.notify_all();
    }
}

pub struct Resolver {
    store: Arc<Store>,
    /// Local source, tried first and never cached.
    local: Option<Arc<dyn BlockSource>>,
    /// External sources other than P2P, tried in random order.
    external: Vec<Arc<dyn BlockSource>>,
    /// P2P source, kept apart so its peer list can be refreshed (AUD-137).
    /// It counts as a source only while it has peers.
    p2p: Option<Arc<P2pSource>>,
    /// Whole blocks: prune-gap repair, plus entries from older releases.
    cache: Option<BlockCache>,
    /// Verified transactions for wallet requests (AUD-139).
    txcache: Option<Arc<TxCache>>,
    /// Per-txid fetch mode (AUD-163); `None` in block mode or without
    /// per-txid sources.
    tx_mode: Option<txbatch::TxMode>,
    /// Core's prune height as last reported by the server layer. The per-txid
    /// path only sends txids of blocks Core no longer has (AUD-167); 0 means
    /// unknown, which stays on the whole-block path.
    prune_height: std::sync::atomic::AtomicU32,
    privacy: PrivacyParams,
    /// How a pruned transaction is resolved and its budgets.
    path: ExternalPath,
    wallet_deadline: Duration,
    txid_attempt: Duration,
    txid_race: bool,
    /// Per-txid session policy, also used by `with_tx_sources` in tests.
    decoy_sessions: DecoySessions,
    sessions_total: usize,
    jitter_ms: u64,
    /// Verified external blocks kept in memory for a burst of requests.
    recent: Mutex<VecDeque<(Hash32, Arc<bitcoin::Block>)>>,
    cache_dir: std::path::PathBuf,
    decoys: u32,
    stats: Arc<RawtxStats>,
    chain: Chain,
    inflight: Mutex<HashMap<Hash32, Arc<race::InflightBlock>>>,
    /// Blocks whose download failed recently, with the error (AUD-158).
    failed: Mutex<HashMap<Hash32, (Instant, RawtxError)>>,
    external_slots: Arc<Semaphore>,
    /// Attempts running at once over every download (hedges included),
    /// well below Tor's 32 pending circuits.
    external_attempts: Arc<Semaphore>,
    /// Wallet downloads running now; decoys wait for them (bandwidth).
    wallet_downloads: Arc<AtomicU64>,
    /// Wallet downloads waiting for a slot: the prefetch steps aside.
    wallet_slot_waiters: Arc<AtomicU64>,
    decoy_lane: Arc<DecoyLane>,
}

impl Drop for Resolver {
    fn drop(&mut self) {
        self.decoy_lane.closed.store(true, Ordering::Relaxed);
        self.decoy_lane.cv.notify_all();
    }
}

impl Resolver {
    pub fn new(store: Arc<Store>, cfg: &RawtxConfig) -> Result<Self> {
        let mut external: Vec<Arc<dyn BlockSource>> = Vec::new();
        let mut p2p = None;
        let tor = cfg.tor_socks.clone();
        let stats = Arc::new(RawtxStats::default());

        if cfg.enabled {
            // Created even with no peers yet: the list is refreshed from Core
            // at runtime, and an empty source is not counted as a source.
            if cfg.use_p2p {
                if tor.is_none() && !cfg.allow_clearnet && !cfg.fallback_clearnet {
                    // fail closed: no external P2P without Tor and no fallback
                } else {
                    p2p = Some(Arc::new(
                        P2pSource::new(
                            cfg.p2p_peers.clone(),
                            tor.clone(),
                            cfg.p2p_timeout,
                            cfg.chain,
                            cfg.p2p_v2,
                            cfg.fallback_clearnet,
                            Duration::from_secs(cfg.provider_cooldown_seconds.max(1)),
                        )
                        .with_tor_timeout(cfg.p2p_tor_timeout)
                        .with_stats(Arc::clone(&stats)),
                    ));
                }
            }
            if cfg.use_esplora && !cfg.esplora_urls.is_empty() {
                match EsploraSource::with_budget(
                    cfg.esplora_urls.clone(),
                    tor.clone(),
                    cfg.allow_clearnet,
                    cfg.fallback_clearnet,
                    Duration::from_secs(cfg.request_timeout_seconds.max(1)),
                    Duration::from_secs(cfg.tor_request_timeout_seconds.max(1)),
                    Duration::from_secs(cfg.provider_cooldown_seconds.max(1)),
                ) {
                    Ok(s) => external.push(Arc::new(s.with_tor_timeouts(
                        Duration::from_secs(cfg.tor_connect_timeout_seconds.max(1)),
                        Duration::from_secs(cfg.tor_stall_timeout_seconds.max(1)),
                    ))),
                    Err(e) => eprintln!("[e-lite] esplora disabled: {e}"),
                }
            }
        }
        if let Some(p2p) = &p2p {
            p2p.set_force_when_cooling(external.is_empty());
        }

        let cache_dir = cfg
            .cache_dir
            .clone()
            .unwrap_or_else(|| store.path().join("rawtx-cache"));
        // `cache_mib` is the total disk budget: a quarter (1..64 MiB) for
        // whole blocks (repair, legacy entries), the rest for transactions.
        let (block_mib, tx_mib) = cache_split(cfg.cache_mib);
        let cache = if block_mib > 0 {
            Some(BlockCache::open(
                &cache_dir,
                block_mib.saturating_mul(1024 * 1024),
            )?)
        } else {
            None
        };
        let txcache = if tx_mib > 0 {
            Some(Arc::new(TxCache::open(
                &cache_dir.join("tx"),
                tx_mib.saturating_mul(1024 * 1024),
            )?))
        } else {
            None
        };
        let tx_mode = if cfg.enabled && cfg.privacy.mode == ExternalMode::Tx {
            Self::build_tx_mode(&store, cfg, txcache.clone(), Arc::clone(&stats))
        } else {
            None
        };

        Ok(Resolver {
            store,
            local: None,
            external,
            p2p,
            cache,
            txcache,
            tx_mode,
            prune_height: std::sync::atomic::AtomicU32::new(0),
            privacy: cfg.privacy.clone(),
            path: cfg.path,
            wallet_deadline: Duration::from_secs(cfg.wallet_deadline_seconds.clamp(3, 60)),
            txid_attempt: Duration::from_secs(cfg.txid_attempt_seconds.clamp(1, 30)),
            txid_race: cfg.txid_race,
            decoy_sessions: cfg.decoy_sessions,
            sessions_total: cfg.sessions_total.clamp(1, 32),
            jitter_ms: cfg.jitter_ms.min(250),
            recent: Mutex::new(VecDeque::new()),
            cache_dir,
            decoys: cfg.decoys,
            stats,
            chain: cfg.chain,
            inflight: Mutex::new(HashMap::new()),
            failed: Mutex::new(HashMap::new()),
            external_slots: Semaphore::new(cfg.max_parallel_fetches),
            external_attempts: Semaphore::new(
                cfg.max_parallel_fetches
                    .max(1)
                    .saturating_mul(2)
                    .clamp(2, 24),
            ),
            wallet_downloads: Arc::new(AtomicU64::new(0)),
            wallet_slot_waiters: Arc::new(AtomicU64::new(0)),
            decoy_lane: DecoyLane::new(),
        })
    }

    /// Per-txid sources (Electrum, Esplora `/tx/:txid/raw`) and the batcher.
    /// Most simultaneous connections the per-txid path may open (AUD-167).
    /// Esplora fans out inside a session, so the session count alone does not
    /// bound the circuits Tor has to build.
    const MAX_TX_CONNECTIONS: usize = 8;

    fn build_tx_mode(
        store: &Arc<Store>,
        cfg: &RawtxConfig,
        txcache: Option<Arc<TxCache>>,
        stats: Arc<RawtxStats>,
    ) -> Option<txbatch::TxMode> {
        let tor = cfg.tor_socks.clone();
        if tor.is_none() && !cfg.allow_clearnet {
            return None;
        }
        let connect = Duration::from_secs(cfg.tor_connect_timeout_seconds.max(1));
        let stall = Duration::from_secs(cfg.tor_stall_timeout_seconds.max(1));
        let cooldown = Duration::from_secs(cfg.provider_cooldown_seconds.max(1));
        // One pool for every per-txid connection (AUD-167): a session's
        // Esplora fan-out used to open up to 8 circuits per session.
        let conns = Semaphore::new(Self::MAX_TX_CONNECTIONS);
        let mut sources: Vec<Arc<dyn TxSource>> = Vec::new();
        if let Some(s) = ElectrumTxSource::new(
            &cfg.electrum_servers,
            tor.clone(),
            cfg.allow_clearnet,
            connect,
            stall,
            cooldown,
            Arc::clone(&conns),
        ) {
            sources.push(Arc::new(s));
        }
        if cfg.use_esplora {
            if let Some(s) = EsploraTxSource::new(
                cfg.esplora_urls.clone(),
                tor,
                cfg.allow_clearnet,
                connect,
                stall,
                cooldown,
                Arc::clone(&conns),
            ) {
                sources.push(Arc::new(s));
            }
        }
        if sources.is_empty() {
            return None;
        }
        let secret = match decoy_wallets::load_or_create_secret(store.path()) {
            Ok(k) => k,
            Err(e) => {
                eprintln!(
                    "[e-lite] WARNING: decoy key unavailable ({}); per-txid mode disabled, whole blocks only",
                    e.kind()
                );
                return None;
            }
        };
        txbatch::TxMode::new(
            Arc::clone(store),
            txcache,
            sources,
            cfg.privacy.clone(),
            secret,
            stats,
            cfg.sessions_total,
            cfg.decoy_sessions,
            cfg.jitter_ms,
        )
    }

    /// Replace the per-txid sources (tests and custom setups). `secret` keys
    /// the decoy wallets. Nothing changes in block mode.
    pub fn with_tx_sources(mut self, sources: Vec<Arc<dyn TxSource>>, secret: [u8; 32]) -> Self {
        if self.privacy.mode != ExternalMode::Tx {
            return self;
        }
        self.tx_mode = txbatch::TxMode::new(
            Arc::clone(&self.store),
            self.txcache.clone(),
            sources,
            self.privacy.clone(),
            secret,
            Arc::clone(&self.stats),
            self.sessions_total,
            self.decoy_sessions,
            self.jitter_ms,
        );
        self
    }

    /// Effective privacy parameters (`privacy.toml`).
    pub fn privacy(&self) -> &PrivacyParams {
        &self.privacy
    }

    /// Whether the per-txid mode is active, and over how many servers.
    /// Record Core's prune height (called by the server layer on each wallet
    /// request). 0 keeps the per-txid path disabled.
    pub fn set_prune_height(&self, height: u32) {
        self.prune_height.store(height, Ordering::Relaxed);
    }

    /// True when the per-txid path has nothing queued or running (tests).
    pub fn tx_idle(&self) -> bool {
        self.tx_mode.as_ref().is_none_or(|tm| tm.idle())
    }

    pub fn tx_mode_servers(&self) -> Option<usize> {
        self.tx_mode.as_ref().map(|t| t.servers())
    }

    /// The index served `script`'s history to connection `ctx`: in per-txid
    /// mode, fetch every transaction of it now (with the connection's decoy
    /// wallets), before the wallet asks. Returns how many were queued.
    pub fn want_history(&self, ctx: u64, script: el_model::ScriptHash, hist: &[TxRef]) -> usize {
        match &self.tx_mode {
            Some(tm) => tm.want_history(ctx, script, hist.to_vec()),
            None => 0,
        }
    }

    /// Connection `ctx` closed: drop its decoy wallets.
    pub fn forget_client(&self, ctx: u64) {
        if let Some(tm) = &self.tx_mode {
            tm.forget(ctx);
        }
    }

    pub fn with_local(mut self, source: Arc<dyn BlockSource>) -> Self {
        self.local = Some(source);
        self
    }

    /// Inject an additional external source (used by tests and custom setups).
    pub fn with_external(mut self, source: Arc<dyn BlockSource>) -> Self {
        self.external.push(source);
        if let Some(p2p) = &self.p2p {
            p2p.set_force_when_cooling(false);
        }
        self
    }

    pub fn stats(&self) -> Arc<RawtxStats> {
        Arc::clone(&self.stats)
    }

    pub fn external_sources(&self) -> usize {
        self.sources().len()
    }

    /// External sources usable right now: the configured ones plus P2P when
    /// it has at least one peer.
    fn sources(&self) -> Vec<Arc<dyn BlockSource>> {
        let mut out = self.external.clone();
        if let Some(p2p) = &self.p2p {
            if p2p.peer_count() > 0 {
                out.push(Arc::clone(p2p) as Arc<dyn BlockSource>);
            }
        }
        out
    }

    /// Replace the P2P peer list (discovered from Core). Returns the new
    /// count, or `None` when P2P is disabled.
    pub fn refresh_p2p_peers(&self, peers: Vec<PeerHint>) -> Option<usize> {
        self.p2p.as_ref().map(|p2p| p2p.set_peers(peers))
    }

    /// Current P2P peer count, `None` when P2P is disabled.
    pub fn p2p_peers(&self) -> Option<usize> {
        self.p2p.as_ref().map(|p2p| p2p.peer_count())
    }

    /// Directory used by the validated rawtx cache.
    pub fn cache_dir(&self) -> &std::path::Path {
        &self.cache_dir
    }

    /// Bytes stored in the rawtx caches (blocks + transactions).
    pub fn cache_size_bytes(&self) -> u64 {
        self.cache.as_ref().map(|c| c.size()).unwrap_or(0)
            + self.txcache.as_ref().map(|c| c.size()).unwrap_or(0)
    }

    /// Transactions in the transaction cache.
    pub fn cached_txs(&self) -> usize {
        self.txcache.as_ref().map(|c| c.len()).unwrap_or(0)
    }

    /// Whole blocks in the block cache.
    pub fn cached_blocks(&self) -> usize {
        self.cache.as_ref().map(|c| c.len()).unwrap_or(0)
    }

    /// Cache inventory from the in-memory state each cache rebuilt at open.
    /// O(1): never walks the directory (that is `cachelayout::inventory`).
    pub fn cache_inventory(&self) -> CacheInventory {
        CacheInventory {
            block_files: self.cached_blocks() as u64,
            block_bytes: self.cache.as_ref().map(|c| c.size()).unwrap_or(0),
            tx_files: self.cached_txs() as u64,
            tx_bytes: self.txcache.as_ref().map(|c| c.size()).unwrap_or(0),
            skipped: 0,
        }
    }

    /// Whether a pruned-index repair has any chance: a validated block cache
    /// or at least one external whole-block source.
    pub fn has_repair_sources(&self) -> bool {
        !self.sources().is_empty() || self.cache.as_ref().map(|c| !c.is_empty()).unwrap_or(false)
    }

    /// Fetch a whole block by hash for index repair, verifying its hash and
    /// merkle root. This is only used for heights Core has already pruned; the
    /// normal index path keeps using the local bulk source/RPC and never sends
    /// block hashes to external sources.
    pub fn fetch_block_by_hash(&self, hash: &Hash32) -> Result<Vec<u8>> {
        if let Some(cache) = &self.cache {
            if let Some(bytes) = cache.get(hash) {
                match verify_block_by_hash(&bytes, hash) {
                    Ok(_) => {
                        self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
                        return Ok(bytes);
                    }
                    Err(e) => {
                        self.stats.verify_failures.fetch_add(1, Ordering::Relaxed);
                        eprintln!(
                            "[e-lite] cached repair block {} rejected: {e}",
                            hash.to_hex()
                        );
                        let _ = cache.remove(hash);
                    }
                }
            }
        }

        let mut errors = Vec::new();
        if let Some(local) = &self.local {
            match local.fetch_block(hash) {
                Ok(bytes) => match verify_block_by_hash(&bytes, hash) {
                    Ok(_) => {
                        self.cache_store(hash, &bytes);
                        return Ok(bytes);
                    }
                    Err(e) => errors.push(format!("{}: {e}", local.name())),
                },
                Err(e) => errors.push(format!("{}: {e}", local.name())),
            }
        }

        let sources = self.sources();
        let mut order: Vec<usize> = (0..sources.len()).collect();
        order.shuffle(&mut thread_rng());
        for idx in order {
            let source = &sources[idx];
            match source.fetch_block(hash) {
                Ok(bytes) => {
                    self.stats.fetches.fetch_add(1, Ordering::Relaxed);
                    self.stats
                        .bytes
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    match verify_block_by_hash(&bytes, hash) {
                        Ok(_) => {
                            self.cache_store(hash, &bytes);
                            return Ok(bytes);
                        }
                        Err(e) => {
                            self.stats.verify_failures.fetch_add(1, Ordering::Relaxed);
                            eprintln!(
                                "[e-lite] {} returned an invalid repair block: {e}",
                                source.name()
                            );
                            errors.push(format!("{}: {e}", source.name()));
                        }
                    }
                }
                Err(e) => errors.push(format!("{}: {e}", source.name())),
            }
        }
        self.stats.unavailable.fetch_add(1, Ordering::Relaxed);
        Err(RawtxError::Unavailable(if errors.is_empty() {
            "no repair source configured (cache, P2P or Esplora)".to_string()
        } else {
            errors.join("; ")
        }))
    }

    pub fn chain(&self) -> Chain {
        self.chain
    }

    /// Resolve a confirmed transaction's raw hex at `height`.
    pub fn raw_tx(&self, txid: Hash32, height: u32) -> Result<String> {
        self.raw_tx_inner(0, txid, height, Priority::Wallet)
    }

    /// `raw_tx` for wallet connection `ctx` (its decoy wallets go along).
    pub fn raw_tx_for(&self, ctx: u64, txid: Hash32, height: u32) -> Result<String> {
        self.raw_tx_inner(ctx, txid, height, Priority::Wallet)
    }

    /// Prefetch every listed transaction of one block with a single block
    /// fetch, caching each one. An external download owes decoys like a
    /// wallet request (AUD-159) and never takes the last free download
    /// slot. Returns how many were fetched (0 when all were already cached).
    pub fn prefetch_block_txs(&self, height: u32, txids: &[Hash32]) -> Result<usize> {
        let rec = self.block_record(height)?;
        let missing: Vec<Hash32> = txids
            .iter()
            .copied()
            .filter(|txid| self.cached_tx(&rec, *txid).is_none())
            .collect();
        if missing.is_empty() {
            return Ok(0);
        }
        let (block, _) = self.verified_block(&rec, Priority::Prefetch)?;
        self.cache_txs(&rec, &block, &missing)?;
        Ok(missing.len())
    }

    /// The position of `txid` in the block at `height`, from the index.
    fn position_of(&self, txid: Hash32, height: u32) -> Option<u32> {
        self.store
            .occurrences(txid)
            .ok()?
            .into_iter()
            .find(|(h, _)| *h == height)
            .map(|(_, pos)| pos)
    }

    fn block_record(&self, height: u32) -> Result<el_model::BlockRecord> {
        self.store
            .get_block(height)?
            .ok_or_else(|| RawtxError::Unavailable(format!("block {height} not indexed")))
    }

    fn raw_tx_inner(
        &self,
        ctx: u64,
        txid: Hash32,
        height: u32,
        priority: Priority,
    ) -> Result<String> {
        let rec = self.block_record(height)?;
        // 1) Transaction cache: one small file per transaction (AUD-139).
        if let Some(hex) = self.cached_tx(&rec, txid) {
            self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(hex);
        }
        // 2) The block without asking outside: memory, block cache, Core.
        let mut block = self.local_block(&rec).map(|(block, _)| block);
        if block.is_none() {
            // One deadline for the whole wallet request
            // (rawtx.wallet_deadline_seconds): queue and both paths share it.
            let request_deadline = Instant::now() + self.wallet_deadline;
            let pruned_below = self.prune_height.load(Ordering::Relaxed);
            let pruned = pruned_below > 0 && height < pruned_below;
            // `block`: whole block only. No tx_mode (block privacy mode or no
            // per-txid sources) also means the block path.
            let block_only = self.path == ExternalPath::Block || self.tx_mode.is_none();
            let mut raced = false;
            if pruned && !block_only {
                // A prefetch or another tx.get already started this block: in
                // `auto` it gets a short head start and the per-txid path runs
                // concurrently; the first verified answer wins.
                if self.txid_race && self.path == ExternalPath::Auto {
                    if let Some(entry) = self.inflight_entry(&rec.hash) {
                        raced = true;
                        if let Some(Ok(joined)) =
                            entry.wait_outcome(RACE_HEAD_START.min(self.txid_attempt))
                        {
                            self.stats.block_joins.fetch_add(1, Ordering::Relaxed);
                            block = Some(joined);
                        }
                    }
                }
                if block.is_none() {
                    // 3) Per-txid mode (AUD-163): the loose transaction,
                    //    verified against the index, with its own short budget
                    //    (rawtx.txid_attempt_seconds); the block keeps the rest.
                    if let Some(tm) = &self.tx_mode {
                        if let Some(pos) = self.position_of(txid, height) {
                            let t = TxRef { txid, height, pos };
                            let now = Instant::now();
                            let attempt = (now + self.txid_attempt).min(request_deadline);
                            if attempt > now {
                                if let Ok(hex) = tm.fetch(ctx, t, attempt) {
                                    if raced {
                                        self.stats.race_wins_txid.fetch_add(1, Ordering::Relaxed);
                                    }
                                    return Ok(hex);
                                }
                            }
                        }
                    }
                }
            }
            if block.is_none() {
                // 4) Whole block from external sources, with what is left.
                let joining = self.block_inflight(&rec.hash);
                let (fetched, _) = self.external_block(
                    &rec,
                    priority,
                    Some(request_deadline.max(Instant::now() + MIN_ATTEMPT)),
                )?;
                if joining {
                    self.stats.block_joins.fetch_add(1, Ordering::Relaxed);
                }
                if raced {
                    self.stats.race_wins_block.fetch_add(1, Ordering::Relaxed);
                }
                block = Some(fetched);
            }
        }
        let block = block.expect("a block was fetched or joined");
        let hex = self
            .cache_txs(&rec, &block, &[txid])?
            .pop()
            .ok_or_else(|| RawtxError::Verify("transaction is not in the block".into()))?;
        Ok(hex)
    }

    /// The in-flight download of this block, when another caller started it.
    fn inflight_entry(&self, hash: &Hash32) -> Option<Arc<race::InflightBlock>> {
        self.inflight.lock().ok()?.get(hash).cloned()
    }

    fn block_inflight(&self, hash: &Hash32) -> bool {
        self.inflight
            .lock()
            .map(|map| map.contains_key(hash))
            .unwrap_or(false)
    }

    /// A cached transaction authenticated against the indexed header: txid,
    /// position and merkle branch must reproduce the header's merkle root.
    /// A mismatching entry is removed.
    fn cached_tx(&self, rec: &el_model::BlockRecord, txid: Hash32) -> Option<String> {
        let cache = self.txcache.as_ref()?;
        let entry = cache.get(&txid)?;
        match verify_cached_tx(&entry, rec, txid) {
            Ok(()) => Some(hex::encode(&entry.raw)),
            Err(e) => {
                self.stats.verify_failures.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    "[e-lite] cached transaction at height {} rejected ({})",
                    rec.height,
                    e.kind()
                );
                cache.remove(&txid);
                None
            }
        }
    }

    /// Store each requested transaction of a verified block in the
    /// transaction cache and return their hex, in request order.
    fn cache_txs(
        &self,
        rec: &el_model::BlockRecord,
        block: &bitcoin::Block,
        wanted: &[Hash32],
    ) -> Result<Vec<String>> {
        let txids = block
            .txdata
            .iter()
            .map(|tx| {
                Hash32::from_hex(&tx.compute_txid().to_string())
                    .map_err(|e| RawtxError::Verify(format!("bad txid: {e}")))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut out = Vec::with_capacity(wanted.len());
        for txid in wanted {
            let pos = txids
                .iter()
                .position(|t| t == txid)
                .ok_or_else(|| RawtxError::Verify("transaction is not in the block".into()))?;
            let raw = bitcoin::consensus::serialize(&block.txdata[pos]);
            if let Some(cache) = &self.txcache {
                if let Some((_, branch)) = el_model::merkle_branch(&txids, pos) {
                    let entry = txcache::CachedTx {
                        height: rec.height,
                        tx_pos: pos as u32,
                        branch,
                        raw: raw.clone(),
                    };
                    match cache.put(txid, &entry) {
                        Ok(()) => {
                            self.stats.cache_puts.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => eprintln!("[e-lite] tx cache put failed: {}", e.kind()),
                    }
                }
            }
            out.push(hex::encode(raw));
        }
        Ok(out)
    }

    /// A block fully verified against the indexed header (hash, header,
    /// merkle root, BIP141 commitment), from the cheapest source that has it.
    fn verified_block(
        &self,
        rec: &el_model::BlockRecord,
        priority: Priority,
    ) -> Result<(Arc<bitcoin::Block>, Origin)> {
        if let Some(found) = self.local_block(rec) {
            return Ok(found);
        }
        self.external_block(rec, priority, None)
    }

    /// The block without asking anyone outside: recent blocks in memory, the
    /// legacy block cache, then the local Core.
    fn local_block(&self, rec: &el_model::BlockRecord) -> Option<(Arc<bitcoin::Block>, Origin)> {
        let height = rec.height;
        if let Some(block) = self.recent_block(&rec.hash) {
            self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
            return Some((block, Origin::Recent));
        }
        // Legacy whole-block cache (and repair blocks).
        if let Some(cache) = &self.cache {
            if let Some(bytes) = cache.get(&rec.hash) {
                match verify_full(&bytes, rec) {
                    Ok(block) => {
                        self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
                        return Some((self.remember(rec.hash, block), Origin::BlockCache));
                    }
                    Err(e) => {
                        self.stats.verify_failures.fetch_add(1, Ordering::Relaxed);
                        eprintln!(
                            "[e-lite] cached block at height {height} rejected ({})",
                            e.kind()
                        );
                        let _ = cache.remove(&rec.hash);
                    }
                }
            }
        }
        // Local Core: zero external requests.
        if let Some(local) = &self.local {
            if let Ok(bytes) = local.fetch_block(&rec.hash) {
                match verify_full(&bytes, rec) {
                    // Kept in memory too: a burst of transactions from one
                    // block costs one `getblock`.
                    Ok(block) => return Some((self.remember(rec.hash, block), Origin::Local)),
                    Err(e) => {
                        self.stats.verify_failures.fetch_add(1, Ordering::Relaxed);
                        eprintln!("[e-lite] local rawtx rejected ({})", e.kind());
                    }
                }
            }
        }
        None
    }

    /// The block from external sources (shared across concurrent callers).
    fn external_block(
        &self,
        rec: &el_model::BlockRecord,
        priority: Priority,
        request_deadline: Option<Instant>,
    ) -> Result<(Arc<bitcoin::Block>, Origin)> {
        let height = rec.height;
        if self.sources().is_empty() {
            self.stats.unavailable.fetch_add(1, Ordering::Relaxed);
            return Err(RawtxError::NoSource(
                "no external source configured (or Tor is required and unavailable)".to_string(),
            ));
        }
        // The same block failed a moment ago (a wallet retrying it): answer
        // with that failure instead of asking every source again (AUD-158).
        if let Some(e) = self.recent_failure(&rec.hash) {
            return Err(e);
        }
        match self.fetch_external_block(rec, priority, request_deadline) {
            Ok((block, leader)) => {
                // Decoys are owed once per download, by its leader (AUD-165).
                if leader {
                    self.owe_decoys(rec.hash);
                }
                Ok((block, Origin::External))
            }
            Err(RawtxError::Verify(m)) => Err(RawtxError::Verify(m)),
            Err(e) => {
                // A prefetch failure is an optimisation that did not happen;
                // only a wallet's failure is worth an INFO line.
                if priority == Priority::Wallet {
                    eprintln!(
                        "[e-lite] rawtx at height {height} failed ({}); sources: {}",
                        e.kind(),
                        redact_hashes(&e.to_string())
                    );
                } else if debug_logs() {
                    eprintln!(
                        "[e-lite] debug: prefetch at height {height} failed ({})",
                        e.kind()
                    );
                }
                Err(e)
            }
        }
    }

    /// Verify an external block against the indexed header and keep it in
    /// memory. Runs once per download: the leader of a shared download does
    /// it before waking the requests that wait for the same block.
    fn verify_external(
        &self,
        bytes: &[u8],
        rec: &el_model::BlockRecord,
    ) -> Result<Arc<bitcoin::Block>> {
        self.stats.verifications.fetch_add(1, Ordering::Relaxed);
        match verify_full(bytes, rec) {
            Ok(block) => Ok(self.remember(rec.hash, block)),
            Err(e) => {
                self.stats.verify_failures.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    "[e-lite] external block at height {} rejected ({})",
                    rec.height,
                    e.kind()
                );
                Err(e)
            }
        }
    }

    /// Recently fetched external blocks stay in memory so a wallet asking for
    /// several transactions of one block does not download it again.
    fn recent_block(&self, hash: &Hash32) -> Option<Arc<bitcoin::Block>> {
        let recent = self.recent.lock().ok()?;
        recent
            .iter()
            .find(|(h, _)| h == hash)
            .map(|(_, b)| Arc::clone(b))
    }

    fn remember(&self, hash: Hash32, block: bitcoin::Block) -> Arc<bitcoin::Block> {
        let block = Arc::new(block);
        if let Ok(mut recent) = self.recent.lock() {
            recent.retain(|(h, _)| *h != hash);
            recent.push_back((hash, Arc::clone(&block)));
            while recent.len() > RECENT_BLOCKS {
                recent.pop_front();
            }
        }
        block
    }

    /// Decoys owed and not yet downloaded.
    pub fn decoys_pending(&self) -> usize {
        self.decoy_lane.pending()
    }

    fn cache_store(&self, hash: &Hash32, bytes: &[u8]) {
        if let Some(cache) = &self.cache {
            match cache.put(hash, bytes) {
                Ok(()) => {
                    self.stats.cache_puts.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => eprintln!("[e-lite] cache put failed: {e}"),
            }
        }
    }

    /// Owe `decoys` downloads of random indexed blocks for one real external
    /// download (wallet or prefetch), and start the decoy thread on first use.
    fn owe_decoys(&self, exclude: Hash32) {
        if self.decoys == 0 || self.sources().is_empty() {
            return;
        }
        let tip = match self.store.committed_tip() {
            Ok(Some(t)) => t.height,
            _ => return,
        };
        if tip == 0 {
            return;
        }
        let mut owed = Vec::new();
        for _ in 0..self.decoys {
            // Retry the random pick so decoys are distinct from the real block
            // and always exist in the index.
            for _ in 0..8 {
                let height = rand::random::<u32>() % (tip + 1);
                if let Ok(Some(r)) = self.store.get_block(height) {
                    if r.hash != exclude {
                        owed.push(r.hash);
                        break;
                    }
                }
            }
        }
        if owed.is_empty() {
            return;
        }
        if let Ok(mut queue) = self.decoy_lane.queue.lock() {
            for hash in owed {
                if queue.len() >= MAX_DECOY_DEBT {
                    break;
                }
                queue.push_back(hash);
            }
        }
        self.decoy_lane.cv.notify_all();
        self.start_decoy_thread();
    }

    /// One thread serves every owed decoy, one at a time, outside the
    /// wallet's download slots. It holds the sources but not the store, so a
    /// dropped resolver releases its database at once.
    fn start_decoy_thread(&self) {
        let lane = Arc::clone(&self.decoy_lane);
        let wallet_downloads = Arc::clone(&self.wallet_downloads);
        let external = self.external.clone();
        let p2p = self.p2p.clone();
        let stats = Arc::clone(&self.stats);
        self.decoy_lane.started.call_once(move || {
            let spawned = std::thread::Builder::new()
                .name("el-decoys".to_string())
                .spawn(move || {
                    while let Some(hash) = lane.next() {
                        // Spread decoys over time so they do not form a burst
                        // next to the real download.
                        let jitter = Duration::from_millis(250 + rand::random::<u64>() % 1250);
                        if !lane.pause(jitter) {
                            return;
                        }
                        let mut sources = external.clone();
                        if let Some(p2p) = &p2p {
                            if p2p.peer_count() > 0 {
                                sources.push(Arc::clone(p2p) as Arc<dyn BlockSource>);
                            }
                        }
                        // A wallet download shares the same Tor bandwidth:
                        // the decoy waits for it, at most DECOY_YIELD.
                        let yield_until = Instant::now() + DECOY_YIELD;
                        while wallet_downloads.load(Ordering::Relaxed) > 0
                            && Instant::now() < yield_until
                        {
                            if !lane.pause(Duration::from_millis(250)) {
                                return;
                            }
                        }
                        if sources.iter().any(|s| s.under_pressure()) {
                            // A provider asked us to slow down: keep the debt.
                            if !lane.pause(Duration::from_secs(5)) {
                                return;
                            }
                            continue;
                        }
                        sources.shuffle(&mut thread_rng());
                        let deadline = Instant::now() + EXTERNAL_BUDGET;
                        for source in &sources {
                            if source
                                .fetch_block_until(&hash, deadline, FetchMode::Quiet)
                                .is_ok()
                            {
                                stats.decoys.fetch_add(1, Ordering::Relaxed);
                                break;
                            }
                        }
                        lane.done();
                    }
                });
            if let Err(e) = spawned {
                eprintln!("[e-lite] decoy thread did not start: {e}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prefetch_leaves_a_download_slot_for_wallets() {
        let sem = Semaphore::new(2);
        let soon = || Instant::now() + Duration::from_millis(50);
        let wallet = sem.acquire_until(soon(), 0).expect("first slot");
        // One slot left: the prefetch may not take it, a wallet may.
        assert!(sem.acquire_until(soon(), 1).is_none());
        let second = sem.acquire_until(soon(), 0).expect("last slot");
        drop((wallet, second));
        assert!(sem.acquire_until(soon(), 1).is_some());
        // A single-slot pool never blocks the prefetch forever.
        let single = Semaphore::new(1);
        assert!(single.acquire_until(soon(), 1).is_some());
    }
}
