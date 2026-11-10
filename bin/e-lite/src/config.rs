use el_model::Chain;
use std::path::PathBuf;

/// How e-lite should treat the Bitcoin Core process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CoreManage {
    /// Never start, stop or otherwise control Core.
    External,
    /// Use Core if it is already running; otherwise start and supervise it.
    #[default]
    Auto,
    /// Always start Core; fail if another instance is already running.
    Managed,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub network: NetworkCfg,
    #[serde(default)]
    pub core: CoreCfg,
    #[serde(default)]
    pub index: IndexCfg,
    #[serde(default)]
    pub server: ServerCfg,
    #[serde(default)]
    pub metrics: MetricsCfg,
    #[serde(default)]
    pub safety: SafetyCfg,
    #[serde(default)]
    pub rawtx: RawtxCfg,
    #[serde(default)]
    pub pruning: PruningCfg,
    #[serde(default)]
    pub silent_payments: SilentPaymentsCfg,
    #[serde(default)]
    pub logging: LoggingCfg,
    #[serde(default)]
    pub tor: TorCfg,
    #[serde(default)]
    pub public_servers: PublicServersCfg,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicServersCfg {
    /// Explicit `public-servers.toml` path. Empty uses the file next to the
    /// active config file.
    #[serde(default)]
    pub file: Option<String>,
    /// Create the file from the shipped template when it is missing on a first
    /// run. Existing files are never overwritten.
    #[serde(default = "d_true")]
    pub generate: bool,
    /// Refuse to start `run` when no public provider answers for the network.
    #[serde(default = "d_true")]
    pub require_ready: bool,
    /// Timeout for each provider probe.
    #[serde(default = "d_probe_timeout")]
    pub probe_timeout_seconds: u64,
    /// How long to wait for a provider before failing `run`.
    #[serde(default = "d_ready_timeout")]
    pub ready_timeout_seconds: u64,
}

impl Default for PublicServersCfg {
    fn default() -> Self {
        Self {
            file: None,
            generate: true,
            require_ready: true,
            probe_timeout_seconds: d_probe_timeout(),
            ready_timeout_seconds: d_ready_timeout(),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkCfg {
    /// `auto`, `mainnet`, `testnet4`, `signet` or `regtest`.
    #[serde(default = "d_network")]
    pub name: String,
    #[serde(default)]
    pub data_dir: Option<String>,
}

impl Default for NetworkCfg {
    fn default() -> Self {
        NetworkCfg {
            name: d_network(),
            data_dir: None,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoreCfg {
    #[serde(default)]
    pub manage: CoreManage,
    #[serde(default = "d_bitcoind")]
    pub bitcoind: String,
    #[serde(default)]
    pub rpc_url: Option<String>,
    #[serde(default)]
    pub rpc_user: Option<String>,
    #[serde(default)]
    pub rpc_password: Option<String>,
    #[serde(default)]
    pub cookie_file: Option<String>,
    #[serde(default)]
    pub core_datadir: Option<String>,
    /// Override Core's blocks directory (for `-blocksdir` or a custom layout).
    #[serde(default)]
    pub blocks_dir: Option<String>,
    #[serde(default = "d_timeout")]
    pub timeout_seconds: u64,
    /// Core prune target in MiB. `1` enables manual pruning, where e-lite is
    /// the only component allowed to call `pruneblockchain`; this is the safe
    /// default for a from-zero pruned IBD. `0` is a full node. Values >= 550
    /// enable Core's automatic pruning and require [pruning]
    /// allow_auto_prune = true.
    #[serde(default = "d_prune_mib")]
    pub prune_mib: u64,
    /// Extra arguments passed to bitcoind when e-lite launches it.
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// Best-effort Linux I/O priority (0-7, lower is higher priority) applied
    /// to a Core launched/adopted by e-lite. None leaves the priority alone.
    #[serde(default)]
    pub io_priority: Option<u8>,
    /// Best-effort cgroup v2 `io.weight` (1-10000) for Core. Requires a
    /// delegated cgroup with the io controller; failure is a warning.
    #[serde(default)]
    pub cgroup_io_weight: Option<u64>,
    /// Restart a Core process launched by e-lite if it exits unexpectedly.
    #[serde(default = "d_true")]
    pub restart: bool,
    /// Stop a Core process launched by e-lite when e-lite stops.
    #[serde(default = "d_true")]
    pub stop_on_exit: bool,
    /// Log file for a Core process launched by e-lite.
    #[serde(default)]
    pub log_file: Option<String>,
    /// Refuse a non-loopback `rpc_url`. Core RPC is HTTP only, so credentials
    /// would cross LAN/WAN in cleartext; use an SSH/VPN tunnel or leave this off
    /// and accept the warning.
    #[serde(default)]
    pub require_loopback_rpc: bool,
}

impl Default for CoreCfg {
    fn default() -> Self {
        CoreCfg {
            manage: CoreManage::Auto,
            bitcoind: d_bitcoind(),
            rpc_url: None,
            rpc_user: None,
            rpc_password: None,
            cookie_file: None,
            core_datadir: None,
            blocks_dir: None,
            timeout_seconds: d_timeout(),
            prune_mib: d_prune_mib(),
            extra_args: Vec::new(),
            io_priority: None,
            cgroup_io_weight: None,
            restart: true,
            stop_on_exit: true,
            log_file: None,
            require_loopback_rpc: false,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexCfg {
    /// `auto` (files if readable, else RPC), `rpc`, or `files`.
    #[serde(default = "d_block_source")]
    pub block_source: String,
    /// Parallel block fetch/read workers. 0 selects a value from the CPU count.
    #[serde(default)]
    pub readers: usize,
    /// Shared RocksDB block-cache size in MiB. 0 selects a value from RAM.
    #[serde(default)]
    pub cache_mib: u64,
    #[serde(default = "d_window")]
    pub reorg_window: u32,
    /// Maximum blocks per atomic commit group.
    #[serde(default = "d_batch")]
    pub batch_blocks: u32,
    /// Startup tuning profile: auto | hdd | ssd | nvme | manual.
    #[serde(default = "d_profile")]
    pub profile: String,
    /// Let the event-driven controller adjust readers/commit/cache.
    #[serde(default = "d_true")]
    pub adaptive: bool,
    /// Commit/fsync policy: auto | always | txids.
    #[serde(default = "d_commit_sync")]
    pub commit_sync: String,
    /// Commit interval in milliseconds. 0 selects the profile/default.
    #[serde(default)]
    pub commit_interval_ms: u64,
    /// Future UTXO prefetch depth. 0 selects the profile/default.
    #[serde(default)]
    pub prefetch_blocks: usize,
    /// RocksDB write-buffer budget in MiB across all column families. 0 = auto.
    #[serde(default)]
    pub write_buffer_mib: u64,
    /// Let the controller resize the shared memtable budget at runtime. Off by
    /// default: shrinking can force a flush and stall commits.
    #[serde(default)]
    pub adaptive_memtables: bool,
    /// Optional per-block prefetch deadline in ms. 0 = no deadline; the apply
    /// path falls back to point lookups for prevouts not prefetched in time.
    #[serde(default)]
    pub prefetch_deadline_ms: u64,
    /// RAM left for the OS, Core and page cache, in MiB. 0 = auto
    /// (`max(1024, RAM/8)`).
    #[serde(default)]
    pub memory_reserve_mib: u64,
    /// Maximum share of total RAM e-lite may target (0 = 0.75).
    #[serde(default)]
    pub memory_fraction: f64,
    /// Skip the minimum-RAM check. Unsupported for real use; it exists so
    /// small CI containers can run regtest.
    #[serde(default)]
    pub allow_low_memory: bool,
    /// RocksDB background jobs. 0 = profile default, capped by CPU count.
    #[serde(default)]
    pub background_jobs: usize,
    /// Log a progress milestone every N blocks. 0 disables block milestones.
    #[serde(default = "d_milestone_blocks")]
    pub milestone_blocks: u64,
    /// Log a progress milestone every N seconds. 0 disables time milestones.
    #[serde(default = "d_milestone_seconds")]
    pub milestone_seconds: u64,
    #[serde(default = "d_true")]
    pub follow: bool,
}

impl Default for IndexCfg {
    fn default() -> Self {
        IndexCfg {
            block_source: d_block_source(),
            readers: 0,
            cache_mib: 0,
            reorg_window: d_window(),
            batch_blocks: d_batch(),
            profile: d_profile(),
            adaptive: d_true(),
            commit_sync: d_commit_sync(),
            commit_interval_ms: 0,
            prefetch_blocks: 0,
            write_buffer_mib: 0,
            adaptive_memtables: false,
            prefetch_deadline_ms: 0,
            memory_reserve_mib: 0,
            memory_fraction: 0.0,
            allow_low_memory: false,
            background_jobs: 0,
            milestone_blocks: d_milestone_blocks(),
            milestone_seconds: d_milestone_seconds(),
            follow: true,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerCfg {
    /// Plain Electrum listener. Empty = `0.0.0.0:<network default>`.
    #[serde(default)]
    pub listen: String,
    /// Serve TLS as well. Enabled by default; `false` disables it.
    #[serde(default = "d_true")]
    pub tls: bool,
    /// TLS listener. Empty = host of `listen` + `<network default>`.
    #[serde(default)]
    pub tls_listen: String,
    /// PEM certificate chain (a public CA certificate can be reused). Empty =
    /// e-lite generates and reuses a self-signed certificate in `<data-dir>/tls`.
    #[serde(default)]
    pub tls_cert: String,
    /// PEM private key matching `tls_cert`. Empty = generated with the cert.
    #[serde(default)]
    pub tls_key: String,
    /// Generate the self-signed certificate when no operator cert is configured.
    #[serde(default = "d_true")]
    pub tls_auto_cert: bool,
    /// Optional wallet banner template. Empty uses the built-in templates.
    /// If set and missing, e-lite creates it with the default template.
    #[serde(default)]
    pub banner_file: String,
    #[serde(default = "d_clients")]
    pub max_clients: usize,
    /// Per-client request rate (requests/second). `None` = auto: enabled only
    /// for non-loopback listeners. `Some(0)` disables the limiter.
    #[serde(default)]
    pub rate_limit: Option<u64>,
    /// Per-client burst capacity for the rate limiter.
    #[serde(default)]
    pub rate_burst: Option<u64>,
}

impl ServerCfg {
    pub fn tls_enabled(&self) -> bool {
        self.tls && !self.tls_listen.trim().is_empty()
    }
}

impl Default for ServerCfg {
    fn default() -> Self {
        ServerCfg {
            listen: String::new(),
            tls: true,
            tls_listen: String::new(),
            tls_cert: String::new(),
            tls_key: String::new(),
            tls_auto_cert: true,
            banner_file: String::new(),
            max_clients: d_clients(),
            rate_limit: None,
            rate_burst: None,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TorCfg {
    /// `managed` runs e-lite's own Tor; `off` disables it.
    #[serde(default = "d_tor_mode")]
    pub mode: String,
    /// Publish the plain Electrum listener as an onion service.
    #[serde(default = "d_true")]
    pub plain: bool,
    /// Publish the TLS Electrum listener as an onion service.
    #[serde(default = "d_true")]
    pub tls: bool,
    /// Generate a self-signed certificate when no operator cert is configured.
    #[serde(default = "d_true")]
    pub tls_auto_cert: bool,
    #[serde(default = "d_onion_port")]
    pub onion_port: u16,
    #[serde(default = "d_onion_tls_port")]
    pub onion_tls_port: u16,
    /// SOCKS port for the managed Tor (also used by rawtx). 0 = auto.
    #[serde(default)]
    pub socks_port: u16,
    /// Tor binary. Empty = bundled expert bundle, then `tor` in PATH.
    #[serde(default)]
    pub binary: String,
    #[serde(default = "d_tor_timeout")]
    pub start_timeout_seconds: u64,
    /// Tor home. Empty = `<data-dir>/tor`.
    #[serde(default)]
    pub data_dir: String,
    /// Optional loopback target for the plain onion port. Empty = [server] listen.
    #[serde(default)]
    pub target_plain: String,
    /// Optional loopback target for the TLS onion port. Empty = [server] tls_listen.
    #[serde(default)]
    pub target_tls: String,
}

impl TorCfg {
    pub fn managed(&self) -> bool {
        self.mode.eq_ignore_ascii_case("managed")
    }
}

impl Default for TorCfg {
    fn default() -> Self {
        TorCfg {
            mode: d_tor_mode(),
            plain: true,
            tls: true,
            tls_auto_cert: true,
            onion_port: d_onion_port(),
            onion_tls_port: d_onion_tls_port(),
            socks_port: 0,
            binary: String::new(),
            start_timeout_seconds: d_tor_timeout(),
            data_dir: String::new(),
            target_plain: String::new(),
            target_tls: String::new(),
        }
    }
}

fn d_tor_mode() -> String {
    "managed".to_string()
}

fn d_onion_port() -> u16 {
    0
}

fn d_onion_tls_port() -> u16 {
    0
}

fn d_tor_timeout() -> u64 {
    120
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsCfg {
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// Loopback metrics listener. Empty = `127.0.0.1:<network default>`.
    #[serde(default)]
    pub listen: String,
}

impl Default for MetricsCfg {
    fn default() -> Self {
        MetricsCfg {
            enabled: true,
            listen: String::new(),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SilentPaymentsCfg {
    /// Opt-in gate. The BIP352 indexer is not included yet; when disabled the
    /// protocol methods return an explicit error instead of "unknown method".
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingCfg {
    /// Application log path. Empty/None uses `<data-dir>/e-lite.log`.
    #[serde(default)]
    pub file: Option<String>,
    /// Rotate `e-lite.log` to `e-lite.log.1` above this size. 0 disables it.
    #[serde(default = "d_log_max_mib")]
    pub max_mib: u64,
}

impl Default for LoggingCfg {
    fn default() -> Self {
        LoggingCfg {
            file: None,
            max_mib: d_log_max_mib(),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PruningCfg {
    /// Pause Core downloads when free space is below this. The indexer keeps
    /// draining already-downloaded blocks so it can prune and free space.
    #[serde(default = "d_min_free")]
    pub min_free_gib: u64,
    /// Stop indexing as well when free space falls below this hard floor.
    /// Must be lower than `min_free_gib`.
    #[serde(default = "d_critical_free")]
    pub critical_free_gib: u64,
    /// Pause Core when the index lags more than this. Only used for automatic
    /// pruning (`prune_mib >= 550`); manual pruning is bounded by the prune
    /// gate instead. The effective value is capped at 224 blocks, below Core's
    /// hard `MIN_BLOCKS_TO_KEEP` (288) floor.
    #[serde(default = "d_max_lag")]
    pub max_core_lag_blocks: u32,
    /// Automatic pruning during IBD can delete a block before it is indexed.
    /// It therefore requires this explicit opt-in. From zero, use
    /// `prune_mib = 1` (manual) or `0` (full node).
    #[serde(default)]
    pub allow_auto_prune: bool,
    /// Opt-in download throttle for manual pruning / bulk catch-up. When the
    /// index lags Core by more than this many blocks during IBD, e-lite pauses
    /// Core's P2P until the index halves the gap. 0 disables it.
    #[serde(default = "d_max_bulk_lag")]
    pub max_bulk_lag_blocks: u32,
    /// Absolute unindexed backlog in GiB. This is the preferred unit: a block
    /// at 800k is several times larger than one at 100k. 0 = use the legacy
    /// block window above.
    #[serde(default = "d_max_bulk_lag_gib")]
    pub max_bulk_lag_gib: u64,
    /// Pause Core during a long bulk catch-up and pre-warm its networking when
    /// the index is close to catching up.
    #[serde(default = "d_true")]
    pub pause_core_during_bulk: bool,
    /// Resume Core's networking this many blocks before the tip so peers can
    /// reconnect while e-lite finishes the catch-up. Used only when
    /// `core_resume_ahead_gib = 0`.
    #[serde(default = "d_core_resume_ahead")]
    pub core_resume_ahead_blocks: u32,
    /// Absolute low-water mark in GiB. Core resumes when the unindexed backlog
    /// falls to this; it is the byte equivalent of `core_resume_ahead_blocks`.
    #[serde(default = "d_core_resume_ahead_gib")]
    pub core_resume_ahead_gib: u64,
    /// `overlap`: e-lite keeps indexing while Core downloads. `alternate`:
    /// Core and e-lite take turns (Core downloads, then e-lite indexes) to
    /// avoid I/O contention. `alternate` is opt-in for testing.
    #[serde(default = "d_scheduler")]
    pub scheduler: String,
    /// Skip normal pruning while free space is at or above this many GiB.
    #[serde(default = "d_prune_defer_free")]
    pub prune_defer_free_gib: u64,
    /// Minimum safe-height gain between `pruneblockchain` requests. Core
    /// prunes whole files, so a small value produces no-op calls and flushes.
    #[serde(default = "d_min_gain")]
    pub min_gain_blocks: u32,
    /// Minimum wall-clock interval between prune requests. Low disk bypasses
    /// this with an emergency prune.
    #[serde(default = "d_prune_interval")]
    pub prune_interval_seconds: u64,
    /// Largest prune gap the indexer may repair from verified external block
    /// sources (cache/P2P/Esplora). Larger gaps fail closed.
    #[serde(default = "d_repair_max_gap")]
    pub repair_max_gap_blocks: u32,
    #[serde(default = "d_poll")]
    pub poll_seconds: u64,
}

impl Default for PruningCfg {
    fn default() -> Self {
        PruningCfg {
            min_free_gib: d_min_free(),
            critical_free_gib: d_critical_free(),
            max_core_lag_blocks: d_max_lag(),
            allow_auto_prune: false,
            max_bulk_lag_blocks: d_max_bulk_lag(),
            max_bulk_lag_gib: d_max_bulk_lag_gib(),
            pause_core_during_bulk: true,
            core_resume_ahead_blocks: d_core_resume_ahead(),
            core_resume_ahead_gib: d_core_resume_ahead_gib(),
            scheduler: d_scheduler(),
            prune_defer_free_gib: d_prune_defer_free(),
            min_gain_blocks: d_min_gain(),
            prune_interval_seconds: d_prune_interval(),
            repair_max_gap_blocks: d_repair_max_gap(),
            poll_seconds: d_poll(),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawtxCfg {
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default = "d_cache_mib")]
    pub cache_mib: u64,
    #[serde(default = "d_decoys")]
    pub decoys: u32,
    #[serde(default = "d_tor_socks")]
    pub tor_socks: Option<String>,
    #[serde(default)]
    pub allow_clearnet: bool,
    /// Use clearnet when Tor is unavailable or a Tor request fails.
    #[serde(default = "d_true")]
    pub tor_fallback_clearnet: bool,
    /// Timeout for each Esplora raw-block request.
    #[serde(default = "d_rawtx_timeout")]
    pub request_timeout_seconds: u64,
    /// Skip a provider (Tor and/or direct) after a failed request.
    #[serde(default = "d_provider_cooldown")]
    pub provider_cooldown_seconds: u64,
    /// Max concurrent external block downloads (wallet + prefetch).
    #[serde(default = "d_max_parallel_fetches")]
    pub max_parallel_fetches: usize,
    /// Rawtx cache directory. Empty = `<data-dir>/rawtx-cache`. Point it at
    /// tmpfs (for example `/dev/shm/e-lite-rawtx`) to keep blocks off disk.
    #[serde(default)]
    pub cache_dir: String,
    /// Timeout for one P2P connect/handshake/block attempt.
    #[serde(default = "d_p2p_timeout")]
    pub p2p_timeout_seconds: u64,
    #[serde(default = "d_true")]
    pub use_p2p: bool,
    #[serde(default = "d_true")]
    pub use_esplora: bool,
    #[serde(default = "d_true")]
    pub p2p_v2: bool,
    /// Empty means per-network defaults.
    #[serde(default)]
    pub esplora_urls: Vec<String>,
    #[serde(default)]
    pub p2p_peers: Vec<String>,
}

impl Default for RawtxCfg {
    fn default() -> Self {
        RawtxCfg {
            enabled: true,
            cache_mib: d_cache_mib(),
            decoys: d_decoys(),
            tor_socks: d_tor_socks(),
            allow_clearnet: false,
            tor_fallback_clearnet: true,
            request_timeout_seconds: d_rawtx_timeout(),
            provider_cooldown_seconds: d_provider_cooldown(),
            max_parallel_fetches: d_max_parallel_fetches(),
            cache_dir: String::new(),
            p2p_timeout_seconds: d_p2p_timeout(),
            use_p2p: true,
            use_esplora: true,
            p2p_v2: true,
            esplora_urls: Vec::new(),
            p2p_peers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SafetyCfg {
    #[serde(default)]
    pub allow_mainnet: bool,
}

fn d_network() -> String {
    "auto".to_string()
}
fn d_bitcoind() -> String {
    "bitcoind".to_string()
}
fn d_timeout() -> u64 {
    30
}
fn d_prune_mib() -> u64 {
    1
}
fn d_batch() -> u32 {
    256
}
fn d_profile() -> String {
    "auto".to_string()
}
fn d_milestone_blocks() -> u64 {
    10_000
}
fn d_commit_sync() -> String {
    "auto".to_string()
}
fn d_milestone_seconds() -> u64 {
    21_600
}
fn d_log_max_mib() -> u64 {
    16
}
fn d_block_source() -> String {
    "auto".to_string()
}
fn d_window() -> u32 {
    288
}
fn d_true() -> bool {
    true
}
fn d_probe_timeout() -> u64 {
    5
}
fn d_ready_timeout() -> u64 {
    60
}
fn d_rawtx_timeout() -> u64 {
    8
}
fn d_provider_cooldown() -> u64 {
    300
}
fn d_max_parallel_fetches() -> usize {
    4
}
fn d_p2p_timeout() -> u64 {
    8
}
fn d_clients() -> usize {
    32
}
fn d_min_free() -> u64 {
    10
}
fn d_critical_free() -> u64 {
    1
}
fn d_max_lag() -> u32 {
    192
}
fn d_min_gain() -> u32 {
    // Core deletes whole block files anyway. 5k safe blocks (~7.5 GiB on
    // mainnet) reduces prune churn and the commit-boundary barrier frequency.
    5_000
}
fn d_max_bulk_lag() -> u32 {
    20_000
}
fn d_max_bulk_lag_gib() -> u64 {
    // ~20k mainnet blocks at the 2026 height; the byte value keeps the phase
    // comparable as blocks grow.
    30
}
fn d_core_resume_ahead() -> u32 {
    5_000
}
fn d_core_resume_ahead_gib() -> u64 {
    7
}
fn d_scheduler() -> String {
    "overlap".to_string()
}

/// Host part of a `host:port` listener, used to pair the TLS listener with the
/// plain one (wildcard stays wildcard, loopback stays loopback).
fn listener_host(addr: &str) -> String {
    let addr = addr.trim();
    match addr.rsplit_once(':') {
        Some((host, _)) if !host.is_empty() => host.to_string(),
        _ => "0.0.0.0".to_string(),
    }
}
fn d_prune_defer_free() -> u64 {
    // Keep a 20 GiB free-space margin before normal pruning; low-space
    // emergency pruning still bypasses the deferral. On a large disk this
    // avoids pruning every 1k blocks while Core is writing.
    20
}
fn d_prune_interval() -> u64 {
    30
}
fn d_repair_max_gap() -> u32 {
    2048
}
fn d_poll() -> u64 {
    10
}
fn d_cache_mib() -> u64 {
    256
}
fn d_decoys() -> u32 {
    1
}
fn d_tor_socks() -> Option<String> {
    Some("127.0.0.1:9050".to_string())
}

pub fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn default_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        paths.push(PathBuf::from(x).join("e-lite/config.toml"));
    }
    paths.push(home().join(".config/e-lite/config.toml"));
    paths.push(PathBuf::from("/etc/e-lite/config.toml"));
    paths
}

impl Config {
    /// Resolve an explicit config path or the first default path that exists.
    pub fn resolve_path(path: Option<&str>) -> Option<PathBuf> {
        match path {
            Some(p) => Some(PathBuf::from(p)),
            None => default_config_paths().into_iter().find(|p| p.is_file()),
        }
    }

    pub fn load(path: Option<&str>) -> Result<Config, String> {
        Self::load_with_overrides(path, &[])
    }

    /// Load the TOML file and apply `section.key=value` overrides. Values are
    /// parsed as TOML when possible (`true`, `42`, `["a","b"]`); otherwise they
    /// are treated as strings. This is the generic escape hatch for every
    /// configuration field without requiring a config file.
    pub fn load_with_overrides(path: Option<&str>, overrides: &[String]) -> Result<Config, String> {
        let resolved = Self::resolve_path(path);
        let text = match &resolved {
            Some(p) => std::fs::read_to_string(p)
                .map_err(|e| format!("cannot read config {}: {e}", p.display()))?,
            None => String::new(),
        };
        if overrides.is_empty() {
            return if text.trim().is_empty() {
                Ok(Config::default())
            } else {
                toml::from_str(&text).map_err(|e| {
                    let where_ = resolved
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "<defaults>".to_string());
                    format!("invalid config {where_}: {e}")
                })
            };
        }
        let mut value: toml::Value = if text.trim().is_empty() {
            toml::Value::Table(Default::default())
        } else {
            toml::from_str(&text).map_err(|e| {
                let where_ = resolved
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<defaults>".to_string());
                format!("invalid config {where_}: {e}")
            })?
        };
        for item in overrides {
            apply_override(&mut value, item)?;
        }
        value
            .try_into()
            .map_err(|e| format!("invalid config after --set: {e}"))
    }

    pub fn chain(&self) -> Result<Chain, String> {
        if self.network.name == "auto" {
            return Err(
                "network = auto must be resolved against Core or the database first".to_string(),
            );
        }
        Chain::from_name(&self.network.name)
            .ok_or_else(|| format!("unknown network: {}", self.network.name))
    }

    /// Per-network data directory. New installs use `~/e-lite/<chain>` so each
    /// instance (and its Core datadir) lives in one self-contained folder.
    /// Existing installs that still use `~/.local/share/e-lite/<chain>` keep
    /// working until the operator moves them.
    pub fn data_dir(&self, chain: Chain) -> PathBuf {
        if let Some(d) = &self.network.data_dir {
            return PathBuf::from(d);
        }
        let current = home().join("e-lite").join(chain.name());
        if !current.exists() {
            let legacy = home().join(".local/share/e-lite").join(chain.name());
            if legacy.join("CURRENT").is_file() {
                return legacy;
            }
        }
        current
    }

    pub fn rpc_url(&self, chain: Chain) -> String {
        self.core
            .rpc_url
            .clone()
            .unwrap_or_else(|| format!("http://127.0.0.1:{}", chain.default_rpc_port()))
    }

    /// Core datadir. Each network gets its own (`<data-dir>/bitcoin`) so several
    /// instances never share a Core datadir. A single legacy datadir
    /// (`~/.bitcoin` or the old `~/e-lite/bitcoin`) is still detected to avoid
    /// silently starting a fresh download; two candidates are ambiguous and the
    /// caller must choose with `[core] core_datadir`.
    pub fn core_datadir(&self, chain: Chain) -> PathBuf {
        if let Some(d) = &self.core.core_datadir {
            return PathBuf::from(d);
        }
        let current = self.data_dir(chain).join("bitcoin");
        if current.exists() {
            return current;
        }
        match self.legacy_core_datadirs(chain).as_slice() {
            [only] => only.clone(),
            _ => current,
        }
    }

    /// Existing Core datadirs that already hold this chain's data and are not
    /// the configured/default one.
    pub fn legacy_core_datadirs(&self, chain: Chain) -> Vec<PathBuf> {
        if self.core.core_datadir.is_some() {
            return Vec::new();
        }
        let current = self.data_dir(chain).join("bitcoin");
        let mut candidates = vec![
            home().join(".bitcoin"),
            home().join("e-lite").join("bitcoin"),
        ];
        candidates.retain(|dir| {
            // Only count a datadir that actually holds this chain's block
            // files. A bare bitcoin.conf (or a wallet-only ~/.bitcoin) does not
            // mean the chain data lives there, and guessing would hide a real
            // migration instead of warning about it.
            *dir != current && el_blocks::default_blocks_dir(dir, chain).is_dir()
        });
        candidates.dedup();
        candidates
    }

    pub fn cookie_file(&self) -> Option<PathBuf> {
        self.core.cookie_file.as_ref().map(PathBuf::from)
    }

    /// Core's blocks directory: explicit override, or `<datadir>/<sub>/blocks`.
    pub fn blocks_dir(&self, chain: Chain) -> PathBuf {
        match &self.core.blocks_dir {
            Some(d) => PathBuf::from(d),
            None => el_blocks::default_blocks_dir(&self.core_datadir(chain), chain),
        }
    }

    pub fn log_file(&self, chain: Chain) -> PathBuf {
        match &self.core.log_file {
            Some(p) => PathBuf::from(p),
            None => self.data_dir(chain).join("bitcoind.log"),
        }
    }

    /// Application log (`e-lite.log`). Separate from Core's `bitcoind.log`.
    pub fn app_log_file(&self, chain: Chain) -> PathBuf {
        match &self.logging.file {
            Some(p) if !p.trim().is_empty() => PathBuf::from(p),
            _ => self.data_dir(chain).join("e-lite.log"),
        }
    }

    /// Optional wallet banner template. Relative paths live in the data dir.
    pub fn app_banner_file(&self, chain: Chain) -> Option<PathBuf> {
        let path = self.server.banner_file.trim();
        if path.is_empty() {
            return None;
        }
        let path = PathBuf::from(path);
        Some(if path.is_absolute() {
            path
        } else {
            self.data_dir(chain).join(path)
        })
    }

    pub fn pid_file(&self, chain: Chain) -> PathBuf {
        self.data_dir(chain).join("core-child.pid")
    }

    /// Directory for the managed Tor instance (keys, logs, torrc).
    pub fn tor_dir(&self, chain: Chain) -> PathBuf {
        let configured = self.tor.data_dir.trim();
        if configured.is_empty() {
            self.data_dir(chain).join("tor")
        } else {
            let p = PathBuf::from(configured);
            if p.is_absolute() {
                p
            } else {
                self.data_dir(chain).join(p)
            }
        }
    }

    /// Fill the per-network listener, metrics and TLS defaults. Called after
    /// the chain is known and before validation/binding.
    pub fn apply_network_defaults(&mut self, chain: Chain) {
        if self.server.listen.trim().is_empty() {
            self.server.listen = format!("0.0.0.0:{}", chain.default_electrum_port());
        }
        if self.server.tls {
            if self.server.tls_listen.trim().is_empty() {
                let host = listener_host(&self.server.listen);
                self.server.tls_listen = format!("{host}:{}", chain.default_electrum_tls_port());
            }
            let tls_dir = self.data_dir(chain).join("tls");
            // Fill both paths only when neither was configured. A half pair
            // must reach validation and fail instead of being "completed"
            // silently with a key that does not match the operator cert.
            if self.server.tls_cert.trim().is_empty() && self.server.tls_key.trim().is_empty() {
                self.server.tls_cert = tls_dir.join("self-signed.crt").display().to_string();
                self.server.tls_key = tls_dir.join("self-signed.key").display().to_string();
            }
        } else {
            self.server.tls_listen.clear();
        }
        if self.metrics.enabled && self.metrics.listen.trim().is_empty() {
            self.metrics.listen = format!("127.0.0.1:{}", chain.default_metrics_port());
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.index.batch_blocks == 0 {
            return Err("[index] batch_blocks must be >= 1".to_string());
        }
        if self.index.reorg_window == 0 {
            return Err("[index] reorg_window must be >= 1".to_string());
        }
        if crate::tuning::Profile::parse(&self.index.profile).is_none() {
            return Err("[index] profile must be auto, hdd, ssd, nvme or manual".to_string());
        }
        if self.index.commit_interval_ms != 0
            && !(500..=30_000).contains(&self.index.commit_interval_ms)
        {
            return Err("[index] commit_interval_ms must be 0 or 500..30000".to_string());
        }
        if !matches!(self.index.commit_sync.as_str(), "auto" | "always" | "txids") {
            return Err("[index] commit_sync must be auto, always or txids".to_string());
        }
        if self.index.prefetch_deadline_ms != 0
            && !(50..=60_000).contains(&self.index.prefetch_deadline_ms)
        {
            return Err("[index] prefetch_deadline_ms must be 0 or 50..60000".to_string());
        }
        if let Some(prio) = self.core.io_priority {
            if prio > 7 {
                return Err("[core] io_priority must be 0..7".to_string());
            }
        }
        if let Some(weight) = self.core.cgroup_io_weight {
            if !(1..=10_000).contains(&weight) {
                return Err("[core] cgroup_io_weight must be 1..10000".to_string());
            }
        }
        if !matches!(self.pruning.scheduler.as_str(), "overlap" | "alternate") {
            return Err("[pruning] scheduler must be overlap or alternate".to_string());
        }
        if self.pruning.scheduler == "alternate" && !self.pruning.pause_core_during_bulk {
            return Err(
                "[pruning] scheduler = \"alternate\" requires pause_core_during_bulk = true"
                    .to_string(),
            );
        }
        if self.pruning.scheduler == "alternate" && self.core.prune_mib != 1 {
            return Err(
                "[pruning] scheduler = \"alternate\" requires prune_mib = 1 (manual prune)"
                    .to_string(),
            );
        }
        if self.pruning.max_bulk_lag_gib > 0
            && self.pruning.core_resume_ahead_gib >= self.pruning.max_bulk_lag_gib
        {
            return Err(
                "[pruning] core_resume_ahead_gib must be lower than max_bulk_lag_gib".to_string(),
            );
        }
        if self.index.memory_fraction != 0.0 && !(0.1..=1.0).contains(&self.index.memory_fraction) {
            return Err("[index] memory_fraction must be 0 (auto) or 0.1..1.0".to_string());
        }
        if self.index.memory_reserve_mib != 0 && self.index.memory_reserve_mib < 256 {
            return Err("[index] memory_reserve_mib must be 0 (auto) or >= 256".to_string());
        }
        if self.index.cache_mib != 0 && self.index.cache_mib < 8 {
            return Err("[index] cache_mib must be 0 (auto) or >= 8".to_string());
        }
        if self.index.write_buffer_mib != 0 && self.index.write_buffer_mib < 16 {
            return Err("[index] write_buffer_mib must be 0 (auto) or >= 16".to_string());
        }
        if !matches!(self.tor.mode.as_str(), "managed" | "off") {
            return Err("[tor] mode must be managed or off".to_string());
        }
        if self.tor.onion_port > 0
            && self.tor.onion_tls_port > 0
            && self.tor.onion_port == self.tor.onion_tls_port
        {
            return Err("[tor] onion_tls_port must differ from onion_port".to_string());
        }
        if self.server.max_clients == 0 {
            return Err("[server] max_clients must be >= 1".to_string());
        }
        if self.server.tls {
            if self.server.tls_cert.trim().is_empty() != self.server.tls_key.trim().is_empty() {
                return Err(
                    "[server] tls_cert and tls_key must be set together (or both empty for \
                     an automatic self-signed certificate)"
                        .to_string(),
                );
            }
            if !self.server.tls_listen.trim().is_empty()
                && self.server.tls_listen.trim() == self.server.listen.trim()
            {
                return Err("[server] tls_listen must differ from listen".to_string());
            }
        } else if !self.server.tls_listen.trim().is_empty()
            || !self.server.tls_cert.trim().is_empty()
            || !self.server.tls_key.trim().is_empty()
        {
            return Err(
                "[server] tls = false but tls_listen/tls_cert/tls_key are set; remove them \
                 or set tls = true"
                    .to_string(),
            );
        }
        if self.pruning.min_free_gib == 0 {
            return Err("[pruning] min_free_gib must be >= 1".to_string());
        }
        if self.pruning.min_gain_blocks == 0 {
            return Err("[pruning] min_gain_blocks must be >= 1".to_string());
        }
        if self.pruning.prune_interval_seconds == 0 {
            return Err("[pruning] prune_interval_seconds must be >= 1".to_string());
        }
        if self.pruning.repair_max_gap_blocks == 0 {
            return Err("[pruning] repair_max_gap_blocks must be >= 1".to_string());
        }
        if self.core.prune_mib >= 550 {
            if !self.pruning.allow_auto_prune {
                return Err(
                    "automatic pruning during IBD can delete blocks before they are indexed. \
                     From zero use [core] prune_mib = 1 (manual) or 0 (full node); to opt in \
                     anyway set [pruning] allow_auto_prune = true"
                        .to_string(),
                );
            }
            if self.pruning.max_core_lag_blocks == 0 {
                return Err(
                    "[pruning] max_core_lag_blocks = 0 disables the lag guard, which is not \
                     allowed with automatic pruning"
                        .to_string(),
                );
            }
        }
        if self.core.manage == CoreManage::Managed && self.network.name == "auto" {
            return Err(
                "[core] manage = managed needs an explicit [network] name (not auto)".to_string(),
            );
        }
        if !(self.core.prune_mib == 0 || self.core.prune_mib == 1 || self.core.prune_mib >= 550) {
            return Err(
                "[core] prune_mib must be 0 (full node), 1 (manual prune) or >= 550".to_string(),
            );
        }
        if self.core.manage == CoreManage::Managed {
            if let Some(url) = &self.core.rpc_url {
                let local =
                    url.contains("127.0.0.1") || url.contains("localhost") || url.contains("[::1]");
                if !local {
                    return Err(
                        "[core] manage = managed requires a loopback rpc_url; a remote Core \
                         must use manage = external or auto"
                            .to_string(),
                    );
                }
            }
        }
        if let Some(dir) = &self.network.data_dir {
            let data = PathBuf::from(dir);
            if let Some(core) = &self.core.core_datadir {
                let core = PathBuf::from(core);
                if data == core || data.starts_with(&core) {
                    return Err(format!(
                        "[network] data_dir {} must not be inside the Core datadir {}",
                        data.display(),
                        core.display()
                    ));
                }
            }
        }
        Ok(())
    }
}

fn apply_override(root: &mut toml::Value, item: &str) -> Result<(), String> {
    let (key, raw) = item
        .split_once('=')
        .ok_or_else(|| format!("--set expects section.key=value, got {item:?}"))?;
    let key = key.trim();
    let raw = raw.trim();
    if key.is_empty() {
        return Err(format!("--set key is empty in {item:?}"));
    }
    let parsed = match toml::from_str::<toml::Value>(&format!("v = {raw}")) {
        Ok(toml::Value::Table(mut table)) => table
            .remove("v")
            .unwrap_or_else(|| toml::Value::String(raw.to_string())),
        _ => toml::Value::String(raw.to_string()),
    };
    let parts: Vec<&str> = key.split('.').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return Err(format!("--set key is empty in {item:?}"));
    }
    let mut table = root
        .as_table_mut()
        .ok_or_else(|| "--set cannot replace the root table".to_string())?;
    for part in &parts[..parts.len() - 1] {
        let entry = table
            .entry((*part).to_string())
            .or_insert_with(|| toml::Value::Table(Default::default()));
        table = entry
            .as_table_mut()
            .ok_or_else(|| format!("--set path {key:?} conflicts with a scalar value"))?;
    }
    table.insert((*parts.last().expect("non-empty")).to_string(), parsed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_network_agnostic() {
        let cfg = Config::default();
        assert_eq!(cfg.network.name, "auto");
        assert_eq!(cfg.core.manage, CoreManage::Auto);
        assert_eq!(cfg.core.prune_mib, 1);
        assert_eq!(cfg.index.reorg_window, 288);
        assert_eq!(cfg.pruning.max_core_lag_blocks, 192);
        assert_eq!(cfg.rawtx.cache_mib, 256);
        assert!(cfg.rawtx.esplora_urls.is_empty());
    }

    #[test]
    fn rejects_data_dir_inside_core_datadir() {
        let mut cfg = Config::default();
        cfg.core.core_datadir = Some("/srv/bitcoin".to_string());
        cfg.network.data_dir = Some("/srv/bitcoin/e-lite".to_string());
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn automatic_pruning_requires_explicit_opt_in() {
        let mut cfg = Config::default();
        cfg.core.prune_mib = 550;
        assert!(cfg.validate().is_err());
        cfg.pruning.allow_auto_prune = true;
        assert!(cfg.validate().is_ok());
        cfg.pruning.max_core_lag_blocks = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn accepts_data_dir_next_to_core() {
        let mut cfg = Config::default();
        cfg.core.core_datadir = Some("/srv/bitcoin".to_string());
        cfg.network.data_dir = Some("/srv/e-lite".to_string());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn per_network_listener_defaults_do_not_collide() {
        use el_model::Chain;
        let cases = [
            (
                Chain::Mainnet,
                "0.0.0.0:55011",
                "0.0.0.0:55022",
                "127.0.0.1:55084",
            ),
            (
                Chain::Testnet4,
                "0.0.0.0:55111",
                "0.0.0.0:55122",
                "127.0.0.1:55184",
            ),
            (
                Chain::Signet,
                "0.0.0.0:55211",
                "0.0.0.0:55222",
                "127.0.0.1:55284",
            ),
            (
                Chain::Regtest,
                "0.0.0.0:55311",
                "0.0.0.0:55322",
                "127.0.0.1:55384",
            ),
        ];
        let mut seen = std::collections::HashSet::new();
        for (chain, plain, tls, metrics) in cases {
            let mut cfg = Config::default();
            cfg.apply_network_defaults(chain);
            assert_eq!(cfg.server.listen, plain);
            assert_eq!(cfg.server.tls_listen, tls);
            assert_eq!(cfg.metrics.listen, metrics);
            assert!(cfg.server.tls_enabled());
            assert!(cfg.server.tls_cert.contains(chain.name()));
            for port in [plain, tls, metrics] {
                assert!(seen.insert(port), "port {port} is reused");
            }
            cfg.validate().unwrap();
        }
    }

    #[test]
    fn loopback_plain_listener_keeps_tls_on_loopback() {
        use el_model::Chain;
        let mut cfg = Config::default();
        cfg.server.listen = "127.0.0.1:55011".to_string();
        cfg.apply_network_defaults(Chain::Mainnet);
        assert_eq!(cfg.server.tls_listen, "127.0.0.1:55022");
    }

    #[test]
    fn tls_can_be_disabled_explicitly() {
        use el_model::Chain;
        let mut cfg = Config::default();
        cfg.server.tls = false;
        cfg.apply_network_defaults(Chain::Signet);
        assert!(!cfg.server.tls_enabled());
        assert!(cfg.server.tls_listen.is_empty());
        assert!(cfg.server.tls_cert.is_empty());
        cfg.validate().unwrap();
    }

    #[test]
    fn tls_false_with_tls_fields_is_rejected() {
        let mut cfg = Config::default();
        cfg.server.tls = false;
        cfg.server.tls_listen = "127.0.0.1:50002".to_string();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn half_of_a_tls_pair_is_rejected() {
        let mut cfg = Config::default();
        cfg.server.tls_cert = "/etc/e-lite/tls/cert.pem".to_string();
        cfg.server.tls_listen = "127.0.0.1:50002".to_string();
        assert!(cfg.validate().is_err());
        cfg.server.tls_key = "/etc/e-lite/tls/key.pem".to_string();
        cfg.validate().unwrap();
    }

    #[test]
    fn tls_listener_must_differ_from_plain() {
        let mut cfg = Config::default();
        cfg.server.listen = "127.0.0.1:50001".to_string();
        cfg.server.tls_listen = "127.0.0.1:50001".to_string();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn prune_defaults_defer_and_gain_are_bounded() {
        assert_eq!(Config::default().pruning.min_gain_blocks, 5_000);
        assert_eq!(Config::default().pruning.prune_defer_free_gib, 20);
    }

    #[test]
    fn rate_limit_config_is_optional() {
        let cfg = Config::default();
        assert_eq!(cfg.server.rate_limit, None);
        assert_eq!(cfg.server.rate_burst, None);
        let cfg = Config::load_with_overrides(
            None,
            &[
                "server.rate_limit=0".to_string(),
                "server.rate_burst=50".to_string(),
            ],
        )
        .unwrap();
        assert_eq!(cfg.server.rate_limit, Some(0));
        assert_eq!(cfg.server.rate_burst, Some(50));
    }

    #[test]
    fn require_loopback_rpc_parses() {
        assert!(!Config::default().core.require_loopback_rpc);
        let cfg =
            Config::load_with_overrides(None, &["core.require_loopback_rpc=true".to_string()])
                .unwrap();
        assert!(cfg.core.require_loopback_rpc);
    }

    #[test]
    fn set_overrides_apply_to_nested_keys() {
        let cfg = Config::load_with_overrides(
            None,
            &[
                "index.profile=ssd".to_string(),
                "index.readers=6".to_string(),
                "pruning.max_bulk_lag_blocks=20000".to_string(),
                "rawtx.p2p_peers=[\"1.2.3.4:8333\"]".to_string(),
            ],
        )
        .unwrap();
        assert_eq!(cfg.index.profile, "ssd");
        assert_eq!(cfg.index.readers, 6);
        assert_eq!(cfg.pruning.max_bulk_lag_blocks, 20_000);
        assert_eq!(cfg.rawtx.p2p_peers, vec!["1.2.3.4:8333"]);
    }

    #[test]
    fn set_override_without_equals_is_rejected() {
        assert!(Config::load_with_overrides(None, &["index.profile".to_string()]).is_err());
    }

    #[test]
    fn byte_scheduler_defaults_and_validation() {
        let cfg = Config::default();
        assert_eq!(cfg.pruning.max_bulk_lag_gib, 30);
        assert_eq!(cfg.pruning.core_resume_ahead_gib, 7);
        assert_eq!(cfg.pruning.scheduler, "overlap");
        assert_eq!(cfg.pruning.max_bulk_lag_blocks, 20_000);

        let mut alt = Config::default();
        alt.pruning.scheduler = "alternate".to_string();
        assert!(alt.validate().is_ok());
        alt.pruning.pause_core_during_bulk = false;
        assert!(alt.validate().is_err());

        let mut full = Config::default();
        full.pruning.scheduler = "alternate".to_string();
        full.core.prune_mib = 0;
        assert!(full.validate().is_err());

        let mut bad = Config::default();
        bad.pruning.scheduler = "round-robin".to_string();
        assert!(bad.validate().is_err());

        let mut inverted = Config::default();
        inverted.pruning.max_bulk_lag_gib = 10;
        inverted.pruning.core_resume_ahead_gib = 10;
        assert!(inverted.validate().is_err());
    }

    #[test]
    fn adaptive_memtables_and_prefetch_deadline_are_opt_in() {
        let cfg = Config::default();
        assert!(!cfg.index.adaptive_memtables);
        assert_eq!(cfg.index.prefetch_deadline_ms, 0);
        let mut bad = Config::default();
        bad.index.prefetch_deadline_ms = 10;
        assert!(bad.validate().is_err());
        bad.index.prefetch_deadline_ms = 500;
        assert!(bad.validate().is_ok());
        let mut prio = Config::default();
        prio.core.io_priority = Some(8);
        assert!(prio.validate().is_err());
        prio.core.io_priority = Some(7);
        assert!(prio.validate().is_ok());
        let mut cg = Config::default();
        cg.core.cgroup_io_weight = Some(0);
        assert!(cg.validate().is_err());
        cg.core.cgroup_io_weight = Some(100);
        assert!(cg.validate().is_ok());
    }
}
