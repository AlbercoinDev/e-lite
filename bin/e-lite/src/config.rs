use el_model::Chain;
use std::path::PathBuf;

#[derive(Debug, Clone, serde::Deserialize, Default)]
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
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct NetworkCfg {
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
pub struct CoreCfg {
    #[serde(default)]
    pub rpc_url: Option<String>,
    #[serde(default)]
    pub cookie_file: Option<String>,
    #[serde(default)]
    pub core_datadir: Option<String>,
    #[serde(default = "d_timeout")]
    pub timeout_seconds: u64,
}

impl Default for CoreCfg {
    fn default() -> Self {
        CoreCfg {
            rpc_url: None,
            cookie_file: None,
            core_datadir: None,
            timeout_seconds: d_timeout(),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct IndexCfg {
    #[serde(default = "d_mode")]
    pub mode: String,
    #[serde(default = "d_batch")]
    pub batch_blocks: u32,
    #[serde(default = "d_window")]
    pub reorg_window: u32,
    #[serde(default = "d_true")]
    pub follow: bool,
}

impl Default for IndexCfg {
    fn default() -> Self {
        IndexCfg {
            mode: d_mode(),
            batch_blocks: d_batch(),
            reorg_window: d_window(),
            follow: true,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
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

#[derive(Debug, Clone, serde::Deserialize)]
pub struct PruningCfg {
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default = "d_disk_low")]
    pub disk_low_free_gib: u64,
    #[serde(default = "d_disk_resume")]
    pub disk_resume_free_gib: u64,
    #[serde(default = "d_undo_margin")]
    pub undo_margin_blocks: u32,
    #[serde(default = "d_core_margin")]
    pub core_recent_blocks_margin: u32,
    #[serde(default = "d_min_gain")]
    pub min_gain_blocks: u32,
    #[serde(default = "d_poll")]
    pub poll_seconds: u64,
}

impl Default for PruningCfg {
    fn default() -> Self {
        PruningCfg {
            enabled: true,
            disk_low_free_gib: d_disk_low(),
            disk_resume_free_gib: d_disk_resume(),
            undo_margin_blocks: d_undo_margin(),
            core_recent_blocks_margin: d_core_margin(),
            min_gain_blocks: d_min_gain(),
            poll_seconds: d_poll(),
        }
    }
}

fn d_disk_low() -> u64 {
    5
}
fn d_disk_resume() -> u64 {
    10
}
fn d_undo_margin() -> u32 {
    288
}
fn d_core_margin() -> u32 {
    288
}
fn d_min_gain() -> u32 {
    288
}
fn d_poll() -> u64 {
    15
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RawtxCfg {
    #[serde(default = "d_cache_gib")]
    pub cache_gib: u64,
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
    #[serde(default = "d_esplora_urls")]
    pub esplora_urls: Vec<String>,
    #[serde(default)]
    pub p2p_peers: Vec<String>,
    #[serde(default = "d_p2p_timeout")]
    pub p2p_timeout_seconds: u64,
}

impl Default for RawtxCfg {
    fn default() -> Self {
        RawtxCfg {
            cache_gib: d_cache_gib(),
            decoys: d_decoys(),
            tor_socks: d_tor_socks(),
            allow_clearnet: false,
            use_p2p: true,
            use_esplora: true,
            p2p_v2: true,
            esplora_urls: d_esplora_urls(),
            p2p_peers: Vec::new(),
            p2p_timeout_seconds: d_p2p_timeout(),
        }
    }
}

fn d_cache_gib() -> u64 {
    10
}
fn d_decoys() -> u32 {
    3
}
fn d_tor_socks() -> Option<String> {
    Some("127.0.0.1:9050".to_string())
}
fn d_p2p_timeout() -> u64 {
    20
}
fn d_esplora_urls() -> Vec<String> {
    vec![
        "http://mempoolhqx4isw62xs7abwphsq7ldayuidyx2v2oethdhhj6mlo2r6ad.onion/api".to_string(),
        "https://blockstream.info/api".to_string(),
    ]
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
pub struct SafetyCfg {
    #[serde(default)]
    pub allow_mainnet: bool,
}

fn d_network() -> String {
    "regtest".to_string()
}
fn d_timeout() -> u64 {
    30
}
fn d_mode() -> String {
    "full-core".to_string()
}
fn d_batch() -> u32 {
    32
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

pub fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

impl Config {
    pub fn load(path: Option<&str>) -> Result<Config, String> {
        match path {
            Some(p) => {
                let text = std::fs::read_to_string(p)
                    .map_err(|e| format!("cannot read config {p}: {e}"))?;
                toml::from_str(&text).map_err(|e| format!("invalid config {p}: {e}"))
            }
            None => Ok(Config::default()),
        }
    }

    pub fn chain(&self) -> Result<Chain, String> {
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
}
