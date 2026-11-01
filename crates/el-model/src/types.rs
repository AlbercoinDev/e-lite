use std::fmt;
use std::str::FromStr;

pub const HASH_LEN: usize = 32;
pub const HEADER_LEN: usize = 80;

/// A 32-byte hash stored in **display (big-endian hex) order**.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Hash32(pub [u8; HASH_LEN]);

impl Hash32 {
    pub const ZERO: Hash32 = Hash32([0u8; HASH_LEN]);

    pub fn from_slice(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() != HASH_LEN {
            return Err(format!("hash needs {HASH_LEN} bytes, got {}", bytes.len()));
        }
        let mut out = [0u8; HASH_LEN];
        out.copy_from_slice(bytes);
        Ok(Hash32(out))
    }

    pub fn from_hex(s: &str) -> Result<Self, String> {
        let raw = hex::decode(s).map_err(|e| format!("invalid hex: {e}"))?;
        Self::from_slice(&raw)
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn as_bytes(&self) -> &[u8; HASH_LEN] {
        &self.0
    }

    /// Byte order used on the wire / by consensus hashing.
    pub fn reversed(&self) -> Hash32 {
        let mut out = [0u8; HASH_LEN];
        for (i, b) in self.0.iter().rev().enumerate() {
            out[i] = *b;
        }
        Hash32(out)
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; HASH_LEN]
    }
}

impl fmt::Debug for Hash32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl fmt::Display for Hash32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl FromStr for Hash32 {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex(s)
    }
}

/// A sha256 over a raw scriptPubKey. Canonical order (not reversed).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct ScriptHash(pub Hash32);

impl ScriptHash {
    pub fn from_script(script: &[u8]) -> Self {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(script);
        let mut out = [0u8; HASH_LEN];
        out.copy_from_slice(&h.finalize());
        ScriptHash(Hash32(out))
    }

    /// Electrum 1.4 exposes scripthashes reversed.
    pub fn to_electrum_hex(&self) -> String {
        self.0.reversed().to_hex()
    }

    pub fn from_electrum_hex(s: &str) -> Result<Self, String> {
        Ok(ScriptHash(Hash32::from_hex(s)?.reversed()))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub struct Outpoint {
    pub txid: Hash32,
    pub vout: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UtxoValue {
    pub value_sat: u64,
    pub script_hash: ScriptHash,
    pub creation_height: u32,
    pub coinbase: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BlockPosition {
    pub height: u32,
    pub tx_pos: u32,
}

/// Occurrence id. Encodes `(height, tx_pos)` so it is derivable and never
/// allocated separately. 40 bits of height + 24 bits of position.
pub const TX_POS_BITS: u32 = 24;
pub const TX_POS_MASK: u64 = (1u64 << TX_POS_BITS) - 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub struct TxRef(u64);

impl TxRef {
    pub fn try_new(height: u32, tx_pos: u32) -> Result<Self, String> {
        if (tx_pos as u64) > TX_POS_MASK {
            return Err(format!("tx_pos {tx_pos} does not fit in 24 bits"));
        }
        Ok(TxRef(((height as u64) << TX_POS_BITS) | tx_pos as u64))
    }

    pub fn from_parts_unchecked(height: u32, tx_pos: u32) -> Self {
        TxRef(((height as u64) << TX_POS_BITS) | (tx_pos as u64 & TX_POS_MASK))
    }

    pub fn height(self) -> u32 {
        (self.0 >> TX_POS_BITS) as u32
    }

    pub fn tx_pos(self) -> u32 {
        (self.0 & TX_POS_MASK) as u32
    }

    pub fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IndexedTip {
    pub height: u32,
    pub block_hash: Hash32,
    pub schema_version: u32,
    /// Total number of indexed transaction occurrences (offset into txids.dat).
    pub txnum: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct HistoryEffect {
    pub received_sat: u64,
    pub spent_sat: u64,
    pub flags: u16,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BlockRecord {
    pub height: u32,
    pub hash: Hash32,
    pub header: [u8; HEADER_LEN],
    pub tx_count: u32,
    pub txnum_base: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub enum Chain {
    Mainnet,
    Testnet4,
    Signet,
    Regtest,
}

impl Chain {
    pub fn name(self) -> &'static str {
        match self {
            Chain::Mainnet => "mainnet",
            Chain::Testnet4 => "testnet4",
            Chain::Signet => "signet",
            Chain::Regtest => "regtest",
        }
    }

    pub fn from_name(name: &str) -> Option<Chain> {
        match name {
            "mainnet" | "bitcoin" => Some(Chain::Mainnet),
            "testnet4" => Some(Chain::Testnet4),
            "signet" => Some(Chain::Signet),
            "regtest" => Some(Chain::Regtest),
            _ => None,
        }
    }

    /// Core's chain directory name inside the datadir.
    pub fn core_subdir(self) -> &'static str {
        match self {
            Chain::Mainnet => "",
            Chain::Testnet4 => "testnet4",
            Chain::Signet => "signet",
            Chain::Regtest => "regtest",
        }
    }

    pub fn default_rpc_port(self) -> u16 {
        match self {
            Chain::Mainnet => 8332,
            Chain::Testnet4 => 48332,
            Chain::Signet => 38332,
            Chain::Regtest => 18443,
        }
    }

    pub fn default_electrum_port(self) -> u16 {
        50001
    }

    /// Known genesis hash, when we can pin it without trusting the node.
    pub fn known_genesis(self) -> Option<Hash32> {
        let hex = match self {
            Chain::Mainnet => "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f",
            Chain::Testnet4 => "00000000da84f2bafbbc53dee25a72ae507ff4914b867c565be350b0da8bf043",
            Chain::Regtest => "0f9188f13cb7b2c71f2a335e3a4fc328bf5beb436012afca590b1a11466e2206",
            // Signet has a configurable challenge; the genesis is pinned at init
            // from `getblockhash 0` and re-checked on every open.
            Chain::Signet => return None,
        };
        Hash32::from_hex(hex).ok()
    }

    pub fn requires_explicit_opt_in(self) -> bool {
        matches!(self, Chain::Mainnet)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NetworkIdentity {
    pub chain: Chain,
    pub genesis: Hash32,
    pub signet_challenge: Option<Hash32>,
}

impl NetworkIdentity {
    pub fn new(chain: Chain, genesis: Hash32) -> Self {
        NetworkIdentity {
            chain,
            genesis,
            signet_challenge: None,
        }
    }

    pub fn check_known_genesis(&self) -> Result<(), String> {
        if let Some(known) = self.chain.known_genesis() {
            if known != self.genesis {
                return Err(format!(
                    "genesis mismatch for {}: expected {}, core says {}",
                    self.chain.name(),
                    known,
                    self.genesis
                ));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- undo records

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UndoSpent {
    pub outpoint: Outpoint,
    pub utxo: UtxoValue,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UndoCreated {
    pub outpoint: Outpoint,
    pub script_hash: ScriptHash,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UndoHistory {
    pub script_hash: ScriptHash,
    pub height: u32,
    pub tx_pos: u32,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct UndoRecord {
    pub height: u32,
    pub block_hash: Hash32,
    pub spent: Vec<UndoSpent>,
    pub created: Vec<UndoCreated>,
    pub history: Vec<UndoHistory>,
    pub txids: Vec<Hash32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txref_roundtrip() {
        for (h, p) in [
            (0u32, 0u32),
            (1, 1),
            (917_000, 4242),
            (0x00ff_ffff, 0x00ff_ffff),
        ] {
            let r = TxRef::try_new(h, p).unwrap();
            assert_eq!(r.height(), h);
            assert_eq!(r.tx_pos(), p);
        }
        assert!(TxRef::try_new(1, 1 << 24).is_err());
    }

    #[test]
    fn hash_reverse_is_involution() {
        let h =
            Hash32::from_hex("000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f")
                .unwrap();
        assert_eq!(h.reversed().reversed(), h);
        assert_eq!(h.reversed().to_hex().len(), 64);
    }

    #[test]
    fn scripthash_electrum_is_reversed_hex() {
        let sh = ScriptHash::from_script(b"\x51");
        let shown = sh.to_electrum_hex();
        assert_eq!(ScriptHash::from_electrum_hex(&shown).unwrap(), sh);
        assert_eq!(shown, sh.0.reversed().to_hex());
    }

    #[test]
    fn known_genesis_checks() {
        let bad = NetworkIdentity::new(Chain::Regtest, Hash32::ZERO);
        assert!(bad.check_known_genesis().is_err());
        let g = Chain::Regtest
            .known_genesis()
            .expect("regtest genesis is pinned");
        let good = NetworkIdentity::new(Chain::Regtest, g);
        assert!(good.check_known_genesis().is_ok());
        // Signet has no hardcoded genesis: it is pinned from Core at init.
        assert!(Chain::Signet.known_genesis().is_none());
    }
}
