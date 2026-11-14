//! Minimal Bitcoin P2P client: version/verack, ping/pong, getdata(block).
//!
//! Only whole blocks are requested (never txids). Peers must advertise
//! NODE_NETWORK; pruned peers (NODE_NETWORK_LIMITED) are not used for old
//! blocks. BIP324 v2 is tried first and falls back to v1. Use Tor for network
//! privacy.

use crate::health::{Blame, HealthMap};
use crate::{AttemptCtl, BlockSource, Endpoint, FetchMode, RawtxError, RawtxStats, Result};
use bip324::io::{Payload, Protocol};
use bip324::{PacketType, Role};
use el_model::{Chain, Hash32};
use rand::seq::SliceRandom;
use rand::thread_rng;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt;
use std::io::{self, BufReader, Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

const PROTOCOL_VERSION: i32 = 70016;
const MAX_MESSAGE: u32 = 4 * 1024 * 1024 + 1024;
const NODE_NETWORK: u64 = 1;
const NODE_NETWORK_LIMITED: u64 = 1 << 10;
const NODE_WITNESS: u64 = 1 << 3;
/// BIP144: request the witness serialization explicitly.
const MSG_WITNESS_BLOCK: u32 = 2 | (1 << 30);

/// A peer to fetch blocks from, with what Core knows about its transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerHint {
    pub addr: String,
    /// `Some(false)`: Core talks BIP324 v1 to it, so the v2 attempt (a whole
    /// extra connection over Tor) is skipped. `None`: unknown, try v2 first.
    pub v2: Option<bool>,
}

impl From<String> for PeerHint {
    fn from(addr: String) -> Self {
        PeerHint { addr, v2: None }
    }
}

impl From<&str> for PeerHint {
    fn from(addr: &str) -> Self {
        PeerHint::from(addr.to_string())
    }
}

/// A transient peer failure backs off from this (doubling up to the
/// provider cooldown): every dead peer costs a whole connection attempt.
const PEER_TRANSIENT_BASE: Duration = Duration::from_secs(30);
/// The local Tor SOCKS proxy refused a connection or its handshake: P2P over
/// Tor stops for this long. A peer-side failure never trips it (AUD-153).
const PROXY_PAUSE: Duration = Duration::from_secs(15);
/// Peers tried per call.
const PEERS_PER_CALL: usize = 3;
/// Phase limits (AUD-160). A SOCKS CONNECT (circuit + peer TCP) through Tor
/// either completes in a few seconds or not at all: measured on mainnet, 3
/// of 10 peers answered in ~2 s and 7 never did within 30 s, while Tor keeps
/// retrying a dead peer on new circuits for up to `SocksTimeout` (2 min).
/// The resolver hedges a silent attempt anyway; these bound what is held.
const P2P_TOR_CONNECT: Duration = Duration::from_secs(10);
/// `.onion` peers need a rendezvous circuit.
const P2P_ONION_CONNECT: Duration = Duration::from_secs(20);
const P2P_DIRECT_CONNECT: Duration = Duration::from_secs(4);
/// No byte for this long once connected: stalled (Tor / direct).
const P2P_TOR_STALL: Duration = Duration::from_secs(12);
const P2P_DIRECT_STALL: Duration = Duration::from_secs(6);

pub struct P2pSource {
    /// Replaced at runtime by `set_peers` (AUD-137): a list discovered once
    /// at start goes stale, or is empty if Core had no peers yet.
    peers: RwLock<Vec<PeerHint>>,
    tor_socks: Option<String>,
    /// Per-attempt limit of a direct connection.
    timeout: Duration,
    /// Per-attempt limit through Tor: a circuit to the peer plus a whole
    /// block, so it is longer than the direct one.
    tor_timeout: Duration,
    magic: [u8; 4],
    v2: bool,
    fallback_clearnet: bool,
    fallback_warned: AtomicBool,
    /// Cooldown after a hard failure (and cap of the transient backoff).
    cooldown: Duration,
    health: HealthMap,
    /// Peers that have served a block (statistics only; no priority).
    good: Mutex<HashSet<String>>,
    /// Set only when the local SOCKS proxy itself fails (AUD-153).
    proxy_down_until: Mutex<Option<Instant>>,
    /// Force one attempt when every peer is cooling down. The resolver sets
    /// it when P2P is the only external source; otherwise Esplora already
    /// forces its own attempt and a dead peer list is not paid for again.
    force_when_cooling: AtomicBool,
    stats: Option<Arc<RawtxStats>>,
}

/// Why one attempt on one peer failed.
struct Failure {
    err: RawtxError,
    /// `None`: the local proxy failed, not the peer.
    blame: Option<Blame>,
    /// The TCP (or SOCKS) connection to the peer was established.
    connected: bool,
}

impl Failure {
    fn transient(err: RawtxError) -> Self {
        Failure {
            err,
            blame: Some(Blame::Transient),
            connected: true,
        }
    }

    fn hard(err: RawtxError) -> Self {
        Failure {
            err,
            blame: Some(Blame::Hard),
            connected: true,
        }
    }
}

/// A peer socket whose every read and write is bounded by the stall time
/// and by the attempt deadline: a slow but live transfer is not cut short,
/// and a peer that trickles bytes cannot hold the call past its budget.
struct DeadlineIo {
    stream: TcpStream,
    deadline: Instant,
    stall: Duration,
    ctl: AttemptCtl,
}

impl DeadlineIo {
    fn arm(&self, write: bool) -> io::Result<()> {
        if self.ctl.aborted() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "aborted"));
        }
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "attempt deadline reached",
            ));
        }
        let limit = Some(left.min(self.stall));
        if write {
            self.stream.set_write_timeout(limit)
        } else {
            self.stream.set_read_timeout(limit)
        }
    }

    fn try_clone(&self) -> io::Result<Self> {
        Ok(DeadlineIo {
            stream: self.stream.try_clone()?,
            deadline: self.deadline,
            stall: self.stall,
            ctl: self.ctl.clone(),
        })
    }
}

impl Read for DeadlineIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.arm(false)?;
        let n = self.stream.read(buf)?;
        self.ctl.add_bytes(n);
        Ok(n)
    }
}

impl Write for DeadlineIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.arm(true)?;
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
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
    put_varstr(crate::P2P_USER_AGENT, &mut p);
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

/// Why a SOCKS5 CONNECT failed. Only `Proxy` means the local Tor is unusable;
/// a reply code or a CONNECT that never completes is about the peer or the
/// circuit, and must not stop P2P for every other peer (AUD-153).
#[derive(Debug)]
pub(crate) enum SocksError {
    /// TCP to the proxy, its greeting or its authentication failed.
    Proxy(io::Error),
    /// The proxy answered the CONNECT with a non-zero reply code.
    Reply(u8),
    /// The CONNECT was sent but its reply did not complete (slow circuit).
    Stream(io::Error),
}

impl fmt::Display for SocksError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SocksError::Proxy(e) => write!(f, "socks proxy: {e}"),
            SocksError::Reply(code) => {
                write!(f, "socks reply {code} ({})", socks_reply_name(*code))
            }
            SocksError::Stream(e) => write!(f, "socks connect: {e}"),
        }
    }
}

fn socks_reply_name(code: u8) -> &'static str {
    match code {
        1 => "general failure",
        2 => "not allowed by ruleset",
        3 => "network unreachable",
        4 => "host unreachable",
        5 => "connection refused",
        6 => "TTL expired",
        7 => "command not supported",
        8 => "address type not supported",
        _ => "unknown",
    }
}

/// Minimal SOCKS5 CONNECT with bounded connect/read/write timeouts.
///
/// The `socks` crate does not expose timeouts, so a stalled Tor SOCKS proxy
/// could block a P2P fetch indefinitely. This performs the same handshake with
/// the P2P timeout applied to every step.
fn socks_connect(
    proxy: &str,
    peer: &str,
    timeout: Duration,
) -> std::result::Result<TcpStream, SocksError> {
    let mut stream = socks_greet(proxy, timeout).map_err(SocksError::Proxy)?;
    socks_request(&mut stream, peer)?;
    Ok(stream)
}

/// Connect to the proxy and complete the method negotiation (with a random
/// username: Tor isolates streams with different credentials).
fn socks_greet(proxy: &str, timeout: Duration) -> io::Result<TcpStream> {
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
    Ok(stream)
}

/// Send the CONNECT for `peer` and read the reply.
fn socks_request(stream: &mut TcpStream, peer: &str) -> std::result::Result<(), SocksError> {
    let (host, port) = split_host_port(peer).map_err(SocksError::Stream)?;
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
            return Err(SocksError::Stream(io::Error::new(
                io::ErrorKind::InvalidInput,
                "peer host too long",
            )));
        }
        request.push(3);
        request.push(host.len() as u8);
        request.extend_from_slice(host);
    }
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).map_err(SocksError::Proxy)?;

    // From here on Tor is building a circuit to the peer: a timeout is the
    // circuit's (or the peer's), not the proxy's.
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).map_err(SocksError::Stream)?;
    if head[0] != 5 {
        return Err(SocksError::Proxy(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad SOCKS reply version",
        )));
    }
    if head[1] != 0 {
        return Err(SocksError::Reply(head[1]));
    }
    let skip = match head[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).map_err(SocksError::Stream)?;
            len[0] as usize
        }
        _ => {
            return Err(SocksError::Proxy(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad SOCKS reply address type",
            )))
        }
    };
    // Bound address and port.
    let mut rest = vec![0u8; skip + 2];
    stream.read_exact(&mut rest).map_err(SocksError::Stream)?;
    Ok(())
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
            peers: RwLock::new(peers.into_iter().map(PeerHint::from).collect()),
            tor_socks,
            timeout,
            tor_timeout: timeout,
            magic: chain.magic(),
            v2,
            fallback_clearnet,
            fallback_warned: AtomicBool::new(false),
            cooldown,
            health: HealthMap::new(),
            good: Mutex::new(HashSet::new()),
            proxy_down_until: Mutex::new(None),
            force_when_cooling: AtomicBool::new(false),
            stats: None,
        }
    }

    /// Per-attempt limit for connections made through Tor (default: the
    /// direct `timeout`).
    pub fn with_tor_timeout(mut self, tor_timeout: Duration) -> Self {
        self.tor_timeout = tor_timeout.max(Duration::from_secs(1));
        self
    }

    /// Force one attempt when every peer is cooling down (P2P is the only
    /// external source).
    pub fn set_force_when_cooling(&self, force: bool) {
        self.force_when_cooling.store(force, Ordering::Relaxed);
    }

    /// Replace the peer list. Cooldowns and statistics are kept for peers
    /// that are still listed. Returns the new peer count.
    pub fn set_peers(&self, mut list: Vec<PeerHint>) -> usize {
        let mut seen = HashSet::new();
        list.retain(|p| seen.insert(p.addr.clone()));
        let count = list.len();
        self.health.retain(|peer| seen.contains(peer));
        if let Ok(mut good) = self.good.lock() {
            good.retain(|peer| seen.contains(peer));
        }
        if let Ok(mut current) = self.peers.write() {
            *current = list;
        }
        count
    }

    pub fn peer_count(&self) -> usize {
        self.peers.read().map(|p| p.len()).unwrap_or(0)
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

    fn proxy_down(&self) -> bool {
        self.proxy_down_until
            .lock()
            .map(|until| until.is_some_and(|t| t > Instant::now()))
            .unwrap_or(false)
    }

    fn set_proxy_down(&self, down: bool) {
        if let Ok(mut until) = self.proxy_down_until.lock() {
            *until = down.then(|| Instant::now() + PROXY_PAUSE);
        }
    }

    /// Whether the next connection goes through Tor.
    fn via_tor(&self) -> bool {
        self.tor_socks.is_some() && !self.proxy_down()
    }

    /// Per-attempt limit of the next connection (Tor or direct).
    fn per_attempt(&self) -> Duration {
        if self.via_tor() {
            self.tor_timeout
        } else {
            self.timeout
        }
    }

    /// An attempt with less time left than this is not started.
    fn min_attempt(&self) -> Duration {
        crate::MIN_ATTEMPT.min(self.per_attempt())
    }

    /// Deadline and stall time of one protocol attempt inside a call.
    fn attempt(&self, call_deadline: Instant) -> (Instant, Duration) {
        let limit = self.per_attempt();
        let deadline = call_deadline.min(Instant::now() + limit);
        let stall = if self.via_tor() {
            P2P_TOR_STALL
        } else {
            P2P_DIRECT_STALL
        };
        (deadline, limit.min(stall))
    }

    /// Limit of the connection phase to `peer` (circuit + TCP).
    fn connect_limit(&self, peer: &str) -> Duration {
        let phase = if !self.via_tor() {
            P2P_DIRECT_CONNECT
        } else if peer.to_ascii_lowercase().contains(".onion") {
            P2P_ONION_CONNECT
        } else {
            P2P_TOR_CONNECT
        };
        phase.min(self.per_attempt())
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

    /// Clearnet connection used only when the operator enabled the fallback.
    fn fallback_connect(
        &self,
        peer: &str,
        timeout: Duration,
        why: &str,
    ) -> std::result::Result<TcpStream, Failure> {
        if !self.fallback_warned.swap(true, Ordering::Relaxed) {
            eprintln!(
                "[e-lite] WARNING: Tor failed for a P2P peer ({why}); using clearnet (operator IP exposed)"
            );
        }
        self.direct_connect(peer, timeout).map_err(|err| Failure {
            err,
            blame: Some(Blame::Unreachable),
            connected: false,
        })
    }

    fn connect_raw(
        &self,
        peer: &str,
        deadline: Instant,
        stall: Duration,
        ctl: &AttemptCtl,
    ) -> std::result::Result<DeadlineIo, Failure> {
        let timeout = deadline
            .saturating_duration_since(Instant::now())
            .min(self.connect_limit(peer))
            .max(Duration::from_millis(100));
        let stream = match &self.tor_socks {
            Some(proxy) if !self.proxy_down() => match socks_connect(proxy, peer, timeout) {
                Ok(s) => s,
                Err(SocksError::Proxy(e)) => {
                    // The local Tor itself failed: every peer would fail the
                    // same way, so pause P2P over Tor briefly.
                    self.bump(|stats| {
                        stats.p2p_tor_failures.fetch_add(1, Ordering::Relaxed);
                    });
                    self.set_proxy_down(true);
                    if self.fallback_clearnet {
                        self.fallback_connect(peer, timeout, "proxy")?
                    } else {
                        return Err(Failure {
                            err: RawtxError::Unavailable(format!("tor proxy: {e}")),
                            blame: None,
                            connected: false,
                        });
                    }
                }
                // The proxy works; this peer does not answer through it (a
                // reply code, or no reply within the connect limit).
                Err(e) => {
                    if self.fallback_clearnet {
                        self.fallback_connect(peer, timeout, "circuit")?
                    } else {
                        return Err(Failure {
                            err: RawtxError::Unavailable(format!("tor {peer}: {e}")),
                            blame: Some(Blame::Unreachable),
                            connected: false,
                        });
                    }
                }
            },
            Some(_) => {
                if self.fallback_clearnet {
                    self.fallback_connect(peer, timeout, "proxy paused")?
                } else {
                    return Err(Failure {
                        err: RawtxError::Unavailable(
                            "tor proxy unavailable; clearnet fallback disabled".into(),
                        ),
                        blame: None,
                        connected: false,
                    });
                }
            }
            None => self.direct_connect(peer, timeout).map_err(|err| Failure {
                err,
                blame: Some(Blame::Unreachable),
                connected: false,
            })?,
        };
        let setup = |e: io::Error| Failure::transient(RawtxError::Io(e.to_string()));
        stream.set_nodelay(true).map_err(setup)?;
        ctl.watch(&stream);
        Ok(DeadlineIo {
            stream,
            deadline,
            stall,
            ctl: ctl.clone(),
        })
    }

    fn fetch_from(
        &self,
        hint: &PeerHint,
        hash: &Hash32,
        call_deadline: Instant,
        ctl: &AttemptCtl,
    ) -> std::result::Result<Vec<u8>, Failure> {
        let peer = hint.addr.as_str();
        let mut v2_error = None;
        if self.v2 && hint.v2 != Some(false) {
            match self.fetch_from_v2(peer, hash, call_deadline, ctl) {
                Ok(bytes) => return Ok(bytes),
                // No connection (dead peer, slow circuit, Tor down), or a
                // peer that answered with wrong services or a bad block: a
                // v1 attempt would pay another connection for the same
                // result (AUD-153).
                Err(f) if !f.connected || f.blame != Some(Blame::Transient) => return Err(f),
                Err(f) => v2_error = Some(f),
            }
        }
        if call_deadline.saturating_duration_since(Instant::now()) < self.min_attempt() {
            return Err(v2_error.unwrap_or_else(|| {
                Failure::transient(RawtxError::Unavailable(format!("{peer} p2p timeout")))
            }));
        }
        if ctl.aborted() {
            return Err(v2_error.unwrap_or_else(|| {
                Failure::transient(RawtxError::Unavailable(format!("{peer} aborted")))
            }));
        }
        match self.fetch_from_v1(peer, hash, call_deadline, ctl) {
            Ok(bytes) => Ok(bytes),
            Err(mut f) => {
                if let Some(v2) = v2_error {
                    f.err = RawtxError::Unavailable(format!("{}; v1: {}", v2.err, f.err));
                }
                Err(f)
            }
        }
    }

    fn fetch_from_v1(
        &self,
        peer: &str,
        hash: &Hash32,
        call_deadline: Instant,
        ctl: &AttemptCtl,
    ) -> std::result::Result<Vec<u8>, Failure> {
        let (deadline, stall) = self.attempt(call_deadline);
        let mut stream = self.connect_raw(peer, deadline, stall, ctl)?;
        let fail = |what: &str, e: io::Error| {
            Failure::transient(RawtxError::Unavailable(format!("{peer} {what}: {e}")))
        };
        send_message(
            &mut stream,
            self.magic,
            "version",
            &version_payload(NODE_WITNESS, 0),
        )
        .map_err(|e| fail("version", e))?;

        let mut got_version = false;
        let mut services = 0u64;
        loop {
            let (cmd, payload) = read_message(&mut stream, self.magic, MAX_MESSAGE)
                .map_err(|e| fail("handshake", e))?;
            match cmd.as_str() {
                "version" => {
                    got_version = true;
                    if payload.len() >= 12 {
                        services = u64::from_le_bytes(payload[4..12].try_into().unwrap());
                    }
                    send_message(&mut stream, self.magic, "verack", &[])
                        .map_err(|e| fail("verack", e))?;
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
            .map_err(|e| fail("getdata", e))?;
        loop {
            let (cmd, payload) =
                read_message(&mut stream, self.magic, MAX_MESSAGE).map_err(|e| fail("block", e))?;
            match cmd.as_str() {
                "block" => {
                    check_block(&payload, hash, peer).map_err(Failure::hard)?;
                    return Ok(payload);
                }
                "notfound" => {
                    return Err(Failure::hard(RawtxError::Unavailable(format!(
                        "{peer} notfound for {hash}"
                    ))))
                }
                "ping" => {
                    let _ = send_message(&mut stream, self.magic, "pong", &pong_payload(&payload));
                }
                _ => {}
            }
        }
    }

    fn fetch_from_v2(
        &self,
        peer: &str,
        hash: &Hash32,
        call_deadline: Instant,
        ctl: &AttemptCtl,
    ) -> std::result::Result<Vec<u8>, Failure> {
        let (deadline, stall) = self.attempt(call_deadline);
        let stream = self.connect_raw(peer, deadline, stall, ctl)?;
        let reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| Failure::transient(RawtxError::Io(e.to_string())))?,
        );
        let writer = stream;
        let mut protocol = Protocol::new(self.magic, Role::Initiator, None, None, reader, writer)
            .map_err(|e| {
            Failure::transient(RawtxError::Unavailable(format!("{peer} v2 handshake: {e}")))
        })?;

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
                    check_block(&payload, hash, peer).map_err(Failure::hard)?;
                    return Ok(payload);
                }
                "notfound" => {
                    return Err(Failure::hard(RawtxError::Unavailable(format!(
                        "{peer} notfound for {hash}"
                    ))))
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
        protocol: &mut Protocol<BufReader<DeadlineIo>, DeadlineIo>,
        command: &str,
        payload: &[u8],
    ) -> std::result::Result<(), Failure> {
        protocol
            .write(&Payload::genuine(v2_message(command, payload)))
            .map_err(|e| Failure::transient(RawtxError::Unavailable(format!("v2 write: {e}"))))
    }

    fn v2_read(
        &self,
        protocol: &mut Protocol<BufReader<DeadlineIo>, DeadlineIo>,
        peer: &str,
    ) -> std::result::Result<(String, Vec<u8>), Failure> {
        loop {
            let packet = protocol.read().map_err(|e| {
                Failure::transient(RawtxError::Unavailable(format!("{peer} v2 read: {e}")))
            })?;
            if matches!(packet.packet_type(), PacketType::Decoy) {
                continue;
            }
            return v2_parse(packet.contents()).map_err(|e| {
                Failure::transient(RawtxError::Unavailable(format!("{peer} v2 message: {e}")))
            });
        }
    }

    /// Apply one attempt's outcome to the peer's health. Returns false when
    /// the local proxy failed (no other peer can work now).
    fn record_failure(&self, peer: &str, f: &Failure, mode: FetchMode, ctl: &AttemptCtl) -> bool {
        self.bump(|stats| {
            stats.p2p_failures.fetch_add(1, Ordering::Relaxed);
        });
        match f.blame {
            // A decoy or a losing hedge never cools a peer down (AUD-157).
            Some(blame) if mode == FetchMode::Normal && !ctl.aborted() => {
                self.health.fail(
                    peer,
                    blame,
                    PEER_TRANSIENT_BASE,
                    self.cooldown,
                    self.cooldown,
                );
                true
            }
            Some(_) => true,
            None => false,
        }
    }

    fn record_success(&self, peer: &str) {
        if let Ok(mut good) = self.good.lock() {
            good.insert(peer.to_string());
        }
        self.health.succeed(peer);
    }

    /// Peers usable now, random order, without the cooling ones; the one
    /// whose cooldown ends first when P2P is the only source.
    fn candidates(&self, mode: FetchMode) -> Vec<PeerHint> {
        let mut peers = self.peers.read().map(|p| p.clone()).unwrap_or_default();
        if peers.is_empty() {
            return peers;
        }
        if self.tor_socks.is_some() && self.proxy_down() && !self.fallback_clearnet {
            return Vec::new();
        }
        peers.shuffle(&mut thread_rng());
        // Purely random order (AUD-135): no peer is preferred because it
        // answered last; failed peers are skipped while they cool down.
        let mut candidates: Vec<PeerHint> = peers
            .iter()
            .filter(|p| !self.health.cooling(&p.addr))
            .cloned()
            .collect();
        if candidates.is_empty()
            && mode == FetchMode::Normal
            && self.force_when_cooling.load(Ordering::Relaxed)
        {
            // P2P is the only external source: try the peer whose cooldown
            // ends first rather than failing without touching the network.
            if let Some(peer) = peers.iter().min_by_key(|p| self.health.until(&p.addr)) {
                self.health.release(&peer.addr);
                candidates.push(peer.clone());
            }
        }
        candidates
    }

    fn require_network(&self, peer: &str, services: u64) -> std::result::Result<(), Failure> {
        if services & NODE_NETWORK == 0 {
            let limited = services & NODE_NETWORK_LIMITED != 0;
            return Err(Failure::hard(RawtxError::Unavailable(format!(
                "{peer} does not serve full blocks (services={services:#x}{})",
                if limited { ", limited" } else { "" }
            ))));
        }
        if services & NODE_WITNESS == 0 {
            return Err(Failure::hard(RawtxError::Unavailable(format!(
                "{peer} does not serve witness blocks (services={services:#x})"
            ))));
        }
        Ok(())
    }
}

impl BlockSource for P2pSource {
    fn name(&self) -> &str {
        "p2p"
    }

    fn fetch_block(&self, hash: &Hash32) -> Result<Vec<u8>> {
        self.fetch_block_until(
            hash,
            Instant::now() + crate::EXTERNAL_BUDGET,
            FetchMode::Normal,
        )
    }

    fn endpoints(&self, mode: FetchMode) -> Vec<Endpoint> {
        let via_tor = self.via_tor();
        self.candidates(mode)
            .into_iter()
            .take(PEERS_PER_CALL)
            .map(|p| Endpoint {
                key: p.addr,
                tier: 0,
                via_tor,
            })
            .collect()
    }

    fn fetch_endpoint(
        &self,
        endpoint: &Endpoint,
        hash: &Hash32,
        deadline: Instant,
        mode: FetchMode,
        ctl: &AttemptCtl,
    ) -> Result<Vec<u8>> {
        if self.tor_socks.is_some() && self.proxy_down() && !self.fallback_clearnet {
            return Err(RawtxError::Unavailable(
                "tor proxy unavailable; clearnet fallback disabled".into(),
            ));
        }
        let hint = self
            .peers
            .read()
            .ok()
            .and_then(|p| p.iter().find(|h| h.addr == endpoint.key).cloned())
            .unwrap_or_else(|| PeerHint::from(endpoint.key.clone()));
        let call_deadline = deadline.min(Instant::now() + self.per_attempt().saturating_mul(2));
        if call_deadline.saturating_duration_since(Instant::now()) < self.min_attempt() {
            return Err(RawtxError::Unavailable("p2p deadline reached".into()));
        }
        self.bump(|stats| {
            stats.p2p_attempts.fetch_add(1, Ordering::Relaxed);
        });
        match self.fetch_from(&hint, hash, call_deadline, ctl) {
            Ok(bytes) => {
                self.record_success(&hint.addr);
                Ok(bytes)
            }
            Err(f) => {
                self.record_failure(&hint.addr, &f, mode, ctl);
                Err(RawtxError::Unavailable(format!("{}: {}", hint.addr, f.err)))
            }
        }
    }

    fn fetch_block_until(
        &self,
        hash: &Hash32,
        deadline: Instant,
        mode: FetchMode,
    ) -> Result<Vec<u8>> {
        if self.peers.read().map(|p| p.is_empty()).unwrap_or(true) {
            return Err(RawtxError::Unavailable("no p2p peers".into()));
        }
        if self.tor_socks.is_some() && self.proxy_down() && !self.fallback_clearnet {
            return Err(RawtxError::Unavailable(
                "tor proxy unavailable; clearnet fallback disabled".into(),
            ));
        }
        let candidates = self.candidates(mode);
        if candidates.is_empty() {
            self.bump(|stats| {
                stats.p2p_skipped.fetch_add(1, Ordering::Relaxed);
            });
            return Err(RawtxError::Unavailable(
                "all p2p peers are cooling down".into(),
            ));
        }

        // One call gets at most two attempt budgets, so a slow peer list
        // leaves time for the other sources.
        let call_deadline = deadline.min(Instant::now() + self.per_attempt().saturating_mul(2));
        let ctl = AttemptCtl::new();
        let mut errors = Vec::new();
        for hint in candidates.into_iter().take(PEERS_PER_CALL) {
            if call_deadline.saturating_duration_since(Instant::now()) < self.min_attempt() {
                errors.push("p2p deadline reached".to_string());
                break;
            }
            self.bump(|stats| {
                stats.p2p_attempts.fetch_add(1, Ordering::Relaxed);
            });
            match self.fetch_from(&hint, hash, call_deadline, &ctl) {
                Ok(bytes) => {
                    self.record_success(&hint.addr);
                    return Ok(bytes);
                }
                Err(f) => {
                    errors.push(format!("{}: {}", hint.addr, f.err));
                    if !self.record_failure(&hint.addr, &f, mode, &ctl) {
                        // The local proxy failed: no other peer can work now.
                        break;
                    }
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
    use std::sync::Arc;

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
        // AUD-133: no e-lite-specific user agent on the wire.
        let ua = crate::P2P_USER_AGENT;
        assert!(payload.windows(ua.len()).any(|w| w == ua));
        assert!(!payload.windows(6).any(|w| w == b"e-lite"));
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

    /// SOCKS5 proxy that completes the greeting and answers every CONNECT
    /// with `reply` (5 = connection refused). Counts connections.
    fn refusing_socks(reply: u8) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&count);
        std::thread::spawn(move || {
            while let Ok((mut sock, _)) = listener.accept() {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut greeting = [0u8; 4];
                if sock.read_exact(&mut greeting).is_err() {
                    continue;
                }
                let _ = sock.write_all(&[5, 2]);
                let mut head = [0u8; 2];
                let _ = sock.read_exact(&mut head);
                let mut user = vec![0u8; head[1] as usize];
                let _ = sock.read_exact(&mut user);
                let mut plen = [0u8; 1];
                let _ = sock.read_exact(&mut plen);
                let mut pass = vec![0u8; plen[0] as usize];
                let _ = sock.read_exact(&mut pass);
                let _ = sock.write_all(&[1, 0]);
                // CONNECT to an IPv4 peer: 4 + 4 + 2 bytes.
                let mut request = [0u8; 10];
                let _ = sock.read_exact(&mut request);
                let _ = sock.write_all(&[5, reply, 0, 1, 0, 0, 0, 0, 0, 0]);
            }
        });
        (addr, count)
    }

    #[test]
    fn a_refusing_peer_does_not_stop_the_others() {
        // AUD-153: a SOCKS reply about one peer (refused, exit policy) must
        // not trip the Tor breaker, and the v2 attempt that could not even
        // connect is not repeated as v1.
        let (socks, connections) = refusing_socks(5);
        let peers = vec![
            "192.0.2.1:18444".to_string(),
            "192.0.2.2:18444".to_string(),
            "192.0.2.3:18444".to_string(),
        ];
        let src = P2pSource::new(
            peers.clone(),
            Some(socks),
            Duration::from_secs(2),
            Chain::Regtest,
            true,
            false,
            Duration::from_secs(60),
        );
        let err = src.fetch_block(&Hash32([0x51; 32])).unwrap_err();
        assert!(format!("{err}").contains("connection refused"), "{err}");
        assert_eq!(
            connections.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "every peer tried once, no v1 retry after a failed connection"
        );
        assert!(!src.proxy_down(), "a peer refusal is not a Tor failure");
        assert!(peers.iter().all(|p| src.health.cooling(p)));
    }

    #[test]
    fn a_dead_proxy_pauses_tor_without_blaming_peers() {
        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let socks = closed.local_addr().unwrap().to_string();
        drop(closed);
        let stats = Arc::new(RawtxStats::default());
        let peers = vec!["192.0.2.1:18444".to_string(), "192.0.2.2:18444".to_string()];
        let src = P2pSource::new(
            peers.clone(),
            Some(socks),
            Duration::from_secs(2),
            Chain::Regtest,
            false,
            false,
            Duration::from_secs(60),
        )
        .with_stats(Arc::clone(&stats));
        assert!(src.fetch_block(&Hash32([0x52; 32])).is_err());
        assert!(src.proxy_down());
        assert_eq!(
            stats.p2p_attempts.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(peers.iter().all(|p| !src.health.cooling(p)));
    }

    #[test]
    fn the_only_source_forces_one_attempt_while_cooling() {
        use std::sync::atomic::AtomicUsize;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let accepts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepts);
        std::thread::spawn(move || {
            while let Ok((sock, _)) = listener.accept() {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                drop(sock);
            }
        });
        let src = P2pSource::new(
            vec![addr],
            None,
            Duration::from_millis(500),
            Chain::Regtest,
            false,
            false,
            Duration::from_secs(60),
        );
        src.set_force_when_cooling(true);
        assert!(src.fetch_block(&Hash32([0x53; 32])).is_err());
        assert!(src.fetch_block(&Hash32([0x54; 32])).is_err());
        assert_eq!(accepts.load(std::sync::atomic::Ordering::SeqCst), 2);
        // A decoy never forces anything.
        assert!(src
            .fetch_block_until(
                &Hash32([0x55; 32]),
                Instant::now() + Duration::from_secs(2),
                FetchMode::Quiet
            )
            .is_err());
        assert_eq!(accepts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn a_v1_peer_hint_skips_the_v2_attempt() {
        let block = test_block();
        let bytes = bitcoin::consensus::serialize(&block);
        let hash = Hash32::from_hex(&block.block_hash().to_string()).unwrap();
        // The mock answers one connection only, in v1: a v2 attempt would use
        // it up and fail.
        let addr = mock_peer_v1(NODE_NETWORK | NODE_WITNESS, bytes.clone(), false);
        let src = P2pSource::new(
            Vec::new(),
            None,
            Duration::from_secs(5),
            Chain::Regtest,
            true,
            false,
            Duration::from_secs(5),
        );
        src.set_peers(vec![PeerHint {
            addr,
            v2: Some(false),
        }]);
        assert_eq!(src.fetch_block(&hash).unwrap(), bytes);
    }

    #[test]
    fn a_trickling_peer_is_bounded_by_the_attempt() {
        // The read deadline bounds a peer that sends a byte per second.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                for _ in 0..30 {
                    if sock.write_all(&[0]).is_err() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(300));
                }
            }
        });
        let src = P2pSource::new(
            vec![addr],
            None,
            Duration::from_secs(1),
            Chain::Regtest,
            false,
            false,
            Duration::from_secs(5),
        );
        let started = Instant::now();
        assert!(src.fetch_block(&Hash32([0x56; 32])).is_err());
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "{:?}",
            started.elapsed()
        );
    }
}
