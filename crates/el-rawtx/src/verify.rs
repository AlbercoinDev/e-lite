//! Cryptographic verification of externally fetched blocks.
//!
//! A source cannot forge data: the block hash and header must match the chain
//! we indexed, the merkle root must commit to the transactions, and the witness
//! commitment must commit to the witnesses (a TXID alone does not).

use crate::{RawtxError, Result};
use bitcoin::{Block, Transaction};
use el_model::{merkle_root, Hash32};
use sha2::{Digest, Sha256};

fn sha256d(a: &[u8], b: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(a);
    h.update(b);
    let first = h.finalize();
    let mut h2 = Sha256::new();
    h2.update(first);
    let mut out = [0u8; 32];
    out.copy_from_slice(&h2.finalize());
    out
}

/// Decode and verify a block against the indexed header.
pub fn verify_block(
    bytes: &[u8],
    expected_hash: &Hash32,
    expected_header: &[u8; 80],
) -> Result<Block> {
    let block: Block = bitcoin::consensus::deserialize(bytes)
        .map_err(|e| RawtxError::Verify(format!("cannot decode block: {e}")))?;

    let hash = Hash32::from_hex(&block.block_hash().to_string())
        .map_err(|e| RawtxError::Verify(format!("bad block hash: {e}")))?;
    if hash != *expected_hash {
        return Err(RawtxError::Verify(format!(
            "block hash mismatch: got {hash}, expected {expected_hash}"
        )));
    }

    let header = bitcoin::consensus::serialize(&block.header);
    if header.as_slice() != expected_header {
        return Err(RawtxError::Verify("header mismatch".into()));
    }

    match block.compute_merkle_root() {
        Some(root) if root == block.header.merkle_root => {}
        _ => return Err(RawtxError::Verify("merkle root mismatch".into())),
    }
    Ok(block)
}

/// Decode and verify a block when only its expected hash is known (for
/// example a block below Core's prune height). The hash commits to the header,
/// so checking the hash, merkle root and (by the caller) witness commitment is
/// sufficient to authenticate the bytes against Core's active chain.
pub fn verify_block_by_hash(bytes: &[u8], expected_hash: &Hash32) -> Result<Block> {
    let block: Block = bitcoin::consensus::deserialize(bytes)
        .map_err(|e| RawtxError::Verify(format!("cannot decode block: {e}")))?;
    let hash = Hash32::from_hex(&block.block_hash().to_string())
        .map_err(|e| RawtxError::Verify(format!("bad block hash: {e}")))?;
    if hash != *expected_hash {
        return Err(RawtxError::Verify(format!(
            "block hash mismatch: got {hash}, expected {expected_hash}"
        )));
    }
    match block.compute_merkle_root() {
        Some(root) if root == block.header.merkle_root => {}
        _ => return Err(RawtxError::Verify("merkle root mismatch".into())),
    }
    Ok(block)
}

/// BIP141 witness commitment check. Required for every served transaction: the
/// TXID commits to the non-witness data only.
pub fn verify_witness_commitment(block: &Block) -> Result<()> {
    let commitment = block.txdata.first().and_then(|cb| {
        cb.output.iter().find_map(|out| {
            let s = out.script_pubkey.as_bytes();
            if s.len() >= 38 && s[0] == 0x6a && s[1] == 0x24 && s[2..6] == [0xaa, 0x21, 0xa9, 0xed]
            {
                Some(s[6..38].to_vec())
            } else {
                None
            }
        })
    });

    let has_witness = block
        .txdata
        .iter()
        .any(|tx| tx.input.iter().any(|i| !i.witness.is_empty()));

    match (commitment, has_witness) {
        (None, false) => Ok(()),
        (None, true) => Err(RawtxError::Verify(
            "block has witness data but no witness commitment".into(),
        )),
        (Some(commitment), _) => {
            let coinbase = block
                .txdata
                .first()
                .ok_or_else(|| RawtxError::Verify("empty block".into()))?;
            let reserved = coinbase
                .input
                .first()
                .and_then(|i| i.witness.last())
                .ok_or_else(|| RawtxError::Verify("missing witness reserved value".into()))?;
            if reserved.len() != 32 {
                return Err(RawtxError::Verify(format!(
                    "witness reserved value is {} bytes",
                    reserved.len()
                )));
            }

            let mut wtxids: Vec<Hash32> = Vec::with_capacity(block.txdata.len());
            for (i, tx) in block.txdata.iter().enumerate() {
                if i == 0 {
                    // BIP141: coinbase wtxid is all zeros in the witness tree.
                    wtxids.push(Hash32::ZERO);
                } else {
                    let w = Hash32::from_hex(&tx.compute_wtxid().to_string())
                        .map_err(|e| RawtxError::Verify(format!("bad wtxid: {e}")))?;
                    wtxids.push(w);
                }
            }
            let root = merkle_root(&wtxids)
                .ok_or_else(|| RawtxError::Verify("empty witness tree".into()))?;
            let computed = sha256d(&root.reversed().0, reserved);
            if computed.as_slice() != commitment.as_slice() {
                return Err(RawtxError::Verify("witness commitment mismatch".into()));
            }
            Ok(())
        }
    }
}

/// Find a transaction by TXID and authenticate its bytes.
pub fn extract_tx(block: &Block, txid: Hash32) -> Result<Transaction> {
    for tx in &block.txdata {
        let id = Hash32::from_hex(&tx.compute_txid().to_string())
            .map_err(|e| RawtxError::Verify(format!("bad txid: {e}")))?;
        if id == txid {
            return Ok(tx.clone());
        }
    }
    Err(RawtxError::Verify(format!(
        "transaction {txid} is not in the block"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::absolute::LockTime;
    use bitcoin::block::Header;
    use bitcoin::hashes::Hash;
    use bitcoin::transaction::Version;
    use bitcoin::{
        Amount, BlockHash, CompactTarget, OutPoint, ScriptBuf, Sequence, TxIn, TxMerkleNode, TxOut,
        Witness,
    };

    fn commitment_script(root: &Hash32, reserved: &[u8; 32]) -> ScriptBuf {
        let c = sha256d(&root.reversed().0, reserved);
        let mut s = vec![0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed];
        s.extend_from_slice(&c);
        ScriptBuf::from_bytes(s)
    }

    fn witness_block() -> Block {
        let reserved = [7u8; 32];
        let spend = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: "11".repeat(32).parse().unwrap(),
                    vout: 0,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::from_slice(&[vec![1, 2, 3]]),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(1000),
                script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
            }],
        };
        let wtxids = vec![
            Hash32::ZERO,
            Hash32::from_hex(&spend.compute_wtxid().to_string()).unwrap(),
        ];
        let root = merkle_root(&wtxids).unwrap();
        let coinbase = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::from_bytes(vec![1, 2, 3, 4]),
                sequence: Sequence::MAX,
                witness: Witness::from_slice(&[reserved.to_vec()]),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(0),
                script_pubkey: commitment_script(&root, &reserved),
            }],
        };
        let mut block = Block {
            header: Header {
                version: bitcoin::block::Version::from_consensus(1),
                prev_blockhash: BlockHash::all_zeros(),
                merkle_root: TxMerkleNode::all_zeros(),
                time: 1,
                bits: CompactTarget::from_consensus(0x207f_ffff),
                nonce: 0,
            },
            txdata: vec![coinbase, spend],
        };
        block.header.merkle_root = block.compute_merkle_root().unwrap();
        block
    }

    #[test]
    fn accepts_valid_witness_block() {
        let block = witness_block();
        assert!(verify_witness_commitment(&block).is_ok());
    }

    #[test]
    fn rejects_tampered_witness() {
        let mut block = witness_block();
        block.txdata[1].input[0].witness = Witness::from_slice(&[vec![9, 9, 9]]);
        // The TXID tree (and therefore the header) is unchanged by the witness.
        assert!(block.compute_merkle_root().unwrap() == block.header.merkle_root);
        assert!(verify_witness_commitment(&block).is_err());
    }

    #[test]
    fn rejects_witness_without_commitment() {
        let mut block = witness_block();
        block.txdata[0].output[0].script_pubkey = ScriptBuf::from_bytes(vec![0x51]);
        assert!(verify_witness_commitment(&block).is_err());
    }

    #[test]
    fn verify_block_rejects_wrong_hash_and_header() {
        let block = witness_block();
        let bytes = bitcoin::consensus::serialize(&block);
        let hash = Hash32::from_hex(&block.block_hash().to_string()).unwrap();
        let header: [u8; 80] = bitcoin::consensus::serialize(&block.header)
            .try_into()
            .unwrap();
        assert!(verify_block(&bytes, &hash, &header).is_ok());

        let wrong = Hash32([0xaa; 32]);
        assert!(verify_block(&bytes, &wrong, &header).is_err());

        let mut bad_header = header;
        bad_header[0] ^= 0xff;
        assert!(verify_block(&bytes, &hash, &bad_header).is_err());
    }

    #[test]
    fn extract_tx_finds_only_present_transactions() {
        let block = witness_block();
        let txid = Hash32::from_hex(&block.txdata[1].compute_txid().to_string()).unwrap();
        assert!(extract_tx(&block, txid).is_ok());
        assert!(extract_tx(&block, Hash32([0xbb; 32])).is_err());
    }
}
