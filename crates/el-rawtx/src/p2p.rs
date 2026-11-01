//! Minimal Bitcoin P2P client: version/verack, ping/pong, getdata(block).
//!
//! Only whole blocks are requested (never txids). Peers must advertise
//! NODE_NETWORK; pruned peers (NODE_NETWORK_LIMITED) are not used for old
//! blocks. Use Tor for network privacy.

use crate::{BlockSource, RawtxError, Result};
use el_model::{Chain, Hash32};
use rand::seq::SliceRandom;
use rand::thread_rng;
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const PROTOCOL_VERSION: i32 = 70016;
const MAX_MESSAGE: u32 = 4 * 1024 * 1024 + 1024;
const NODE_NETWORK: u64 = 1;
const NODE_NETWORK_LIMITED: u64 = 1 << 10;

pub struct P2pSource {
    peers: Vec<String>,
    tor_socks: Option<String>,
    timeout: Duration,
    magic: [u8; 4],
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

fn sha256d(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    let first = h.finalize();
    let mut h2 = Sha256::new();
    h2.update(first);
    let mut out = [0u8; 32];
    out.copy_from_slice(&h2.finalize());
    out
}

fn checksum(payload: &[u8]) -> [u8; 4] {
    let d = sha256d(payload);
    [d[0], d[1], d[2], d[3]]
}

fn send_message<W: Write>(
    w: &mut W,
    magic: [u8; 4],
    command: &str,
    payload: &[u8],
) -> io::Result<()> {
    let mut header = Vec::with_capacity(24);
    header.extend_from_slice(&magic);
    let mut cmd = [0u8; 12];
    let bytes = command.as_bytes();
    let n = bytes.len().min(12);
    cmd[..n].copy_from_slice(&bytes[..n]);
    header.extend_from_slice(&cmd);
    header.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    header.extend_from_slice(&checksum(payload));
    w.write_all(&header)?;
    w.write_all(payload)?;
    w.flush()
}

fn read_message<R: Read>(r: &mut R, magic: [u8; 4], max: u32) -> io::Result<(String, Vec<u8>)> {
    let mut header = [0u8; 24];
    r.read_exact(&mut header)?;
    if header[0..4] != magic {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad magic"));
    }
    let cmd_end = header[4..16].iter().position(|b| *b == 0).unwrap_or(12);
    let command = String::from_utf8_lossy(&header[4..4 + cmd_end]).to_string();
    let len = u32::from_le_bytes([header[16], header[17], header[18], header[19]]);
    if len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message too large: {len}"),
        ));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    if checksum(&payload) != header[20..24] {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad checksum"));
    }
    Ok((command, payload))
}

fn put_varint(v: u64, out: &mut Vec<u8>) {
    if v < 0xfd {
        out.push(v as u8);
    } else if v <= 0xffff {
        out.push(0xfd);
        out.extend_from_slice(&(v as u16).to_le_bytes());
    } else if v <= 0xffff_ffff {
        out.push(0xfe);
        out.extend_from_slice(&(v as u32).to_le_bytes());
    } else {
        out.push(0xff);
        out.extend_from_slice(&v.to_le_bytes());
    }
}

fn put_varstr(v: &[u8], out: &mut Vec<u8>) {
    put_varint(v.len() as u64, out);
    out.extend_from_slice(v);
}

fn version_payload(services: u64, height: i32) -> Vec<u8> {
    let mut p = Vec::with_capacity(100);
    p.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    p.extend_from_slice(&services.to_le_bytes());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    p.extend_from_slice(&now.to_le_bytes());
    // addr_recv: services, ipv6-mapped-zero, port
    p.extend_from_slice(&0u64.to_le_bytes());
    p.extend_from_slice(&[0u8; 16]);
    p.extend_from_slice(&0u16.to_be_bytes());
    // addr_from
    p.extend_from_slice(&0u64.to_le_bytes());
    p.extend_from_slice(&[0u8; 16]);
    p.extend_from_slice(&0u16.to_be_bytes());
    p.extend_from_slice(&rand::random::<u64>().to_le_bytes());
    put_varstr(b"/e-lite:0.1/", &mut p);
    p.extend_from_slice(&height.to_le_bytes());
    p.push(1); // relay
    p
}

fn pong_payload(ping: &[u8]) -> Vec<u8> {
    if ping.len() >= 8 {
        ping[..8].to_vec()
    } else {
        vec![0u8; 8]
    }
}

fn getdata_payload(hash: &Hash32) -> Vec<u8> {
    let mut p = Vec::with_capacity(37);
    put_varint(1, &mut p);
    p.extend_from_slice(&2u32.to_le_bytes()); // MSG_BLOCK
    p.extend_from_slice(&hash.reversed().0);
    p
}

impl P2pSource {
    pub fn new(
        peers: Vec<String>,
        tor_socks: Option<String>,
        timeout: Duration,
        chain: Chain,
    ) -> Self {
        P2pSource {
            peers,
            tor_socks,
            timeout,
            magic: chain.magic(),
        }
    }

    fn connect(&self, peer: &str) -> Result<Box<dyn ReadWrite>> {
        if let Some(proxy) = &self.tor_socks {
            // Unique credentials per stream: Tor's IsolateSOCKSAuth gives every
            // peer its own circuit.
            let user = format!("e-lite-{}", rand::random::<u64>());
            let stream =
                socks::Socks5Stream::connect_with_password(proxy.as_str(), peer, &user, "e-lite")
                    .map_err(|e| RawtxError::Unavailable(format!("tor connect {peer}: {e}")))?;
            stream
                .get_ref()
                .set_read_timeout(Some(self.timeout))
                .map_err(|e| RawtxError::Io(e.to_string()))?;
            stream
                .get_ref()
                .set_write_timeout(Some(self.timeout))
                .map_err(|e| RawtxError::Io(e.to_string()))?;
            Ok(Box::new(stream))
        } else {
            let stream = TcpStream::connect(peer)
                .map_err(|e| RawtxError::Unavailable(format!("connect {peer}: {e}")))?;
            stream
                .set_nodelay(true)
                .map_err(|e| RawtxError::Io(e.to_string()))?;
            stream
                .set_read_timeout(Some(self.timeout))
                .map_err(|e| RawtxError::Io(e.to_string()))?;
            stream
                .set_write_timeout(Some(self.timeout))
                .map_err(|e| RawtxError::Io(e.to_string()))?;
            Ok(Box::new(stream))
        }
    }

    fn fetch_from(&self, peer: &str, hash: &Hash32) -> Result<Vec<u8>> {
        let mut stream = self.connect(peer)?;
        send_message(&mut stream, self.magic, "version", &version_payload(0, 0))
            .map_err(|e| RawtxError::Unavailable(format!("{peer} version: {e}")))?;

        let mut got_version = false;
        let mut services = 0u64;
        loop {
            let (cmd, payload) = read_message(&mut stream, self.magic, MAX_MESSAGE)
                .map_err(|e| RawtxError::Unavailable(format!("{peer} handshake: {e}")))?;
            match cmd.as_str() {
                "version" => {
                    got_version = true;
                    if payload.len() >= 12 {
                        services = u64::from_le_bytes(payload[4..12].try_into().unwrap());
                    }
                    send_message(&mut stream, self.magic, "verack", &[])
                        .map_err(|e| RawtxError::Unavailable(format!("{peer} verack: {e}")))?;
                }
                "verack" => {
                    if got_version {
                        break;
                    }
                }
                "ping" => {
                    let _ = send_message(&mut stream, self.magic, "pong", &pong_payload(&payload));
                }
                _ => {}
            }
        }

        if services & NODE_NETWORK == 0 {
            let limited = services & NODE_NETWORK_LIMITED != 0;
            return Err(RawtxError::Unavailable(format!(
                "{peer} does not serve full blocks (services={services:#x}{})",
                if limited { ", limited" } else { "" }
            )));
        }

        send_message(&mut stream, self.magic, "getdata", &getdata_payload(hash))
            .map_err(|e| RawtxError::Unavailable(format!("{peer} getdata: {e}")))?;

        loop {
            let (cmd, payload) = read_message(&mut stream, self.magic, MAX_MESSAGE)
                .map_err(|e| RawtxError::Unavailable(format!("{peer} block: {e}")))?;
            match cmd.as_str() {
                "block" => {
                    let block: bitcoin::Block = bitcoin::consensus::deserialize(&payload)
                        .map_err(|e| RawtxError::Verify(format!("{peer} bad block: {e}")))?;
                    let got = Hash32::from_hex(&block.block_hash().to_string())
                        .map_err(|e| RawtxError::Verify(e.to_string()))?;
                    if got != *hash {
                        return Err(RawtxError::Verify(format!(
                            "{peer} served hash {got}, expected {hash}"
                        )));
                    }
                    return Ok(payload);
                }
                "ping" => {
                    let _ = send_message(&mut stream, self.magic, "pong", &pong_payload(&payload));
                }
                _ => {}
            }
        }
    }
}

impl BlockSource for P2pSource {
    fn name(&self) -> &str {
        "p2p"
    }

    fn fetch_block(&self, hash: &Hash32) -> Result<Vec<u8>> {
        if self.peers.is_empty() {
            return Err(RawtxError::Unavailable("no p2p peers".into()));
        }
        let mut peers = self.peers.clone();
        peers.shuffle(&mut thread_rng());
        let mut errors = Vec::new();
        for peer in peers.iter().take(4) {
            match self.fetch_from(peer, hash) {
                Ok(bytes) => return Ok(bytes),
                Err(e) => errors.push(format!("{peer}: {e}")),
            }
        }
        Err(RawtxError::Unavailable(errors.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::absolute::LockTime;
    use bitcoin::block::Header;
    use bitcoin::hashes::Hash;
    use bitcoin::transaction::Version as TxVersion;
    use bitcoin::{
        Amount, Block, BlockHash, CompactTarget, OutPoint, ScriptBuf, Sequence, Transaction, TxIn,
        TxMerkleNode, TxOut, Witness,
    };
    use std::net::TcpListener;

    fn test_block() -> Block {
        let coinbase = Transaction {
            version: TxVersion::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::from_bytes(vec![1, 2, 3]),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(50),
                script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
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
            txdata: vec![coinbase],
        };
        block.header.merkle_root = block.compute_merkle_root().unwrap();
        block
    }

    fn mock_peer(services: u64, block_bytes: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let magic = Chain::Regtest.magic();
            let (cmd, _) = read_message(&mut sock, magic, MAX_MESSAGE).unwrap();
            assert_eq!(cmd, "version");
            send_message(&mut sock, magic, "version", &version_payload(services, 0)).unwrap();
            send_message(&mut sock, magic, "verack", &[]).unwrap();
            while let Ok((cmd, _payload)) = read_message(&mut sock, magic, MAX_MESSAGE) {
                if cmd == "getdata" {
                    send_message(&mut sock, magic, "block", &block_bytes).unwrap();
                }
            }
        });
        addr
    }

    #[test]
    fn fetches_block_from_peer() {
        let block = test_block();
        let bytes = bitcoin::consensus::serialize(&block);
        let hash = Hash32::from_hex(&block.block_hash().to_string()).unwrap();
        let addr = mock_peer(NODE_NETWORK, bytes.clone());
        let src = P2pSource::new(vec![addr], None, Duration::from_secs(5), Chain::Regtest);
        assert_eq!(src.fetch_block(&hash).unwrap(), bytes);
    }

    #[test]
    fn rejects_limited_only_peer() {
        let block = test_block();
        let bytes = bitcoin::consensus::serialize(&block);
        let hash = Hash32::from_hex(&block.block_hash().to_string()).unwrap();
        let addr = mock_peer(NODE_NETWORK_LIMITED, bytes);
        let src = P2pSource::new(vec![addr], None, Duration::from_secs(5), Chain::Regtest);
        assert!(src.fetch_block(&hash).is_err());
    }
}
