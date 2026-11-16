//! Electrum JSON-RPC 1.4-1.7 server and local metrics.
//!
//! Each request reads the index through a shared read lock; the indexer commits
//! under the write lock, so no response mixes two tips.

#![recursion_limit = "512"]

use el_chain::ReaderTracker;
use el_mempool::Mempool;
use el_model::sync::Mutex;
use el_model::{merkle_branch, BlockRecord, Hash32, NetworkIdentity, Outpoint, ScriptHash};
use el_rawtx::Resolver;
use el_rpc::CoreClient;
use el_store::Store;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, RwLock, RwLockReadGuard};
use std::time::{Duration, Instant, SystemTime};

mod version;
pub use version::{
    ProtocolVersion, PRE_1_6_VERSIONS, SERVER_MAX, SERVER_MIN, SERVER_VERSIONS, V1_6, V1_7,
};

/// Debug-gated diagnostics, mirrored from the binary's `[logging] level`.
/// The wire format stays byte-compatible: these lines only add information.
static DEBUG_LOGS: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// The wallet connection whose request this thread is serving (0: none).
    /// The per-txid mode keys its decoy wallets by it (AUD-163).
    static CURRENT_CLIENT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn current_client() -> u64 {
    CURRENT_CLIENT.with(|c| c.get())
}

/// Sets `CURRENT_CLIENT` for the life of a request.
struct ClientScope(u64);

impl ClientScope {
    fn enter(id: u64) -> Self {
        let old = CURRENT_CLIENT.with(|c| c.replace(id));
        ClientScope(old)
    }
}

impl Drop for ClientScope {
    fn drop(&mut self) {
        CURRENT_CLIENT.with(|c| c.set(self.0));
    }
}

/// Enable/disable the extra `debug:` lines (wallet sessions, provider notes).
pub fn set_debug_logs(on: bool) {
    DEBUG_LOGS.store(on, Ordering::Relaxed);
}

fn debug_logs() -> bool {
    DEBUG_LOGS.load(Ordering::Relaxed)
}

/// `[server] advertise_electrumx_compat`: add "(electrumx-compatible)" to the
/// `server.version` banner. Sparrow enables JSON-RPC batching only for banner
/// names it recognises and matches ElectrumX by substring; the banner still
/// starts with "e-lite", so it does not claim to be another server.
static ELECTRUMX_COMPAT: AtomicBool = AtomicBool::new(false);

pub fn set_electrumx_compat(on: bool) {
    ELECTRUMX_COMPAT.store(on, Ordering::Relaxed);
}

/// The software name sent in `server.version` and `server.features`.
fn server_banner() -> String {
    banner(ELECTRUMX_COMPAT.load(Ordering::Relaxed))
}

fn banner(electrumx_compat: bool) -> String {
    if electrumx_compat {
        format!("e-lite {VERSION} (electrumx-compatible)")
    } else {
        format!("e-lite {VERSION}")
    }
}

/// The banner this process serves, for the startup diagnostic line.
pub fn banner_for_log() -> String {
    server_banner()
}

/// Process totals at shutdown: the measurement a wallet-over-Tor run is read
/// from. One line, `debug:` only, no identifiers.
pub fn log_rawtx_summary(shared: &Shared) {
    if !debug_logs() {
        return;
    }
    let rawtx = shared.resolver.stats();
    let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let failures: Vec<String> = TxGetFailure::ALL
        .iter()
        .filter(|c| shared.stats.tx_get_failures.get(**c) > 0)
        .map(|c| format!("{} {}", c.label(), shared.stats.tx_get_failures.get(*c)))
        .collect();
    let failures = if failures.is_empty() {
        "none".to_string()
    } else {
        failures.join(" ")
    };
    let cache = shared.resolver.cache_inventory();
    eprintln!(
        "[e-lite] debug: rawtx summary: fetch {} ({}), cache hits {} puts {}, decoys {} \
         (pending {}), tx.get failed {}, batches {} ({} items, max {}), cache now {} blocks / {} \
         txs ({})",
        load(&rawtx.fetches),
        display_size(load(&rawtx.bytes)),
        load(&rawtx.cache_hits),
        load(&rawtx.cache_puts),
        load(&rawtx.decoys),
        shared.resolver.decoys_pending(),
        failures,
        load(&shared.stats.batch_lines),
        load(&shared.stats.batch_items),
        load(&shared.stats.batch_max_items),
        cache.block_files,
        cache.tx_files,
        display_size(cache.bytes()),
    );
}

/// Electrum clients ask for a script's full history. A script with millions of
/// entries would hold the index lock for too long; keep a generous cap that
/// still protects the server from pathological scripts.
pub const MAX_HISTORY_ENTRIES: usize = 100_000;
/// Spender candidates one `outpoint.get_status`/`subscribe` resolution may
/// read (AUD-167).
pub const MAX_OUTPOINT_CANDIDATES: usize = 64;

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
/// Read poll while background lines of the connection are running: the
/// reader holds the socket lock for one poll, and their answers wait for it
/// (up to 250 ms each before, AUD-164).
const CLIENT_READ_POLL_BUSY: Duration = Duration::from_millis(10);
/// A client that sends nothing for this long is disconnected.
const CLIENT_IDLE_TIMEOUT: Duration = Duration::from_secs(600);
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Notifications are pushed from one thread for every client (AUD-146).
const NOTIFY_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
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

mod client;
mod metrics;
mod notify;
mod prefetch;
mod protocol;
mod rpc;
mod server;
mod shared;
mod stats;
mod tls;
mod wire;

pub use client::*;
pub use metrics::*;
pub use notify::*;
pub use prefetch::*;
pub use protocol::*;
pub use rpc::*;
pub use server::*;
pub use shared::*;
pub use stats::*;
pub use tls::*;
pub use wire::*;

#[cfg(test)]
mod tests;
