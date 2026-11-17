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

#[derive(Debug, Clone, PartialEq, serde::Deserialize, Default)]
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

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
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
    /// `run` keeps the Electrum listeners closed until a public provider
    /// answers, when Core is pruned (an unpruned Core never needs one).
    #[serde(default = "d_true")]
    pub require_ready: bool,
    /// Timeout for each provider probe.
    #[serde(default = "d_probe_timeout")]
    pub probe_timeout_seconds: u64,
    /// How long one readiness round waits for a provider before logging and
    /// probing again.
    #[serde(default = "d_ready_timeout")]
    pub ready_timeout_seconds: u64,
    /// Open the listeners after `ready_timeout_seconds` even if no provider
    /// answered (AUD-127). Off by default: with a pruned Core some wallets
    /// (Sparrow) mishandle a failed `transaction.get` (AUD-045).
    #[serde(default)]
    pub open_without_provider: bool,
}

impl Default for PublicServersCfg {
    fn default() -> Self {
        Self {
            file: None,
            generate: true,
            require_ready: true,
            probe_timeout_seconds: d_probe_timeout(),
            ready_timeout_seconds: d_ready_timeout(),
            open_without_provider: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
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

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
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
    /// Restart a Core process launched by e-lite if it exits unexpectedly.
    #[serde(default = "d_true")]
    pub restart: bool,
    /// Stop a Core process launched by e-lite when e-lite stops.
    #[serde(default = "d_true")]
    pub stop_on_exit: bool,
    /// Log file for a Core process launched by e-lite.
    #[serde(default)]
    pub log_file: Option<String>,
    /// P2P networks of a Core started by e-lite (AUD-136):
    /// `auto` (default) keeps clearnet and adds every private network that is
    /// available (Tor onion peers through the Tor e-lite uses, I2P when a SAM
    /// router answers, CJDNS when enabled); `private` uses **only** those
    /// networks, so the node's IP is never shown to a Bitcoin peer (slower
    /// IBD); `clearnet` adds nothing.
    #[serde(default = "d_core_network")]
    pub network: String,
    /// Broadcast wallet transactions through short-lived Tor/I2P connections
    /// (`-privatebroadcast`, experimental in Core, needs a version that has
    /// it). The transaction reaches Core's mempool only when the network
    /// relays it back, so wallets see it a few seconds later.
    #[serde(default)]
    pub private_broadcast: bool,
    /// I2P SAM router for Core: `auto` (default) uses 127.0.0.1:7656 when it
    /// answers, an explicit `host:port` forces it, empty disables it.
    #[serde(default = "d_i2p_sam")]
    pub i2p_sam: String,
    /// This host has a CJDNS interface (`-cjdnsreachable`).
    #[serde(default)]
    pub cjdns: bool,
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
            restart: true,
            stop_on_exit: true,
            log_file: None,
            network: d_core_network(),
            private_broadcast: false,
            i2p_sam: d_i2p_sam(),
            cjdns: false,
            require_loopback_rpc: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
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
    /// RocksDB `max_open_files` (fds kept for table/index/filter readers).
    /// 0 = 8192; lowering to 2048/4096 shrinks `table_readers` memory on
    /// 8-16 GiB hosts, at the cost of reopened table readers.
    #[serde(default)]
    pub max_open_files: u32,
    /// RAM left for the OS, Core and page cache, in MiB. 0 = auto
    /// (`clamp(effective/8, 2048, 4096)`; `effective/4` at 2 GiB or less).
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
            max_open_files: 0,
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

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerCfg {
    /// Plain (unencrypted) Electrum listener. Empty = the network default
    /// port on loopback only: wallets on this host and the onion service use
    /// it, the LAN gets TLS (AUD-147). With `plain_lan = true`, or when TLS
    /// is disabled, the default is `0.0.0.0`. An explicit address always wins.
    #[serde(default)]
    pub listen: String,
    /// Offer the plain listener on every interface (`0.0.0.0`). Off by
    /// default: with TLS available, cleartext on the network is opt-in.
    #[serde(default)]
    pub plain_lan: bool,
    /// Add "(electrumx-compatible)" to the `server.version` banner so
    /// Sparrow sends JSON-RPC batches (it recognises ElectrumX by substring).
    /// Off by default: other wallets that look for "electrumx" may change
    /// their behaviour too. Hot-reloadable.
    #[serde(default)]
    pub advertise_electrumx_compat: bool,
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
            plain_lan: false,
            advertise_electrumx_compat: false,
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

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
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

fn d_core_network() -> String {
    "auto".to_string()
}
fn d_i2p_sam() -> String {
    "auto".to_string()
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

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
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

#[derive(Debug, Clone, PartialEq, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SilentPaymentsCfg {
    /// Opt-in gate. The BIP352 indexer is not included yet; when disabled the
    /// protocol methods return an explicit error instead of "unknown method".
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingCfg {
    /// Application log path. Empty/None uses `<data-dir>/e-lite.log`.
    #[serde(default)]
    pub file: Option<String>,
    /// Rotate `e-lite.log` to `e-lite.log.1` above this size. 0 disables it.
    #[serde(default = "d_log_max_mib")]
    pub max_mib: u64,
    /// Verbosity of the extra diagnostic lines: `info` prints only the
    /// standard lines, `debug` adds per-family/levels/statistics/pressure/
    /// controller detail, `trace` adds per-commit-group lines. Alpha default
    /// is `debug`; stable releases default to `info`.
    #[serde(default = "d_log_level")]
    pub level: String,
}

fn d_log_level() -> String {
    "debug".to_string()
}

impl Default for LoggingCfg {
    fn default() -> Self {
        LoggingCfg {
            file: None,
            max_mib: d_log_max_mib(),
            level: d_log_level(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
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
    /// Manual pruning: pause Core's downloads when the unindexed backlog
    /// reaches this many GiB. Bytes, not blocks: a block at 800k is several
    /// times larger than one at 100k. Must be at least 1.
    #[serde(default = "d_max_bulk_lag_gib")]
    pub max_bulk_lag_gib: u64,
    /// Pause Core during a long bulk catch-up and pre-warm its networking when
    /// the index is close to catching up.
    #[serde(default = "d_true")]
    pub pause_core_during_bulk: bool,
    /// Low-water mark in GiB: Core resumes when the unindexed backlog falls to
    /// this, so its peers reconnect while e-lite finishes the catch-up.
    #[serde(default = "d_core_resume_ahead_gib")]
    pub core_resume_ahead_gib: u64,
    /// Skip normal pruning while free space is at or above this many GiB.
    #[serde(default = "d_prune_defer_free")]
    pub prune_defer_free_gib: u64,
    /// Minimum safe-height gain between `pruneblockchain` requests. Core
    /// prunes whole files, so a small value produces no-op calls and flushes.
    #[serde(default = "d_min_gain")]
    pub min_gain_blocks: u32,
    /// Once fully synced the safe cut advances ~144 blocks/day at mainnet
    /// rates; `min_gain_blocks` (5,000) would hold ~7.5 GiB for ~35 days.
    /// 0 = use `min_gain_blocks` in live mode too.
    #[serde(default = "d_min_gain_live")]
    pub min_gain_live_blocks: u32,
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
            max_bulk_lag_gib: d_max_bulk_lag_gib(),
            pause_core_during_bulk: true,
            core_resume_ahead_gib: d_core_resume_ahead_gib(),
            prune_defer_free_gib: d_prune_defer_free(),
            min_gain_blocks: d_min_gain(),
            min_gain_live_blocks: d_min_gain_live(),
            prune_interval_seconds: d_prune_interval(),
            repair_max_gap_blocks: d_repair_max_gap(),
            poll_seconds: d_poll(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
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
    /// Use Tor for every external fetch (Esplora, Electrum by txid, P2P).
    /// On by default. `false` is an explicit opt-out: connections go direct
    /// and the operator IP is visible to providers and peers (AUD-168).
    /// The onion listener is independent (`[tor] mode`).
    #[serde(default = "d_true")]
    pub use_tor: bool,
    #[serde(default)]
    pub allow_clearnet: bool,
    /// Use clearnet when Tor is unavailable or a Tor request fails. Off by
    /// default (AUD-128): the fallback ties the operator IP to the block
    /// hashes of wallet transactions.
    #[serde(default)]
    pub tor_fallback_clearnet: bool,
    /// Download the blocks of a wallet's confirmed history in the background
    /// as soon as the history is requested. On by default: each external
    /// download owes decoys like a wallet request and leaves a download slot
    /// free for the wallet (AUD-159, supersedes the AUD-134 default).
    #[serde(default = "d_true")]
    pub prefetch: bool,
    /// Timeout for each direct (clearnet) Esplora raw-block request.
    #[serde(default = "d_rawtx_timeout")]
    pub request_timeout_seconds: u64,
    /// Absolute limit of one Esplora raw-block attempt. Every Tor request
    /// builds its own circuit (AUD-135) and downloads a whole block; a
    /// transfer that keeps receiving bytes may use all of it (AUD-155).
    #[serde(default = "d_rawtx_tor_timeout")]
    pub tor_request_timeout_seconds: u64,
    /// Circuit + connection time of an Esplora request through Tor
    /// (doubled for `.onion`). A silent attempt is hedged after 6 s anyway.
    #[serde(default = "d_rawtx_tor_connect")]
    pub tor_connect_timeout_seconds: u64,
    /// No response header or body byte for this long through Tor: the
    /// attempt is stalled (doubled for `.onion`).
    #[serde(default = "d_rawtx_tor_stall")]
    pub tor_stall_timeout_seconds: u64,
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
    /// Timeout for one direct P2P connect/handshake/block attempt.
    #[serde(default = "d_p2p_timeout")]
    pub p2p_timeout_seconds: u64,
    /// Timeout for one P2P attempt through Tor (circuit + whole block).
    #[serde(default = "d_p2p_tor_timeout")]
    pub p2p_tor_timeout_seconds: u64,
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
    /// How a pruned transaction is resolved: `auto` races the block download
    /// against the per-txid path, `tx` tries the loose transaction first and
    /// `block` fetches the whole block only (no txid leaves the node).
    #[serde(default = "d_rawtx_path")]
    pub path: String,
    /// Total wallet request deadline (queue + network), seconds.
    #[serde(default = "d_wallet_deadline")]
    pub wallet_deadline_seconds: u64,
    /// Budget of one per-txid attempt, seconds; the provider is replaced
    /// when it misses it.
    #[serde(default = "d_txid_attempt")]
    pub txid_attempt_seconds: u64,
    /// In `auto`, give an in-flight block a short head start and race it
    /// against the per-txid path.
    #[serde(default = "d_true")]
    pub txid_race: bool,
    /// `same_round` (default) sends the decoy wallets in the same dispatch as
    /// the real one; `background` keeps only one while real requests wait.
    #[serde(default = "d_decoy_sessions")]
    pub decoy_sessions: String,
    /// Global concurrent per-txid sessions (1..32).
    #[serde(default = "d_sessions_total")]
    pub sessions_total: usize,
    /// Random delay before a per-txid session, milliseconds (0..250): breaks
    /// the timing correlation with the wallet connection.
    #[serde(default = "d_jitter_ms")]
    pub jitter_ms: u64,
    /// History blocks queued per history response (0..4096). 0 = auto: block
    /// mode prefetches the history, per-txid mode only on demand.
    #[serde(default = "d_prefetch_history_blocks")]
    pub prefetch_history_blocks: usize,
    /// Blocks in flight with one P2P peer (1..4).
    #[serde(default = "d_p2p_blocks_per_peer")]
    pub p2p_blocks_per_peer: usize,
    /// Long-term per-provider quota as a multiple of the fair share
    /// (0 disables; otherwise 1.0..4.0).
    #[serde(default = "d_provider_quota")]
    pub provider_quota: f64,
    /// `privacy.toml` path. Empty = next to config.toml (AUD-163).
    #[serde(default)]
    pub privacy_file: Option<String>,
    /// `--set privacy.*` overrides, filled from the command line.
    #[serde(skip)]
    pub privacy_overrides: Vec<String>,
}

impl RawtxCfg {
    /// SOCKS proxy the external path may use, honouring the `use_tor` opt-out.
    pub fn effective_tor_socks(&self) -> Option<String> {
        if !self.use_tor {
            return None;
        }
        self.tor_socks
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    /// Whether direct connections are allowed. `use_tor = false` implies it:
    /// otherwise there would be no source at all.
    pub fn effective_allow_clearnet(&self) -> bool {
        self.allow_clearnet || !self.use_tor
    }

    /// Transport in force, for logs and `/status`.
    pub fn transport(&self) -> &'static str {
        if !self.use_tor {
            "direct"
        } else if self.tor_fallback_clearnet {
            "tor+fallback"
        } else {
            "tor"
        }
    }
}

impl Default for RawtxCfg {
    fn default() -> Self {
        RawtxCfg {
            enabled: true,
            cache_mib: d_cache_mib(),
            decoys: d_decoys(),
            tor_socks: d_tor_socks(),
            use_tor: true,
            allow_clearnet: false,
            tor_fallback_clearnet: false,
            prefetch: true,
            request_timeout_seconds: d_rawtx_timeout(),
            tor_request_timeout_seconds: d_rawtx_tor_timeout(),
            tor_connect_timeout_seconds: d_rawtx_tor_connect(),
            tor_stall_timeout_seconds: d_rawtx_tor_stall(),
            provider_cooldown_seconds: d_provider_cooldown(),
            max_parallel_fetches: d_max_parallel_fetches(),
            cache_dir: String::new(),
            p2p_timeout_seconds: d_p2p_timeout(),
            p2p_tor_timeout_seconds: d_p2p_tor_timeout(),
            use_p2p: true,
            use_esplora: true,
            p2p_v2: true,
            esplora_urls: Vec::new(),
            p2p_peers: Vec::new(),
            path: d_rawtx_path(),
            wallet_deadline_seconds: d_wallet_deadline(),
            txid_attempt_seconds: d_txid_attempt(),
            txid_race: true,
            decoy_sessions: d_decoy_sessions(),
            sessions_total: d_sessions_total(),
            jitter_ms: d_jitter_ms(),
            prefetch_history_blocks: d_prefetch_history_blocks(),
            p2p_blocks_per_peer: d_p2p_blocks_per_peer(),
            provider_quota: d_provider_quota(),
            privacy_file: None,
            privacy_overrides: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SafetyCfg {
    #[serde(default)]
    pub allow_mainnet: bool,
    /// `run`/`serve`/`index` restart the node after an abnormal end. `false`
    /// is the same as `--no-supervise`: one process, no restarts.
    #[serde(default = "d_true")]
    pub supervise: bool,
}

impl Default for SafetyCfg {
    fn default() -> Self {
        SafetyCfg {
            allow_mainnet: false,
            supervise: true,
        }
    }
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
fn d_rawtx_tor_timeout() -> u64 {
    60
}
fn d_rawtx_tor_connect() -> u64 {
    10
}
fn d_rawtx_tor_stall() -> u64 {
    12
}
fn d_p2p_tor_timeout() -> u64 {
    30
}
fn d_provider_cooldown() -> u64 {
    300
}
fn d_max_parallel_fetches() -> usize {
    16
}
fn d_rawtx_path() -> String {
    "auto".to_string()
}
fn d_wallet_deadline() -> u64 {
    12
}
fn d_txid_attempt() -> u64 {
    3
}
fn d_decoy_sessions() -> String {
    "same_round".to_string()
}
fn d_sessions_total() -> usize {
    16
}
fn d_jitter_ms() -> u64 {
    50
}
fn d_prefetch_history_blocks() -> usize {
    0
}
fn d_p2p_blocks_per_peer() -> usize {
    3
}
fn d_provider_quota() -> f64 {
    1.5
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
fn d_min_gain_live() -> u32 {
    // Fully synced, the safe cut advances ~144 blocks/day at mainnet rates:
    // prune once a day (~220 MiB) instead of holding ~7.5 GiB for ~35 days.
    144
}
fn d_max_bulk_lag_gib() -> u64 {
    // ~20k mainnet blocks at the 2026 height; the byte value keeps the phase
    // comparable as blocks grow.
    30
}
fn d_core_resume_ahead_gib() -> u64 {
    7
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
        let where_ = || {
            resolved
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<defaults>".to_string())
        };
        let text = match &resolved {
            Some(p) => std::fs::read_to_string(p)
                .map_err(|e| format!("cannot read config {}: {e}", p.display()))?,
            None => String::new(),
        };
        let mut value: toml::Value = if text.trim().is_empty() {
            toml::Value::Table(Default::default())
        } else {
            toml::from_str(&text).map_err(|e| format!("invalid config {}: {e}", where_()))?
        };
        for item in overrides {
            apply_override(&mut value, item)?;
        }
        drop_retired_keys(&mut value);
        let context = if overrides.is_empty() {
            format!("invalid config {}", where_())
        } else {
            "invalid config after --set".to_string()
        };
        value.try_into().map_err(|e| format!("{context}: {e}"))
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
        // An explicit `listen` keeps its old meaning: TLS follows its host.
        // By default TLS is the network listener and plain stays on loopback.
        let explicit_plain = !self.server.listen.trim().is_empty();
        if !explicit_plain {
            let host = if self.server.plain_lan || !self.server.tls {
                "0.0.0.0"
            } else {
                "127.0.0.1"
            };
            self.server.listen = format!("{host}:{}", chain.default_electrum_port());
        }
        if self.server.tls {
            if self.server.tls_listen.trim().is_empty() {
                let host = if explicit_plain {
                    listener_host(&self.server.listen)
                } else {
                    "0.0.0.0".to_string()
                };
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
        if !matches!(self.core.network.as_str(), "auto" | "private" | "clearnet") {
            return Err(format!(
                "[core] network = \"{}\" is not valid; use auto, private or clearnet",
                self.core.network
            ));
        }
        if self.index.batch_blocks == 0 {
            return Err("[index] batch_blocks must be >= 1".to_string());
        }
        if el_rawtx::ExternalPath::parse(&self.rawtx.path).is_none() {
            return Err("[rawtx] path must be auto, tx or block".to_string());
        }
        if !(3..=60).contains(&self.rawtx.wallet_deadline_seconds) {
            return Err("[rawtx] wallet_deadline_seconds must be 3..60".to_string());
        }
        if !(1..=30).contains(&self.rawtx.txid_attempt_seconds) {
            return Err("[rawtx] txid_attempt_seconds must be 1..30".to_string());
        }
        if self.rawtx.txid_attempt_seconds > self.rawtx.wallet_deadline_seconds {
            return Err(
                "[rawtx] txid_attempt_seconds must not exceed wallet_deadline_seconds".to_string(),
            );
        }
        if el_rawtx::DecoySessions::parse(&self.rawtx.decoy_sessions).is_none() {
            return Err("[rawtx] decoy_sessions must be same_round or background".to_string());
        }
        if !(1..=32).contains(&self.rawtx.sessions_total) {
            return Err("[rawtx] sessions_total must be 1..32".to_string());
        }
        if self.rawtx.jitter_ms > 250 {
            return Err("[rawtx] jitter_ms must be 0..250".to_string());
        }
        if self.rawtx.prefetch_history_blocks > 4096 {
            return Err("[rawtx] prefetch_history_blocks must be 0..4096".to_string());
        }
        if !(1..=4).contains(&self.rawtx.p2p_blocks_per_peer) {
            return Err("[rawtx] p2p_blocks_per_peer must be 1..4".to_string());
        }
        if self.rawtx.provider_quota != 0.0 && !(1.0..=4.0).contains(&self.rawtx.provider_quota) {
            return Err("[rawtx] provider_quota must be 0 (off) or 1.0..4.0".to_string());
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
        if crate::logging::parse(&self.logging.level).is_none() {
            return Err(
                "[logging] level must be info, debug or trace (alpha default: debug)".to_string(),
            );
        }
        if let Some(prio) = self.core.io_priority {
            if prio > 7 {
                return Err("[core] io_priority must be 0..7".to_string());
            }
        }
        if self.index.max_open_files > 0 && self.index.max_open_files < 256 {
            return Err("[index] max_open_files must be 0 (auto) or >= 256".to_string());
        }
        if self.pruning.max_bulk_lag_gib == 0 {
            return Err(
                "[pruning] max_bulk_lag_gib must be at least 1 (the block-count window that 0 \
                 used to select was removed in 0.1.0-alpha.29.19.0)"
                    .to_string(),
            );
        }
        if self.pruning.core_resume_ahead_gib >= self.pruning.max_bulk_lag_gib {
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
        if self.pruning.min_gain_live_blocks > self.pruning.min_gain_blocks {
            return Err("[pruning] min_gain_live_blocks must be <= min_gain_blocks".to_string());
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

/// Keys that existed and were removed. A configuration that still has one
/// starts, with one warning naming it, instead of failing on an unknown
/// field: every other unknown key is still an error.
pub const RETIRED_KEYS: &[(&str, &str)] = &[
    (
        "index.prefetch_deadline_ms",
        "removed in 0.1.0-alpha.29.19.0; prefetch always completes",
    ),
    (
        "index.adaptive_memtables",
        "removed in 0.1.0-alpha.29.19.0; memtables are fixed at startup and only shrink when memory is critical",
    ),
    (
        "pruning.max_bulk_lag_blocks",
        "removed in 0.1.0-alpha.29.19.0; max_bulk_lag_gib is the only window",
    ),
    (
        "pruning.core_resume_ahead_blocks",
        "removed in 0.1.0-alpha.29.19.0; core_resume_ahead_gib is the only window",
    ),
    (
        "pruning.scheduler",
        "removed in 0.1.0-alpha.29.19.0; e-lite always indexes while Core downloads",
    ),
    (
        "index.zfs_arc_dynamic",
        "removed in 0.1.0-alpha.29.19.0; e-lite no longer writes the ZFS ARC cap",
    ),
    (
        "index.zfs_arc_max_mib",
        "removed in 0.1.0-alpha.29.19.0; set zfs_arc_max yourself if you want a cap",
    ),
    (
        "index.zfs_arc_min_mib",
        "removed in 0.1.0-alpha.29.19.0; set zfs_arc_max yourself if you want a cap",
    ),
    (
        "core.cgroup_io_weight",
        "removed in 0.1.0-alpha.29.19.0; [core] io_priority (ionice) remains",
    ),
];

/// Remove retired keys from a parsed configuration, warning once per key and
/// process.
fn drop_retired_keys(root: &mut toml::Value) {
    static WARNED: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    for (key, note) in RETIRED_KEYS {
        let Some((section, field)) = key.split_once('.') else {
            continue;
        };
        let removed = root
            .get_mut(section)
            .and_then(toml::Value::as_table_mut)
            .and_then(|table| table.remove(field))
            .is_some();
        if removed {
            let mut warned = WARNED.lock().unwrap_or_else(|p| p.into_inner());
            if !warned.contains(key) {
                warned.push(key);
                eprintln!(
                    "[e-lite] WARNING: [{section}] {field} is no longer used ({note}); remove it"
                );
            }
        }
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
    fn wallet_rawtx_defaults_and_validation() {
        let cfg = Config::default();
        assert_eq!(cfg.rawtx.path, "auto");
        assert_eq!(cfg.rawtx.wallet_deadline_seconds, 12);
        assert_eq!(cfg.rawtx.txid_attempt_seconds, 3);
        assert!(cfg.rawtx.txid_race);
        assert_eq!(cfg.rawtx.decoy_sessions, "same_round");
        assert_eq!(cfg.rawtx.sessions_total, 16);
        assert_eq!(cfg.rawtx.jitter_ms, 50);
        assert_eq!(cfg.rawtx.prefetch_history_blocks, 0);
        assert_eq!(cfg.rawtx.p2p_blocks_per_peer, 3);
        assert_eq!(cfg.rawtx.provider_quota, 1.5);
        assert!(cfg.validate().is_ok());

        let mut bad = Config::default();
        bad.rawtx.path = "sometimes".into();
        assert!(bad.validate().is_err());
        bad.rawtx.path = "auto".into();
        bad.rawtx.wallet_deadline_seconds = 2;
        assert!(bad.validate().is_err());
        bad.rawtx.wallet_deadline_seconds = 12;
        bad.rawtx.txid_attempt_seconds = 13;
        assert!(bad.validate().is_err());
        bad.rawtx.txid_attempt_seconds = 3;
        bad.rawtx.decoy_sessions = "later".into();
        assert!(bad.validate().is_err());
        bad.rawtx.decoy_sessions = "background".into();
        bad.rawtx.sessions_total = 0;
        assert!(bad.validate().is_err());
        bad.rawtx.sessions_total = 16;
        bad.rawtx.jitter_ms = 251;
        assert!(bad.validate().is_err());
        bad.rawtx.jitter_ms = 50;
        bad.rawtx.prefetch_history_blocks = 4097;
        assert!(bad.validate().is_err());
        bad.rawtx.prefetch_history_blocks = 0;
        bad.rawtx.p2p_blocks_per_peer = 5;
        assert!(bad.validate().is_err());
        bad.rawtx.p2p_blocks_per_peer = 3;
        bad.rawtx.provider_quota = 0.5;
        assert!(bad.validate().is_err());
        bad.rawtx.provider_quota = 0.0;
        assert!(bad.validate().is_ok());
    }

    #[test]
    fn wallet_rawtx_keys_parse_from_overrides() {
        let cfg = Config::load_with_overrides(
            None,
            &[
                "rawtx.path=block".to_string(),
                "rawtx.wallet_deadline_seconds=20".to_string(),
                "rawtx.txid_attempt_seconds=4".to_string(),
                "rawtx.txid_race=false".to_string(),
                "rawtx.decoy_sessions=background".to_string(),
                "rawtx.sessions_total=24".to_string(),
                "rawtx.jitter_ms=0".to_string(),
                "rawtx.prefetch_history_blocks=128".to_string(),
                "rawtx.p2p_blocks_per_peer=2".to_string(),
                "rawtx.provider_quota=2.0".to_string(),
            ],
        )
        .unwrap();
        assert_eq!(cfg.rawtx.path, "block");
        assert_eq!(cfg.rawtx.wallet_deadline_seconds, 20);
        assert_eq!(cfg.rawtx.txid_attempt_seconds, 4);
        assert!(!cfg.rawtx.txid_race);
        assert_eq!(cfg.rawtx.decoy_sessions, "background");
        assert_eq!(cfg.rawtx.sessions_total, 24);
        assert_eq!(cfg.rawtx.jitter_ms, 0);
        assert_eq!(cfg.rawtx.prefetch_history_blocks, 128);
        assert_eq!(cfg.rawtx.p2p_blocks_per_peer, 2);
        assert_eq!(cfg.rawtx.provider_quota, 2.0);
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

    /// The supervisor is on unless the configuration says otherwise, with or
    /// without a `[safety]` section.
    #[test]
    fn supervise_defaults_to_on() {
        assert!(Config::default().safety.supervise);
        let no_section: Config = toml::from_str("[network]\nname = \"regtest\"\n").unwrap();
        assert!(no_section.safety.supervise);
        let other_key: Config = toml::from_str("[safety]\nallow_mainnet = true\n").unwrap();
        assert!(other_key.safety.supervise);
        let off: Config = toml::from_str("[safety]\nsupervise = false\n").unwrap();
        assert!(!off.safety.supervise);
    }

    #[test]
    fn per_network_listener_defaults_do_not_collide() {
        use el_model::Chain;
        let cases = [
            (
                Chain::Mainnet,
                "127.0.0.1:55011",
                "0.0.0.0:55022",
                "127.0.0.1:55084",
            ),
            (
                Chain::Testnet4,
                "127.0.0.1:55111",
                "0.0.0.0:55122",
                "127.0.0.1:55184",
            ),
            (
                Chain::Signet,
                "127.0.0.1:55211",
                "0.0.0.0:55222",
                "127.0.0.1:55284",
            ),
            (
                Chain::Regtest,
                "127.0.0.1:55311",
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
    fn plain_on_the_network_is_opt_in() {
        // AUD-147: by default cleartext stays on loopback and TLS serves the
        // network.
        use el_model::Chain;
        let mut cfg = Config::default();
        cfg.server.plain_lan = true;
        cfg.apply_network_defaults(Chain::Mainnet);
        assert_eq!(cfg.server.listen, "0.0.0.0:55011");
        assert_eq!(cfg.server.tls_listen, "0.0.0.0:55022");
        // Without TLS the plain listener is the only one: keep it reachable.
        let mut cfg = Config::default();
        cfg.server.tls = false;
        cfg.apply_network_defaults(Chain::Mainnet);
        assert_eq!(cfg.server.listen, "0.0.0.0:55011");
        // An explicit address is never rewritten, and TLS follows its host.
        let mut cfg = Config::default();
        cfg.server.listen = "192.168.1.5:50001".to_string();
        cfg.apply_network_defaults(Chain::Mainnet);
        assert_eq!(cfg.server.listen, "192.168.1.5:50001");
        assert_eq!(cfg.server.tls_listen, "192.168.1.5:55022");
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
                "pruning.max_bulk_lag_gib=40".to_string(),
                "rawtx.p2p_peers=[\"1.2.3.4:8333\"]".to_string(),
            ],
        )
        .unwrap();
        assert_eq!(cfg.index.profile, "ssd");
        assert_eq!(cfg.index.readers, 6);
        assert_eq!(cfg.pruning.max_bulk_lag_gib, 40);
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
        let mut no_pause = Config::default();
        no_pause.pruning.pause_core_during_bulk = false;
        assert!(no_pause.validate().is_ok());

        let mut inverted = Config::default();
        inverted.pruning.max_bulk_lag_gib = 10;
        inverted.pruning.core_resume_ahead_gib = 10;
        assert!(inverted.validate().is_err());

        let mut zero = Config::default();
        zero.pruning.max_bulk_lag_gib = 0;
        let err = zero.validate().unwrap_err();
        assert!(err.contains("max_bulk_lag_gib"), "{err}");
    }

    #[test]
    fn opt_in_knobs_are_validated() {
        let mut fds = Config::default();
        fds.index.max_open_files = 128;
        assert!(fds.validate().is_err());
        fds.index.max_open_files = 0;
        assert!(fds.validate().is_ok());
        fds.index.max_open_files = 2048;
        assert!(fds.validate().is_ok());
        let mut prio = Config::default();
        prio.core.io_priority = Some(8);
        assert!(prio.validate().is_err());
        prio.core.io_priority = Some(7);
        assert!(prio.validate().is_ok());
    }

    #[test]
    fn config_example_matches_defaults() {
        // `config.example.toml` says every shown value is the default. Keep it
        // true: unknown keys fail through deny_unknown_fields, and any value
        // that drifted shows here with the field name.
        let text = include_str!("../../../config.example.toml");
        let example: Config = toml::from_str(text).expect("config.example.toml must parse");
        assert_eq!(example, Config::default());
    }

    #[test]
    fn use_tor_is_on_by_default_and_false_means_direct() {
        let cfg = Config::default();
        assert!(cfg.rawtx.use_tor);
        assert_eq!(
            cfg.rawtx.effective_tor_socks().as_deref(),
            Some("127.0.0.1:9050")
        );
        assert!(!cfg.rawtx.effective_allow_clearnet());
        assert_eq!(cfg.rawtx.transport(), "tor");

        let mut direct = Config::default();
        direct.rawtx.use_tor = false;
        assert_eq!(direct.rawtx.effective_tor_socks(), None);
        assert!(
            direct.rawtx.effective_allow_clearnet(),
            "direct needs clearnet"
        );
        assert_eq!(direct.rawtx.transport(), "direct");

        let mut fallback = Config::default();
        fallback.rawtx.tor_fallback_clearnet = true;
        assert_eq!(fallback.rawtx.transport(), "tor+fallback");
    }

    #[test]
    fn use_tor_parses_from_set() {
        let cfg = Config::load_with_overrides(None, &["rawtx.use_tor=false".to_string()]).unwrap();
        assert!(!cfg.rawtx.use_tor);
        assert!(cfg.rawtx.effective_allow_clearnet());
    }

    /// A configuration that still has a removed key starts (with a warning);
    /// any other unknown key is still an error.
    #[test]
    fn retired_config_keys_are_ignored_and_unknown_keys_still_fail() {
        let dir = std::env::temp_dir().join(format!("e-lite-retired-keys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut text = String::new();
        for (key, _) in RETIRED_KEYS {
            let (section, field) = key.split_once('.').unwrap();
            text.push_str(&format!("[{section}]\n{field} = 1\n"));
        }
        // Several retired keys can share a section: write each section once.
        let mut by_section: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
        for (key, _) in RETIRED_KEYS {
            let (section, field) = key.split_once('.').unwrap();
            by_section.entry(section).or_default().push(field);
        }
        text.clear();
        for (section, fields) in &by_section {
            text.push_str(&format!("[{section}]\n"));
            for field in fields {
                text.push_str(&format!("{field} = 1\n"));
            }
        }
        std::fs::write(&path, &text).unwrap();
        let cfg = Config::load(Some(path.to_str().unwrap())).expect("retired keys must not fail");
        assert_eq!(cfg, Config::default());
        std::fs::write(&path, "[index]\nnot_a_real_key = 1\n").unwrap();
        assert!(Config::load(Some(path.to_str().unwrap())).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
