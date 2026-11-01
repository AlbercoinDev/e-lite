//! Deterministic binary codec. All integer keys are fixed-length big-endian so
//! that lexicographic byte order equals numeric order.

use crate::types::*;
use sha2::{Digest, Sha256};
use std::fmt;

pub const SCHEMA_VERSION: u32 = 1;
pub const UNDO_MAGIC: u32 = 0x454C_5544; // "ELUD"

// Exact byte sizes of one undo entry per section. Keep writer and reader in
// sync through these constants (a mismatch here previously made valid records
// unreadable).
pub const UNDO_SPENT_LEN: usize = 32 + 4 + 8 + 32 + 4 + 1; // 81
pub const UNDO_CREATED_LEN: usize = 32 + 4 + 32; // 68
pub const UNDO_HISTORY_LEN: usize = 32 + 4 + 4; // 40
pub const UNDO_TXID_LEN: usize = 32;
/// magic + schema + height + block_hash + four counts + checksum
pub const UNDO_MIN_LEN: usize = 4 + 4 + 4 + 32 + 4 * 4 + 32; // 92

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecError(pub String);

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "codec error: {}", self.0)
    }
}

impl std::error::Error for CodecError {}

impl From<String> for CodecError {
    fn from(s: String) -> Self {
        CodecError(s)
    }
}

pub type Result<T> = std::result::Result<T, CodecError>;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(CodecError(msg.into()))
}

// ------------------------------------------------------------------ primitives

pub fn u32be(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

pub fn u64be(v: u64) -> [u8; 8] {
    v.to_be_bytes()
}

fn get_u32(buf: &[u8], off: usize) -> Result<u32> {
    if off + 4 > buf.len() {
        return err("truncated u32");
    }
    let mut a = [0u8; 4];
    a.copy_from_slice(&buf[off..off + 4]);
    Ok(u32::from_be_bytes(a))
}

fn get_u64(buf: &[u8], off: usize) -> Result<u64> {
    if off + 8 > buf.len() {
        return err("truncated u64");
    }
    let mut a = [0u8; 8];
    a.copy_from_slice(&buf[off..off + 8]);
    Ok(u64::from_be_bytes(a))
}

fn get_hash(buf: &[u8], off: usize) -> Result<Hash32> {
    if off + 32 > buf.len() {
        return err("truncated hash");
    }
    Hash32::from_slice(&buf[off..off + 32]).map_err(CodecError)
}

// ---------------------------------------------------------------------- blocks

pub fn block_key(height: u32) -> [u8; 4] {
    u32be(height)
}

/// 32 hash + 80 header + 4 tx_count + 8 txnum_base
pub const BLOCK_VALUE_LEN: usize = 32 + HEADER_LEN + 4 + 8;

pub fn block_value(rec: &BlockRecord) -> [u8; BLOCK_VALUE_LEN] {
    let mut out = [0u8; BLOCK_VALUE_LEN];
    out[0..32].copy_from_slice(rec.hash.as_bytes());
    out[32..112].copy_from_slice(&rec.header);
    out[112..116].copy_from_slice(&rec.tx_count.to_be_bytes());
    out[116..124].copy_from_slice(&rec.txnum_base.to_be_bytes());
    out
}

pub fn parse_block_value(height: u32, buf: &[u8]) -> Result<BlockRecord> {
    if buf.len() != BLOCK_VALUE_LEN {
        return err(format!(
            "block value must be {BLOCK_VALUE_LEN} bytes, got {}",
            buf.len()
        ));
    }
    let mut header = [0u8; HEADER_LEN];
    header.copy_from_slice(&buf[32..112]);
    Ok(BlockRecord {
        height,
        hash: Hash32::from_slice(&buf[0..32]).map_err(CodecError)?,
        header,
        tx_count: get_u32(buf, 112)?,
        txnum_base: get_u64(buf, 116)?,
    })
}

// --------------------------------------------------------------------- history

/// script(32) + height(4) + tx_pos(4)
pub fn history_key(script: ScriptHash, height: u32, tx_pos: u32) -> [u8; 40] {
    let mut out = [0u8; 40];
    out[0..32].copy_from_slice(script.0.as_bytes());
    out[32..36].copy_from_slice(&height.to_be_bytes());
    out[36..40].copy_from_slice(&tx_pos.to_be_bytes());
    out
}

pub fn parse_history_key(buf: &[u8]) -> Result<(ScriptHash, u32, u32)> {
    if buf.len() != 40 {
        return err("history key must be 40 bytes");
    }
    Ok((
        ScriptHash(get_hash(buf, 0)?),
        get_u32(buf, 32)?,
        get_u32(buf, 36)?,
    ))
}

/// received(8) + spent(8) + flags(2)
pub const HISTORY_VALUE_LEN: usize = 18;

pub fn history_value(e: &HistoryEffect) -> [u8; HISTORY_VALUE_LEN] {
    let mut out = [0u8; HISTORY_VALUE_LEN];
    out[0..8].copy_from_slice(&e.received_sat.to_be_bytes());
    out[8..16].copy_from_slice(&e.spent_sat.to_be_bytes());
    out[16..18].copy_from_slice(&e.flags.to_be_bytes());
    out
}

pub fn parse_history_value(buf: &[u8]) -> Result<HistoryEffect> {
    if buf.len() != HISTORY_VALUE_LEN {
        return err("history value must be 18 bytes");
    }
    Ok(HistoryEffect {
        received_sat: get_u64(buf, 0)?,
        spent_sat: get_u64(buf, 8)?,
        flags: u16::from_be_bytes([buf[16], buf[17]]),
    })
}

// ------------------------------------------------------------------------ utxo

/// txid(32) + vout(4)
pub fn utxo_key(op: &Outpoint) -> [u8; 36] {
    let mut out = [0u8; 36];
    out[0..32].copy_from_slice(op.txid.as_bytes());
    out[32..36].copy_from_slice(&op.vout.to_be_bytes());
    out
}

pub fn parse_outpoint(buf: &[u8]) -> Result<Outpoint> {
    if buf.len() != 36 {
        return err("outpoint must be 36 bytes");
    }
    Ok(Outpoint {
        txid: get_hash(buf, 0)?,
        vout: get_u32(buf, 32)?,
    })
}

/// value(8) + script(32) + creation_height(4) + coinbase(1)
pub const UTXO_VALUE_LEN: usize = 45;

pub fn utxo_value(v: &UtxoValue) -> [u8; UTXO_VALUE_LEN] {
    let mut out = [0u8; UTXO_VALUE_LEN];
    out[0..8].copy_from_slice(&v.value_sat.to_be_bytes());
    out[8..40].copy_from_slice(v.script_hash.0.as_bytes());
    out[40..44].copy_from_slice(&v.creation_height.to_be_bytes());
    out[44] = u8::from(v.coinbase);
    out
}

pub fn parse_utxo_value(buf: &[u8]) -> Result<UtxoValue> {
    if buf.len() != UTXO_VALUE_LEN {
        return err("utxo value must be 45 bytes");
    }
    Ok(UtxoValue {
        value_sat: get_u64(buf, 0)?,
        script_hash: ScriptHash(get_hash(buf, 8)?),
        creation_height: get_u32(buf, 40)?,
        coinbase: buf[44] != 0,
    })
}

/// script(32) + txid(32) + vout(4)
pub fn script_utxo_key(script: ScriptHash, op: &Outpoint) -> [u8; 68] {
    let mut out = [0u8; 68];
    out[0..32].copy_from_slice(script.0.as_bytes());
    out[32..64].copy_from_slice(op.txid.as_bytes());
    out[64..68].copy_from_slice(&op.vout.to_be_bytes());
    out
}

pub fn parse_script_utxo_key(buf: &[u8]) -> Result<(ScriptHash, Outpoint)> {
    if buf.len() != 68 {
        return err("script utxo key must be 68 bytes");
    }
    Ok((
        ScriptHash(get_hash(buf, 0)?),
        Outpoint {
            txid: get_hash(buf, 32)?,
            vout: get_u32(buf, 64)?,
        },
    ))
}

/// value(8) + height(4) + coinbase(1)
pub const SCRIPT_UTXO_VALUE_LEN: usize = 13;

pub fn script_utxo_value(v: &UtxoValue) -> [u8; SCRIPT_UTXO_VALUE_LEN] {
    let mut out = [0u8; SCRIPT_UTXO_VALUE_LEN];
    out[0..8].copy_from_slice(&v.value_sat.to_be_bytes());
    out[8..12].copy_from_slice(&v.creation_height.to_be_bytes());
    out[12] = u8::from(v.coinbase);
    out
}

pub fn parse_script_utxo_value(buf: &[u8]) -> Result<(u64, u32, bool)> {
    if buf.len() != SCRIPT_UTXO_VALUE_LEN {
        return err("script utxo value must be 13 bytes");
    }
    Ok((get_u64(buf, 0)?, get_u32(buf, 8)?, buf[12] != 0))
}

// ----------------------------------------------------------------- occurrences

/// txid(32) + height(4) + tx_pos(4)
pub fn occurrence_key(txid: Hash32, height: u32, tx_pos: u32) -> [u8; 40] {
    let mut out = [0u8; 40];
    out[0..32].copy_from_slice(txid.as_bytes());
    out[32..36].copy_from_slice(&height.to_be_bytes());
    out[36..40].copy_from_slice(&tx_pos.to_be_bytes());
    out
}

pub fn parse_occurrence_key(buf: &[u8]) -> Result<(Hash32, u32, u32)> {
    if buf.len() != 40 {
        return err("occurrence key must be 40 bytes");
    }
    Ok((get_hash(buf, 0)?, get_u32(buf, 32)?, get_u32(buf, 36)?))
}

// ------------------------------------------------------------------------- tip

/// height(4) + block_hash(32) + schema(4) + txnum(8)
pub const TIP_VALUE_LEN: usize = 48;

pub fn tip_value(t: &IndexedTip) -> [u8; TIP_VALUE_LEN] {
    let mut out = [0u8; TIP_VALUE_LEN];
    out[0..4].copy_from_slice(&t.height.to_be_bytes());
    out[4..36].copy_from_slice(t.block_hash.as_bytes());
    out[36..40].copy_from_slice(&t.schema_version.to_be_bytes());
    out[40..48].copy_from_slice(&t.txnum.to_be_bytes());
    out
}

pub fn parse_tip_value(buf: &[u8]) -> Result<IndexedTip> {
    if buf.len() != TIP_VALUE_LEN {
        return err("tip value must be 48 bytes");
    }
    Ok(IndexedTip {
        height: get_u32(buf, 0)?,
        block_hash: get_hash(buf, 4)?,
        schema_version: get_u32(buf, 36)?,
        txnum: get_u64(buf, 40)?,
    })
}

// ------------------------------------------------------------------------ undo

fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_be_bytes());
}

pub fn undo_value(r: &UndoRecord) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        16 + 32
            + (r.spent.len() * UNDO_SPENT_LEN)
            + (r.created.len() * UNDO_CREATED_LEN)
            + (r.history.len() * UNDO_HISTORY_LEN)
            + (r.txids.len() * UNDO_TXID_LEN)
            + 16,
    );
    push_u32(&mut out, UNDO_MAGIC);
    push_u32(&mut out, SCHEMA_VERSION);
    push_u32(&mut out, r.height);
    out.extend_from_slice(r.block_hash.as_bytes());

    push_u32(&mut out, r.spent.len() as u32);
    for s in &r.spent {
        out.extend_from_slice(s.outpoint.txid.as_bytes());
        push_u32(&mut out, s.outpoint.vout);
        push_u64(&mut out, s.utxo.value_sat);
        out.extend_from_slice(s.utxo.script_hash.0.as_bytes());
        push_u32(&mut out, s.utxo.creation_height);
        out.push(u8::from(s.utxo.coinbase));
    }

    push_u32(&mut out, r.created.len() as u32);
    for c in &r.created {
        out.extend_from_slice(c.outpoint.txid.as_bytes());
        push_u32(&mut out, c.outpoint.vout);
        out.extend_from_slice(c.script_hash.0.as_bytes());
    }

    push_u32(&mut out, r.history.len() as u32);
    for h in &r.history {
        out.extend_from_slice(h.script_hash.0.as_bytes());
        push_u32(&mut out, h.height);
        push_u32(&mut out, h.tx_pos);
    }

    push_u32(&mut out, r.txids.len() as u32);
    for t in &r.txids {
        out.extend_from_slice(t.as_bytes());
    }

    let checksum = Sha256::digest(&out);
    out.extend_from_slice(&checksum);
    out
}

pub fn parse_undo_value(buf: &[u8]) -> Result<UndoRecord> {
    if buf.len() < UNDO_MIN_LEN {
        return err("undo record too short");
    }
    let body_len = buf.len() - 32;
    let checksum = Sha256::digest(&buf[..body_len]);
    if checksum.as_slice() != &buf[body_len..] {
        return err("undo checksum mismatch");
    }
    let body = &buf[..body_len];

    let magic = get_u32(body, 0)?;
    if magic != UNDO_MAGIC {
        return err("bad undo magic");
    }
    let schema = get_u32(body, 4)?;
    if schema > SCHEMA_VERSION {
        return err(format!(
            "undo schema {schema} is newer than {SCHEMA_VERSION}"
        ));
    }
    let height = get_u32(body, 8)?;
    let block_hash = get_hash(body, 12)?;
    let mut off = 44usize;

    let spent_n = get_u32(body, off)? as usize;
    off += 4;
    if spent_n
        .checked_mul(UNDO_SPENT_LEN)
        .is_none_or(|n| n > body.len() - off)
    {
        return err("impossible spent count");
    }
    let mut spent = Vec::with_capacity(spent_n);
    for _ in 0..spent_n {
        let outpoint = Outpoint {
            txid: get_hash(body, off)?,
            vout: get_u32(body, off + 32)?,
        };
        off += 36;
        let value_sat = get_u64(body, off)?;
        off += 8;
        let script_hash = ScriptHash(get_hash(body, off)?);
        off += 32;
        let creation_height = get_u32(body, off)?;
        off += 4;
        let coinbase = body[off] != 0;
        off += 1;
        spent.push(UndoSpent {
            outpoint,
            utxo: UtxoValue {
                value_sat,
                script_hash,
                creation_height,
                coinbase,
            },
        });
    }

    let created_n = get_u32(body, off)? as usize;
    off += 4;
    if created_n
        .checked_mul(UNDO_CREATED_LEN)
        .is_none_or(|n| n > body.len() - off)
    {
        return err("impossible created count");
    }
    let mut created = Vec::with_capacity(created_n);
    for _ in 0..created_n {
        let outpoint = Outpoint {
            txid: get_hash(body, off)?,
            vout: get_u32(body, off + 32)?,
        };
        off += 36;
        let script_hash = ScriptHash(get_hash(body, off)?);
        off += 32;
        created.push(UndoCreated {
            outpoint,
            script_hash,
        });
    }

    let hist_n = get_u32(body, off)? as usize;
    off += 4;
    if hist_n
        .checked_mul(UNDO_HISTORY_LEN)
        .is_none_or(|n| n > body.len() - off)
    {
        return err("impossible history count");
    }
    let mut history = Vec::with_capacity(hist_n);
    for _ in 0..hist_n {
        let script_hash = ScriptHash(get_hash(body, off)?);
        let h = get_u32(body, off + 32)?;
        let p = get_u32(body, off + 36)?;
        off += 40;
        history.push(UndoHistory {
            script_hash,
            height: h,
            tx_pos: p,
        });
    }

    let tx_n = get_u32(body, off)? as usize;
    off += 4;
    if tx_n
        .checked_mul(UNDO_TXID_LEN)
        .is_none_or(|n| n > body.len() - off)
    {
        return err("impossible txid count");
    }
    let mut txids = Vec::with_capacity(tx_n);
    for _ in 0..tx_n {
        txids.push(get_hash(body, off)?);
        off += 32;
    }
    if off != body.len() {
        return err("trailing bytes in undo record");
    }

    Ok(UndoRecord {
        height,
        block_hash,
        spent,
        created,
        history,
        txids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_undo() -> UndoRecord {
        UndoRecord {
            height: 42,
            block_hash: Hash32::from_hex(&"11".repeat(32)).unwrap(),
            spent: vec![UndoSpent {
                outpoint: Outpoint {
                    txid: Hash32::from_hex(&"22".repeat(32)).unwrap(),
                    vout: 3,
                },
                utxo: UtxoValue {
                    value_sat: 1234,
                    script_hash: ScriptHash(Hash32::from_hex(&"33".repeat(32)).unwrap()),
                    creation_height: 7,
                    coinbase: true,
                },
            }],
            created: vec![UndoCreated {
                outpoint: Outpoint {
                    txid: Hash32::from_hex(&"44".repeat(32)).unwrap(),
                    vout: 0,
                },
                script_hash: ScriptHash(Hash32::from_hex(&"55".repeat(32)).unwrap()),
            }],
            history: vec![UndoHistory {
                script_hash: ScriptHash(Hash32::from_hex(&"66".repeat(32)).unwrap()),
                height: 42,
                tx_pos: 1,
            }],
            txids: vec![Hash32::from_hex(&"77".repeat(32)).unwrap()],
        }
    }

    #[test]
    fn undo_roundtrip_and_checksum() {
        let r = sample_undo();
        let enc = undo_value(&r);
        assert_eq!(parse_undo_value(&enc).unwrap(), r);

        let mut bad = enc.clone();
        bad[50] ^= 0xff;
        assert!(parse_undo_value(&bad).is_err());
    }

    #[test]
    fn undo_minimal_roundtrip() {
        let r = UndoRecord {
            height: 1,
            block_hash: Hash32::from_hex(&"aa".repeat(32)).unwrap(),
            ..Default::default()
        };
        let enc = undo_value(&r);
        assert_eq!(enc.len(), UNDO_MIN_LEN);
        assert_eq!(parse_undo_value(&enc).unwrap(), r);
        // One byte less must be rejected, not panic.
        assert!(parse_undo_value(&enc[..UNDO_MIN_LEN - 1]).is_err());
    }

    /// Regression: a block with many spent inputs and almost no other sections.
    /// The reader used to require 85 bytes per spent entry (the encoder writes
    /// 81), so a valid record failed with "impossible spent count".
    #[test]
    fn undo_spent_heavy_roundtrip() {
        let mut spent = Vec::new();
        for i in 0..1000u32 {
            let mut txid = [0u8; 32];
            txid[0..4].copy_from_slice(&i.to_be_bytes());
            spent.push(UndoSpent {
                outpoint: Outpoint {
                    txid: Hash32(txid),
                    vout: i,
                },
                utxo: UtxoValue {
                    value_sat: 1_000 + i as u64,
                    script_hash: ScriptHash(Hash32([0x11; 32])),
                    creation_height: i,
                    coinbase: i % 2 == 0,
                },
            });
        }
        let r = UndoRecord {
            height: 7,
            block_hash: Hash32([0x22; 32]),
            spent,
            created: Vec::new(),
            history: Vec::new(),
            txids: vec![Hash32([0x33; 32])],
        };
        let enc = undo_value(&r);
        assert_eq!(
            enc.len(),
            UNDO_MIN_LEN + 1000 * UNDO_SPENT_LEN + UNDO_TXID_LEN
        );
        assert_eq!(parse_undo_value(&enc).unwrap(), r);
    }

    #[test]
    fn keys_are_order_preserving() {
        let a = history_key(ScriptHash::default(), 1, 1);
        let b = history_key(ScriptHash::default(), 1, 2);
        let c = history_key(ScriptHash::default(), 2, 0);
        assert!(a < b && b < c);

        let u1 = utxo_key(&Outpoint {
            txid: Hash32::default(),
            vout: 1,
        });
        let u2 = utxo_key(&Outpoint {
            txid: Hash32::default(),
            vout: 2,
        });
        assert!(u1 < u2);
    }

    #[test]
    fn block_and_tip_roundtrip() {
        let rec = BlockRecord {
            height: 5,
            hash: Hash32::from_hex(&"aa".repeat(32)).unwrap(),
            header: [7u8; HEADER_LEN],
            tx_count: 9,
            txnum_base: 1234,
        };
        assert_eq!(parse_block_value(5, &block_value(&rec)).unwrap(), rec);

        let tip = IndexedTip {
            height: 5,
            block_hash: rec.hash,
            schema_version: SCHEMA_VERSION,
            txnum: 1243,
        };
        assert_eq!(parse_tip_value(&tip_value(&tip)).unwrap(), tip);
    }

    #[test]
    fn rejects_truncated_and_foreign_values() {
        assert!(parse_block_value(0, &[0u8; 10]).is_err());
        assert!(parse_history_value(&[0u8; 17]).is_err());
        assert!(parse_utxo_value(&[0u8; 44]).is_err());
        assert!(parse_tip_value(&[0u8; 47]).is_err());
        // version newer than ours
        let mut tip = tip_value(&IndexedTip {
            height: 1,
            block_hash: Hash32::default(),
            schema_version: SCHEMA_VERSION + 1,
            txnum: 1,
        });
        assert!(parse_tip_value(&tip).is_ok());
        tip[36..40].copy_from_slice(&(SCHEMA_VERSION + 5).to_be_bytes());
        // value-level check happens in Store::open
        assert_eq!(
            parse_tip_value(&tip).unwrap().schema_version,
            SCHEMA_VERSION + 5
        );
    }
}
