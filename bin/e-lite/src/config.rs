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
    /// Core prune target in MiB. 550 is Core's minimum (maximum pruning);
    /// 0 means a full node. Only applied when e-lite launches Core.
    #[serde(default = "d_prune_mib")]
    pub prune_mib: u64,
    /// Extra arguments passed to bitcoind when e-lite launches it.
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// Restart a Core process launched by e-lite if it exits unexpectedly.
    #[serde(default = "d_true")]
    pub restart: bool,
    /// Stop a Core process launched by e-lite when e-lite stops.
    #[serde(default = "d_true")]
    pub stop_on_exit: bool,
    /// Log file for a Core process launched by e-lite.
    #[serde(default)]
    pub log_file: Option<String>,
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
            restart: true,
            stop_on_exit: true,
            log_file: None,
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
            follow: true,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerCfg {
    #[serde(default = "d_listen")]
    pub listen: String,
    #[serde(default = "d_clients")]
    pub max_clients: usize,
}

impl Default for ServerCfg {
    fn default() -> Self {
        ServerCfg {
            listen: d_listen(),
            max_clients: d_clients(),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsCfg {
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default = "d_metrics")]
    pub listen: String,
}

impl Default for MetricsCfg {
    fn default() -> Self {
        MetricsCfg {
            enabled: true,
            listen: d_metrics(),
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
pub struct PruningCfg {
    /// Pause Core downloads and the indexer when free space is below this.
    #[serde(default = "d_min_free")]
    pub min_free_gib: u64,
    /// Pause Core when the index lags more than this. The effective value is
    /// capped by Core's own retention window (`blocks - pruneheight`).
    /// 0 disables the guard.
    #[serde(default = "d_max_lag")]
    pub max_core_lag_blocks: u32,
    #[serde(default = "d_poll")]
    pub poll_seconds: u64,
}

impl Default for PruningCfg {
    fn default() -> Self {
        PruningCfg {
            min_free_gib: d_min_free(),
            max_core_lag_blocks: d_max_lag(),
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
    550
}
fn d_batch() -> u32 {
    256
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
fn d_listen() -> String {
    "127.0.0.1:50001".to_string()
}
fn d_clients() -> usize {
    32
}
fn d_metrics() -> String {
    "127.0.0.1:9184".to_string()
}
fn d_min_free() -> u64 {
    10
}
fn d_max_lag() -> u32 {
    256
}
fn d_poll() -> u64 {
    10
}
fn d_cache_mib() -> u64 {
    2048
}
fn d_decoys() -> u32 {
    3
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
    pub fn load(path: Option<&str>) -> Result<Config, String> {
        let path = match path {
            Some(p) => Some(PathBuf::from(p)),
            None => default_config_paths().into_iter().find(|p| p.is_file()),
        };
        match path {
            Some(p) => {
                let text = std::fs::read_to_string(&p)
                    .map_err(|e| format!("cannot read config {}: {e}", p.display()))?;
                toml::from_str(&text).map_err(|e| format!("invalid config {}: {e}", p.display()))
            }
            None => Ok(Config::default()),
        }
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

    pub fn data_dir(&self, chain: Chain) -> PathBuf {
        match &self.network.data_dir {
            Some(d) => PathBuf::from(d),
            None => home().join(".local/share/e-lite").join(chain.name()),
        }
    }

    pub fn rpc_url(&self, chain: Chain) -> String {
        self.core
            .rpc_url
            .clone()
            .unwrap_or_else(|| format!("http://127.0.0.1:{}", chain.default_rpc_port()))
    }

    pub fn core_datadir(&self) -> PathBuf {
        match &self.core.core_datadir {
            Some(d) => PathBuf::from(d),
            None => home().join(".bitcoin"),
        }
    }

    pub fn cookie_file(&self) -> Option<PathBuf> {
        self.core.cookie_file.as_ref().map(PathBuf::from)
    }

    /// Core's blocks directory: explicit override, or `<datadir>/<sub>/blocks`.
    pub fn blocks_dir(&self, chain: Chain) -> PathBuf {
        match &self.core.blocks_dir {
            Some(d) => PathBuf::from(d),
            None => el_blocks::default_blocks_dir(&self.core_datadir(), chain),
        }
    }

    pub fn log_file(&self, chain: Chain) -> PathBuf {
        match &self.core.log_file {
            Some(p) => PathBuf::from(p),
            None => self.data_dir(chain).join("bitcoind.log"),
        }
    }

    pub fn pid_file(&self, chain: Chain) -> PathBuf {
        self.data_dir(chain).join("core-child.pid")
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.index.batch_blocks == 0 {
            return Err("[index] batch_blocks must be >= 1".to_string());
        }
        if self.index.reorg_window == 0 {
            return Err("[index] reorg_window must be >= 1".to_string());
        }
        if self.server.max_clients == 0 {
            return Err("[server] max_clients must be >= 1".to_string());
        }
        if self.pruning.min_free_gib == 0 {
            return Err("[pruning] min_free_gib must be >= 1".to_string());
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
            let core = self.core_datadir();
            if data == core || data.starts_with(&core) {
                return Err(format!(
                    "[network] data_dir {} must not be inside the Core datadir {}",
                    data.display(),
                    core.display()
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_network_agnostic() {
        let cfg = Config::default();
        assert_eq!(cfg.network.name, "auto");
        assert_eq!(cfg.core.manage, CoreManage::Auto);
        assert_eq!(cfg.core.prune_mib, 550);
        assert_eq!(cfg.index.reorg_window, 288);
        assert_eq!(cfg.pruning.max_core_lag_blocks, 256);
        assert_eq!(cfg.rawtx.cache_mib, 2048);
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
    fn accepts_data_dir_next_to_core() {
        let mut cfg = Config::default();
        cfg.core.core_datadir = Some("/srv/bitcoin".to_string());
        cfg.network.data_dir = Some("/srv/e-lite".to_string());
        assert!(cfg.validate().is_ok());
    }
}
