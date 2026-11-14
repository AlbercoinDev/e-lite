//! Privacy parameters of the external path (AUD-163).
//!
//! The values come from `privacy.toml` (a preset plus explicit overrides,
//! validated in the binary). This module only carries them; the ranges and
//! presets live in `bin/e-lite/src/privacy.rs`.

/// How a raw transaction that the node does not have is fetched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalMode {
    /// Loose transactions by txid (Electrum batches, Esplora `/tx/:txid/raw`),
    /// verified against the index, with decoy wallets. Whole blocks are the
    /// fallback.
    Tx,
    /// Whole blocks by hash only: no txid ever leaves the node.
    Block,
}

impl ExternalMode {
    pub fn name(self) -> &'static str {
        match self {
            ExternalMode::Tx => "tx",
            ExternalMode::Block => "block",
        }
    }
}

/// Effective privacy parameters.
#[derive(Clone, Debug, PartialEq)]
pub struct PrivacyParams {
    /// Preset name the values started from (`balanced`, `max`, `fast`).
    pub preset: String,
    pub mode: ExternalMode,
    /// Decoy wallets loaded alongside each real wallet (k).
    pub decoy_wallets: u32,
    /// A decoy script's history may be this fraction of the real one...
    pub decoy_size_min_ratio: f64,
    /// ...up to this multiple.
    pub decoy_size_max_ratio: f64,
    /// Decoy history must overlap the real height range widened by this.
    pub decoy_era_margin_blocks: u32,
    /// Index seeks tried per decoy script before the limits are relaxed.
    pub decoy_seek_attempts: u32,
    /// Grow decoy wallets through the outputs of their transactions.
    pub decoy_growth: bool,
    /// Txids per Electrum session before it is closed.
    pub session_size: usize,
    /// Distinct servers a group's transactions are spread over.
    pub min_servers: usize,
    /// Largest fraction of one group (real or decoy) a single server gets.
    pub max_share: f64,
    /// Sessions open at the same time.
    pub parallel_sessions: usize,
    /// How long the batcher gathers requests before sending them.
    pub batch_window_ms: u64,
    /// Decoy blocks per real block download (block mode and fallback).
    pub block_decoys: u32,
}

impl PrivacyParams {
    /// The `balanced` preset (the default).
    pub fn balanced() -> Self {
        PrivacyParams {
            preset: "balanced".into(),
            mode: ExternalMode::Tx,
            decoy_wallets: 3,
            decoy_size_min_ratio: 0.5,
            decoy_size_max_ratio: 2.0,
            decoy_era_margin_blocks: 52_560,
            decoy_seek_attempts: 64,
            decoy_growth: true,
            session_size: 50,
            min_servers: 3,
            max_share: 0.34,
            parallel_sessions: 16,
            batch_window_ms: 20,
            block_decoys: 1,
        }
    }

    /// The `max` preset: whole blocks only, random decoy blocks (no txid ever
    /// leaves the node, at the cost of weaker decoys).
    pub fn max() -> Self {
        PrivacyParams {
            preset: "max".into(),
            mode: ExternalMode::Block,
            ..Self::balanced()
        }
    }

    /// The `fast` preset: one decoy wallet, longer sessions.
    pub fn fast() -> Self {
        PrivacyParams {
            preset: "fast".into(),
            decoy_wallets: 1,
            session_size: 100,
            min_servers: 2,
            max_share: 0.5,
            ..Self::balanced()
        }
    }

    pub fn preset(name: &str) -> Option<Self> {
        match name {
            "balanced" => Some(Self::balanced()),
            "max" => Some(Self::max()),
            "fast" => Some(Self::fast()),
            _ => None,
        }
    }
}

impl Default for PrivacyParams {
    fn default() -> Self {
        Self::balanced()
    }
}
