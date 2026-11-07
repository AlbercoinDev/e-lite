//! Minimal Bitcoin P2P client: version/verack, ping/pong, getdata(block).
//!
//! Only whole blocks are requested (never txids). Peers must advertise
//! NODE_NETWORK; pruned peers (NODE_NETWORK_LIMITED) are not used for old
//! blocks. BIP324 v2 is tried first and falls back to v1. Use Tor for network
//! privacy.

use crate::{BlockSource, RawtxError, RawtxStats, Result};
use bip324::io::{Payload, Protocol};
use bip324::{PacketType, Role};
use el_model::{Chain, Hash32};
use rand::seq::SliceRandom;
use rand::thread_rng;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{self, BufReader, Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PROTOCOL_VERSION: i32 = 70016;
const MAX_MESSAGE: u32 = 4 * 1024 * 1024 + 1024;
const NODE_NETWORK: u64 = 1;
const NODE_NETWORK_LIMITED: u64 = 1 << 10;
const NODE_WITNESS: u64 = 1 << 3;
/// BIP144: request the witness serialization explicitly.
const MSG_WITNESS_BLOCK: u32 = 2 | (1 << 30);

pub struct P2pSource {
    peers: Vec<String>,
    tor_socks: Option<String>,
    timeout: Duration,
    magic: [u8; 4],
    v2: bool,
    fallback_clearnet: bool,
    fallback_warned: AtomicBool,
    cooldown: Duration,
    total_timeout: Duration,
    peer_failures: Mutex<HashMap<String, Instant>>,
    last_good: Mutex<Option<String>>,
    tor_cooldown_until: Mutex<Option<Instant>>,
    stats: Option<Arc<RawtxStats>>,
}

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

fn build_message(magic: [u8; 4], command: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(24 + payload.len());
    out.extend_from_slice(&magic);
    let mut cmd = [0u8; 12];
    let bytes = command.as_bytes();
    let n = bytes.len().min(12);
    cmd[..n].copy_from_slice(&bytes[..n]);
    out.extend_from_slice(&cmd);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&checksum(payload));
    out.extend_from_slice(payload);
    out
}

fn send_message<W: Write>(
    w: &mut W,
    magic: [u8; 4],
    command: &str,
    payload: &[u8],
) -> io::Result<()> {
    w.write_all(&build_message(magic, command, payload))?;
    w.flush()
}

fn read_message<R: Read>(r: &mut R, magic: [u8; 4], max: u32) -> io::Result<(String, Vec<u8>)> {
    let mut header = [0u8; 24];
    r.read_exact(&mut header)?;
    if header[0..4] != magic {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad magic"));
    }
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
    let cmd_end = header[4..16].iter().position(|b| *b == 0).unwrap_or(12);
    Ok((
        String::from_utf8_lossy(&header[4..4 + cmd_end]).to_string(),
        payload,
    ))
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
    p.extend_from_slice(&0u64.to_le_bytes());
    p.extend_from_slice(&[0u8; 16]);
    p.extend_from_slice(&0u16.to_be_bytes());
    p.extend_from_slice(&0u64.to_le_bytes());
    p.extend_from_slice(&[0u8; 16]);
    p.extend_from_slice(&0u16.to_be_bytes());
    p.extend_from_slice(&rand::random::<u64>().to_le_bytes());
    put_varstr(b"/e-lite:0.1/", &mut p);
    p.extend_from_slice(&height.to_le_bytes());
    p.push(1);
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
    p.extend_from_slice(&MSG_WITNESS_BLOCK.to_le_bytes());
    p.extend_from_slice(&hash.reversed().0);
    p
}

/// BIP324 application message: 0x00 + 12-byte ASCII type + payload.
fn v2_message(command: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(13 + payload.len());
    out.push(0);
    let mut cmd = [0u8; 12];
    let bytes = command.as_bytes();
    let n = bytes.len().min(12);
    cmd[..n].copy_from_slice(&bytes[..n]);
    out.extend_from_slice(&cmd);
    out.extend_from_slice(payload);
    out
}

/// Parse a BIP324 application message (short type ID or long ASCII type).
fn v2_parse(contents: &[u8]) -> io::Result<(String, Vec<u8>)> {
    let Some(first) = contents.first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "empty v2 message",
        ));
    };
    if *first == 0 {
        if contents.len() < 13 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "short v2 type"));
        }
        let cmd_end = contents[1..13].iter().position(|b| *b == 0).unwrap_or(12);
        Ok((
            String::from_utf8_lossy(&contents[1..1 + cmd_end]).to_string(),
            contents[13..].to_vec(),
        ))
    } else {
        let command = match *first {
            1 => "addr",
            2 => "block",
            3 => "blocktxn",
            4 => "cmpctblock",
            5 => "feefilter",
            6 => "filteradd",
            7 => "filterclear",
            8 => "filterload",
            9 => "getblocks",
            10 => "getblocktxn",
            11 => "getdata",
            12 => "getheaders",
            13 => "headers",
            14 => "inv",
            15 => "mempool",
            16 => "merkleblock",
            17 => "notfound",
            18 => "ping",
            19 => "pong",
            20 => "sendcmpct",
            21 => "tx",
            22 => "getcfilters",
            23 => "cfilter",
            24 => "getcfheaders",
            25 => "cfheaders",
            26 => "getcfcheckpt",
            27 => "cfcheckpt",
            28 => "addrv2",
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown v2 type {other}"),
                ))
            }
        };
        Ok((command.to_string(), contents[1..].to_vec()))
    }
}

fn check_block(payload: &[u8], hash: &Hash32, peer: &str) -> Result<bitcoin::Block> {
    let block = crate::verify_block_by_hash(payload, hash)
        .map_err(|e| RawtxError::Verify(format!("{peer}: {e}")))?;
    crate::verify_witness_commitment(&block)
        .map_err(|e| RawtxError::Verify(format!("{peer}: {e}")))?;
    Ok(block)
}

fn split_host_port(peer: &str) -> io::Result<(&str, u16)> {
    if let Some(rest) = peer.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad IPv6 peer"))?;
        let port = tail
            .strip_prefix(':')
            .and_then(|p| p.parse::<u16>().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad peer port"))?;
        return Ok((host, port));
    }
    let (host, port) = peer
        .rsplit_once(':')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad peer address"))?;
    let port = port
        .parse::<u16>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad peer port"))?;
    Ok((host, port))
}

/// Minimal SOCKS5 CONNECT with bounded connect/read/write timeouts.
///
/// The `socks` crate does not expose timeouts, so a stalled Tor SOCKS proxy
/// could block a P2P fetch indefinitely. This performs the same handshake with
/// the P2P timeout applied to every step.
fn socks_connect(proxy: &str, peer: &str, timeout: Duration) -> io::Result<TcpStream> {
    let proxy_addrs: Vec<_> = proxy.to_socket_addrs()?.collect();
    let mut stream = None;
    for addr in proxy_addrs {
        if let Ok(sock) = TcpStream::connect_timeout(&addr, timeout) {
            stream = Some(sock);
            break;
        }
    }
    let mut stream = stream.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("socks proxy {proxy} did not accept a connection"),
        )
    })?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    // Offer no-auth and username/password (Tor uses the username for circuit
    // isolation).
    stream.write_all(&[5, 2, 0, 2])?;
    let mut resp = [0u8; 2];
    stream.read_exact(&mut resp)?;
    if resp[0] != 5 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad SOCKS version",
        ));
    }
    match resp[1] {
        0 => {}
        2 => {
            let user = format!("e-lite-{}", rand::random::<u64>());
            let user = user.as_bytes();
            let pass = b"e-lite";
            let mut auth = Vec::with_capacity(3 + user.len() + pass.len());
            auth.push(1);
            auth.push(user.len() as u8);
            auth.extend_from_slice(user);
            auth.push(pass.len() as u8);
            auth.extend_from_slice(pass);
            stream.write_all(&auth)?;
            let mut status = [0u8; 2];
            stream.read_exact(&mut status)?;
            if status[0] != 1 || status[1] != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "SOCKS authentication failed",
                ));
            }
        }
        0xff => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SOCKS proxy rejected all auth methods",
            ))
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported SOCKS auth method",
            ))
        }
    }

    let (host, port) = split_host_port(peer)?;
    let mut request = vec![5, 1, 0];
    if let Ok(ip) = host.parse::<IpAddr>() {
        match ip {
            IpAddr::V4(v4) => {
                request.push(1);
                request.extend_from_slice(&v4.octets());
            }
            IpAddr::V6(v6) => {
                request.push(4);
                request.extend_from_slice(&v6.octets());
            }
        }
    } else {
        let host = host.as_bytes();
        if host.len() > 255 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "peer host too long",
            ));
        }
        request.push(3);
        request.push(host.len() as u8);
        request.extend_from_slice(host);
    }
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request)?;

    let mut head = [0u8; 4];
    stream.read_exact(&mut head)?;
    if head[0] != 5 || head[1] != 0 {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("SOCKS connect failed (reply {})", head[1]),
        ));
    }
    match head[3] {
        1 => {
            let mut skip = [0u8; 4];
            stream.read_exact(&mut skip)?;
        }
        4 => {
            let mut skip = [0u8; 16];
            stream.read_exact(&mut skip)?;
        }
        3 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len)?;
            let mut skip = vec![0u8; len[0] as usize];
            stream.read_exact(&mut skip)?;
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad SOCKS reply address type",
            ))
        }
    }
    let mut port = [0u8; 2];
    stream.read_exact(&mut port)?;
    Ok(stream)
}

impl P2pSource {
    pub fn new(
        peers: Vec<String>,
        tor_socks: Option<String>,
        timeout: Duration,
        chain: Chain,
        v2: bool,
        fallback_clearnet: bool,
        cooldown: Duration,
    ) -> Self {
        P2pSource {
            peers,
            tor_socks,
            timeout,
            magic: chain.magic(),
            v2,
            fallback_clearnet,
            fallback_warned: AtomicBool::new(false),
            cooldown,
            total_timeout: timeout.saturating_mul(2),
            peer_failures: Mutex::new(HashMap::new()),
            last_good: Mutex::new(None),
            tor_cooldown_until: Mutex::new(None),
            stats: None,
        }
    }

    /// Attach the resolver's counters (optional; tests omit it).
    pub fn with_stats(mut self, stats: Arc<RawtxStats>) -> Self {
        self.stats = Some(stats);
        self
    }

    fn bump(&self, f: impl FnOnce(&RawtxStats)) {
        if let Some(stats) = &self.stats {
            f(stats);
        }
    }

    fn direct_connect(&self, peer: &str, timeout: Duration) -> Result<TcpStream> {
        let addrs: Vec<_> = peer
            .to_socket_addrs()
            .map_err(|e| RawtxError::Unavailable(format!("resolve {peer}: {e}")))?
            .collect();
        let mut last = None;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, timeout) {
                Ok(stream) => return Ok(stream),
                Err(e) => last = Some(e),
            }
        }
        Err(RawtxError::Unavailable(format!(
            "connect {peer}: {}",
            last.map(|e| e.to_string())
                .unwrap_or_else(|| "no addresses".to_string())
        )))
    }

    fn connect_raw(&self, peer: &str, timeout: Duration) -> Result<TcpStream> {
        let tor_cooled = self
            .tor_cooldown_until
            .lock()
            .map(|until| until.is_some_and(|t| t > Instant::now()))
            .unwrap_or(false);
        let stream = if let Some(proxy) = &self.tor_socks {
            if tor_cooled {
                if self.fallback_clearnet {
                    self.direct_connect(peer, timeout)?
                } else {
                    return Err(RawtxError::Unavailable(format!(
                        "tor cooling down for {peer}; clearnet fallback disabled"
                    )));
                }
            } else {
                match socks_connect(proxy, peer, timeout) {
                    Ok(s) => {
                        if let Ok(mut until) = self.tor_cooldown_until.lock() {
                            *until = None;
                        }
                        s
                    }
                    Err(e) => {
                        self.bump(|stats| {
                            stats.p2p_tor_failures.fetch_add(1, Ordering::Relaxed);
                        });
                        if let Ok(mut until) = self.tor_cooldown_until.lock() {
                            *until = Some(Instant::now() + self.cooldown);
                        }
                        if self.fallback_clearnet {
                            if !self.fallback_warned.swap(true, Ordering::Relaxed) {
                                eprintln!(
                                    "[e-lite] WARNING: Tor connect failed for P2P peer {peer} ({e}); using clearnet (operator IP exposed)"
                                );
                            }
                            self.direct_connect(peer, timeout)?
                        } else {
                            return Err(RawtxError::Unavailable(format!(
                                "tor connect {peer}: {e}"
                            )));
                        }
                    }
                }
            }
        } else {
            self.direct_connect(peer, timeout)?
        };
        stream
            .set_nodelay(true)
            .map_err(|e| RawtxError::Io(e.to_string()))?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|e| RawtxError::Io(e.to_string()))?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|e| RawtxError::Io(e.to_string()))?;
        Ok(stream)
    }

    fn fetch_from(&self, peer: &str, hash: &Hash32, budget: Duration) -> Result<Vec<u8>> {
        let deadline = Instant::now() + budget;
        let mut v2_error = None;
        if self.v2 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(RawtxError::Unavailable(format!("{peer} p2p timeout")));
            }
            match self.fetch_from_v2(peer, hash, remaining) {
                Ok(bytes) => return Ok(bytes),
                Err(e) => v2_error = Some(e),
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(RawtxError::Unavailable(format!("{peer} p2p timeout")));
        }
        match self.fetch_from_v1(peer, hash, remaining) {
            Ok(bytes) => Ok(bytes),
            Err(e) => match v2_error {
                Some(v2) => Err(RawtxError::Unavailable(format!("{v2}; v1: {e}"))),
                None => Err(e),
            },
        }
    }

    fn fetch_from_v1(&self, peer: &str, hash: &Hash32, timeout: Duration) -> Result<Vec<u8>> {
        let mut stream = self.connect_raw(peer, timeout)?;
        send_message(
            &mut stream,
            self.magic,
            "version",
            &version_payload(NODE_WITNESS, 0),
        )
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
        self.require_network(peer, services)?;

        send_message(&mut stream, self.magic, "getdata", &getdata_payload(hash))
            .map_err(|e| RawtxError::Unavailable(format!("{peer} getdata: {e}")))?;
        loop {
            let (cmd, payload) = read_message(&mut stream, self.magic, MAX_MESSAGE)
                .map_err(|e| RawtxError::Unavailable(format!("{peer} block: {e}")))?;
            match cmd.as_str() {
                "block" => {
                    check_block(&payload, hash, peer)?;
                    return Ok(payload);
                }
                "notfound" => {
                    return Err(RawtxError::Unavailable(format!(
                        "{peer} notfound for {hash}"
                    )))
                }
                "ping" => {
                    let _ = send_message(&mut stream, self.magic, "pong", &pong_payload(&payload));
                }
                _ => {}
            }
        }
    }

    fn fetch_from_v2(&self, peer: &str, hash: &Hash32, timeout: Duration) -> Result<Vec<u8>> {
        let stream = self.connect_raw(peer, timeout)?;
        let reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| RawtxError::Io(e.to_string()))?,
        );
        let writer = stream;
        let mut protocol =
            Protocol::new(self.magic, Role::Initiator, None, None, reader, writer)
                .map_err(|e| RawtxError::Unavailable(format!("{peer} v2 handshake: {e}")))?;

        self.v2_send(&mut protocol, "version", &version_payload(NODE_WITNESS, 0))?;
        let mut got_version = false;
        let mut services = 0u64;
        loop {
            let (cmd, payload) = self.v2_read(&mut protocol, peer)?;
            match cmd.as_str() {
                "version" => {
                    got_version = true;
                    if payload.len() >= 12 {
                        services = u64::from_le_bytes(payload[4..12].try_into().unwrap());
                    }
                    self.v2_send(&mut protocol, "verack", &[])?;
                }
                "verack" => {
                    if got_version {
                        break;
                    }
                }
                "ping" => {
                    let _ = self.v2_send(&mut protocol, "pong", &pong_payload(&payload));
                }
                _ => {}
            }
        }
        self.require_network(peer, services)?;

        self.v2_send(&mut protocol, "getdata", &getdata_payload(hash))?;
        loop {
            let (cmd, payload) = self.v2_read(&mut protocol, peer)?;
            match cmd.as_str() {
                "block" => {
                    check_block(&payload, hash, peer)?;
                    return Ok(payload);
                }
                "notfound" => {
                    return Err(RawtxError::Unavailable(format!(
                        "{peer} notfound for {hash}"
                    )))
                }
                "ping" => {
                    let _ = self.v2_send(&mut protocol, "pong", &pong_payload(&payload));
                }
                _ => {}
            }
        }
    }

    fn v2_send(
        &self,
        protocol: &mut Protocol<BufReader<TcpStream>, TcpStream>,
        command: &str,
        payload: &[u8],
    ) -> Result<()> {
        protocol
            .write(&Payload::genuine(v2_message(command, payload)))
            .map_err(|e| RawtxError::Unavailable(format!("v2 write: {e}")))
    }

    fn v2_read(
        &self,
        protocol: &mut Protocol<BufReader<TcpStream>, TcpStream>,
        peer: &str,
    ) -> Result<(String, Vec<u8>)> {
        loop {
            let packet = protocol
                .read()
                .map_err(|e| RawtxError::Unavailable(format!("{peer} v2 read: {e}")))?;
            if matches!(packet.packet_type(), PacketType::Decoy) {
                continue;
            }
            return v2_parse(packet.contents())
                .map_err(|e| RawtxError::Unavailable(format!("{peer} v2 message: {e}")));
        }
    }

    fn require_network(&self, peer: &str, services: u64) -> Result<()> {
        if services & NODE_NETWORK == 0 {
            let limited = services & NODE_NETWORK_LIMITED != 0;
            return Err(RawtxError::Unavailable(format!(
                "{peer} does not serve full blocks (services={services:#x}{})",
                if limited { ", limited" } else { "" }
            )));
        }
        if services & NODE_WITNESS == 0 {
            return Err(RawtxError::Unavailable(format!(
                "{peer} does not serve witness blocks (services={services:#x})"
            )));
        }
        Ok(())
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
        if let Some(good) = self.last_good.lock().ok().and_then(|g| g.clone()) {
            if let Some(pos) = peers.iter().position(|p| *p == good) {
                let good = peers.remove(pos);
                peers.insert(0, good);
            }
        }

        let now = Instant::now();
        let cooling: HashSet<String> = self
            .peer_failures
            .lock()
            .map(|m| {
                m.iter()
                    .filter(|(_, until)| **until > now)
                    .map(|(peer, _)| peer.clone())
                    .collect()
            })
            .unwrap_or_default();
        let candidates: Vec<String> = peers
            .into_iter()
            .filter(|peer| !cooling.contains(peer))
            .collect();
        if candidates.is_empty() {
            self.bump(|stats| {
                stats.p2p_skipped.fetch_add(1, Ordering::Relaxed);
            });
            return Err(RawtxError::Unavailable(
                "all p2p peers are cooling down".into(),
            ));
        }

        let started = Instant::now();
        let mut errors = Vec::new();
        for peer in candidates.into_iter().take(3) {
            let remaining = self.total_timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                errors.push("p2p total timeout".to_string());
                break;
            }
            let budget = remaining.min(self.timeout);
            self.bump(|stats| {
                stats.p2p_attempts.fetch_add(1, Ordering::Relaxed);
            });
            match self.fetch_from(&peer, hash, budget) {
                Ok(bytes) => {
                    if let Ok(mut good) = self.last_good.lock() {
                        *good = Some(peer.clone());
                    }
                    if let Ok(mut failures) = self.peer_failures.lock() {
                        failures.remove(&peer);
                    }
                    return Ok(bytes);
                }
                Err(e) => {
                    self.bump(|stats| {
                        stats.p2p_failures.fetch_add(1, Ordering::Relaxed);
                    });
                    if let Ok(mut failures) = self.peer_failures.lock() {
                        failures.insert(peer.clone(), Instant::now() + self.cooldown);
                    }
                    errors.push(format!("{peer}: {e}"));
                }
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

    #[test]
    fn socks_connect_is_bounded_by_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 16];
                let _ = sock.read(&mut buf);
                std::thread::sleep(Duration::from_secs(2));
            }
        });
        let started = Instant::now();
        let result = socks_connect(&addr, "127.0.0.1:8333", Duration::from_millis(200));
        assert!(result.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "SOCKS handshake must not block past the timeout"
        );
    }

    #[test]
    fn socks_connect_completes_password_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut greeting = [0u8; 4];
            sock.read_exact(&mut greeting).unwrap();
            sock.write_all(&[5, 2]).unwrap();
            let mut auth_head = [0u8; 2];
            sock.read_exact(&mut auth_head).unwrap();
            let mut user = vec![0u8; auth_head[1] as usize];
            sock.read_exact(&mut user).unwrap();
            let mut plen = [0u8; 1];
            sock.read_exact(&mut plen).unwrap();
            let mut pass = vec![0u8; plen[0] as usize];
            sock.read_exact(&mut pass).unwrap();
            sock.write_all(&[1, 0]).unwrap();
            // CONNECT 127.0.0.1:8333 is 4 + 4 + 2 bytes.
            let mut request = [0u8; 10];
            sock.read_exact(&mut request).unwrap();
            sock.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0]).unwrap();
            std::thread::sleep(Duration::from_secs(1));
        });
        let stream = socks_connect(&addr, "127.0.0.1:8333", Duration::from_secs(2)).unwrap();
        assert_eq!(
            stream.peer_addr().unwrap().ip(),
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        );
    }

    #[test]
    fn failed_peer_is_cooled_down() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let accepts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepts);
        std::thread::spawn(move || {
            while let Ok((mut sock, _)) = listener.accept() {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                std::thread::sleep(Duration::from_secs(2));
            }
        });

        let stats = Arc::new(RawtxStats::default());
        let src = P2pSource::new(
            vec![addr],
            None,
            Duration::from_millis(200),
            Chain::Regtest,
            false,
            false,
            Duration::from_secs(60),
        )
        .with_stats(Arc::clone(&stats));
        let hash = Hash32([0x11; 32]);
        assert!(src.fetch_block(&hash).is_err());
        assert_eq!(accepts.load(std::sync::atomic::Ordering::SeqCst), 1);
        // A second block must not reconnect while the peer is cooled down.
        assert!(src.fetch_block(&hash).is_err());
        assert_eq!(accepts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            stats.p2p_attempts.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            stats.p2p_failures.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            stats.p2p_skipped.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn getdata_requests_witness_blocks() {
        let payload = getdata_payload(&Hash32([0x33; 32]));
        let inv_type = u32::from_le_bytes(payload[1..5].try_into().unwrap());
        assert_eq!(inv_type, MSG_WITNESS_BLOCK);
        assert_eq!(inv_type & (1 << 30), 1 << 30);
    }

    #[test]
    fn version_advertises_witness() {
        let payload = version_payload(NODE_WITNESS, 0);
        let services = u64::from_le_bytes(payload[4..12].try_into().unwrap());
        assert_eq!(services & NODE_WITNESS, NODE_WITNESS);
    }

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

    fn mock_peer_v1(services: u64, block_bytes: Vec<u8>, discard_first: bool) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            if discard_first {
                // The client probes BIP324 first; drop that connection so it
                // retries with v1 on a fresh one.
                let (probe, _) = listener.accept().unwrap();
                drop(probe);
            }
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

    fn mock_peer_v2(services: u64, block_bytes: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let magic = Chain::Regtest.magic();
            let reader = BufReader::new(sock.try_clone().unwrap());
            let mut protocol = match Protocol::new(magic, Role::Responder, None, None, reader, sock)
            {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("v2 mock: handshake error {e}");
                    return;
                }
            };
            // Client version.
            let (cmd, _) = v2_read_test(&mut protocol, magic);
            assert_eq!(cmd, "version");
            v2_send_test(
                &mut protocol,
                magic,
                "version",
                &version_payload(services, 0),
            );
            v2_send_test(&mut protocol, magic, "verack", &[]);
            loop {
                let (cmd, _) = v2_read_test(&mut protocol, magic);
                if cmd == "getdata" {
                    v2_send_test(&mut protocol, magic, "block", &block_bytes);
                }
                // Keep the session open: getdata follows verack.
            }
        });
        addr
    }

    fn v2_read_test(
        protocol: &mut Protocol<BufReader<TcpStream>, TcpStream>,
        magic: [u8; 4],
    ) -> (String, Vec<u8>) {
        loop {
            let packet = protocol.read().unwrap();
            if matches!(packet.packet_type(), PacketType::Decoy) {
                continue;
            }
            let _ = magic;
            match v2_parse(packet.contents()) {
                Ok(v) => return v,
                Err(e) => panic!("v2 parse: {e}"),
            }
        }
    }

    fn v2_send_test(
        protocol: &mut Protocol<BufReader<TcpStream>, TcpStream>,
        magic: [u8; 4],
        cmd: &str,
        payload: &[u8],
    ) {
        let _ = magic;
        protocol
            .write(&Payload::genuine(v2_message(cmd, payload)))
            .unwrap();
    }

    #[test]
    fn fetches_block_over_v1() {
        let block = test_block();
        let bytes = bitcoin::consensus::serialize(&block);
        let hash = Hash32::from_hex(&block.block_hash().to_string()).unwrap();
        let addr = mock_peer_v1(NODE_NETWORK | NODE_WITNESS, bytes.clone(), false);
        let src = P2pSource::new(
            vec![addr],
            None,
            Duration::from_secs(5),
            Chain::Regtest,
            false,
            false,
            Duration::from_secs(5),
        );
        assert_eq!(src.fetch_block(&hash).unwrap(), bytes);
    }

    #[test]
    fn fetches_block_over_v2() {
        let block = test_block();
        let bytes = bitcoin::consensus::serialize(&block);
        let hash = Hash32::from_hex(&block.block_hash().to_string()).unwrap();
        let addr = mock_peer_v2(NODE_NETWORK | NODE_WITNESS, bytes.clone());
        let src = P2pSource::new(
            vec![addr],
            None,
            Duration::from_secs(5),
            Chain::Regtest,
            true,
            false,
            Duration::from_secs(5),
        );
        assert_eq!(src.fetch_block(&hash).unwrap(), bytes);
    }

    #[test]
    fn falls_back_to_v1_when_v2_is_unsupported() {
        let block = test_block();
        let bytes = bitcoin::consensus::serialize(&block);
        let hash = Hash32::from_hex(&block.block_hash().to_string()).unwrap();
        let addr = mock_peer_v1(NODE_NETWORK | NODE_WITNESS, bytes.clone(), true);
        let src = P2pSource::new(
            vec![addr],
            None,
            Duration::from_secs(5),
            Chain::Regtest,
            true,
            false,
            Duration::from_secs(5),
        );
        assert_eq!(src.fetch_block(&hash).unwrap(), bytes);
    }

    #[test]
    fn rejects_peer_without_witness_service() {
        let block = test_block();
        let bytes = bitcoin::consensus::serialize(&block);
        let hash = Hash32::from_hex(&block.block_hash().to_string()).unwrap();
        let addr = mock_peer_v1(NODE_NETWORK, bytes, false);
        let src = P2pSource::new(
            vec![addr],
            None,
            Duration::from_secs(5),
            Chain::Regtest,
            false,
            false,
            Duration::from_secs(5),
        );
        let err = src.fetch_block(&hash).unwrap_err();
        assert!(format!("{err}").contains("witness"), "{err}");
    }

    #[test]
    fn rejects_limited_only_peer() {
        let block = test_block();
        let bytes = bitcoin::consensus::serialize(&block);
        let hash = Hash32::from_hex(&block.block_hash().to_string()).unwrap();
        let addr = mock_peer_v1(NODE_NETWORK_LIMITED, bytes, false);
        let src = P2pSource::new(
            vec![addr],
            None,
            Duration::from_secs(5),
            Chain::Regtest,
            false,
            false,
            Duration::from_secs(5),
        );
        assert!(src.fetch_block(&hash).is_err());
    }
}
