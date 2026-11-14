//! Public Electrum servers as a source of loose transactions (AUD-163).
//!
//! One call is one short session: a fresh Tor circuit (random SOCKS username),
//! TLS when the server entry ends in `:s`, `server.version`, then the txids as
//! one JSON-RPC batch of `blockchain.transaction.get` (or pipelined single
//! requests for servers without batches). The session is closed afterwards,
//! so a server sees at most `privacy.toml [sessions] size` txids together.
//!
//! The TLS certificate is not validated: TLS only hides the request from the
//! Tor exit, and every transaction is verified against the local index.

use crate::health::{Blame, HealthMap, TRANSIENT_BASE, TRANSIENT_CAP};
use crate::p2p::{socks_connect, split_host_port};
use crate::txsource::{TxBatch, TxSource, MAX_TX_BYTES};
use crate::{AttemptCtl, RawtxError, Result};
use el_model::Hash32;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A generic client name: never "e-lite" (AUD-133).
const CLIENT_NAME: &str = "4.5.5";
/// Longest response line accepted (a batch of 500 large transactions).
const MAX_LINE: u64 = 64 * 1024 * 1024;

/// One configured server: `host:port:s` (TLS) or `host:port:t` (plain;
/// normally a `.onion`, where Tor already encrypts end to end).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElectrumServer {
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

impl ElectrumServer {
    pub fn parse(entry: &str) -> std::result::Result<Self, String> {
        let entry = entry.trim();
        let (addr, proto) = match entry.rsplit_once(':') {
            Some((a, "s")) => (a, true),
            Some((a, "t")) => (a, false),
            _ => return Err(format!("{entry}: expected host:port:s or host:port:t")),
        };
        let (host, port) = split_host_port(addr).map_err(|e| format!("{entry}: {e}"))?;
        if host.is_empty() {
            return Err(format!("{entry}: empty host"));
        }
        Ok(ElectrumServer {
            host: host.to_string(),
            port,
            tls: proto,
        })
    }

    pub fn key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.host,
            self.port,
            if self.tls { "s" } else { "t" }
        )
    }

    pub fn is_onion(&self) -> bool {
        self.host.to_ascii_lowercase().ends_with(".onion")
    }
}

pub struct ElectrumTxSource {
    servers: Vec<ElectrumServer>,
    tor_proxy: Option<String>,
    allow_clearnet: bool,
    connect: Duration,
    stall: Duration,
    cooldown: Duration,
    health: HealthMap,
    /// Servers that answered a batch with an error: pipelined requests.
    no_batch: Mutex<HashSet<String>>,
    tls: Arc<rustls::ClientConfig>,
    /// Shared pool of connections for the whole per-txid path (AUD-167).
    conns: Arc<crate::Semaphore>,
}

impl ElectrumTxSource {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        entries: &[String],
        tor_proxy: Option<String>,
        allow_clearnet: bool,
        connect: Duration,
        stall: Duration,
        cooldown: Duration,
        conns: Arc<crate::Semaphore>,
    ) -> Option<Self> {
        let mut servers = Vec::new();
        for entry in entries {
            match ElectrumServer::parse(entry) {
                Ok(s) => {
                    let reachable = tor_proxy.is_some() || (allow_clearnet && !s.is_onion());
                    if reachable && !servers.contains(&s) {
                        servers.push(s);
                    }
                }
                Err(e) => eprintln!("[e-lite] WARNING: ignoring electrum server: {e}"),
            }
        }
        if servers.is_empty() {
            return None;
        }
        Some(ElectrumTxSource {
            servers,
            tor_proxy,
            allow_clearnet,
            connect,
            stall,
            cooldown,
            health: HealthMap::new(),
            no_batch: Mutex::new(HashSet::new()),
            tls: Arc::new(insecure_tls_config()),
            conns,
        })
    }

    fn server(&self, key: &str) -> Option<&ElectrumServer> {
        self.servers.iter().find(|s| s.key() == key)
    }

    fn connect_tcp(&self, s: &ElectrumServer) -> std::result::Result<TcpStream, Blame> {
        let addr = format!("{}:{}", s.host, s.port);
        let factor = if s.is_onion() { 2 } else { 1 };
        let limit = self.connect * factor;
        match &self.tor_proxy {
            Some(proxy) => socks_connect(proxy, &addr, limit).map_err(|_| Blame::Unreachable),
            None if self.allow_clearnet => {
                let addrs: Vec<_> = addr
                    .to_socket_addrs()
                    .map_err(|_| Blame::Unreachable)?
                    .collect();
                for a in addrs {
                    if let Ok(t) = TcpStream::connect_timeout(&a, limit) {
                        return Ok(t);
                    }
                }
                Err(Blame::Unreachable)
            }
            None => Err(Blame::Unreachable),
        }
    }

    fn session(
        &self,
        s: &ElectrumServer,
        txids: &[Hash32],
        deadline: Instant,
        ctl: &AttemptCtl,
    ) -> std::result::Result<TxBatch, Blame> {
        // One connection per session, from the shared pool (AUD-167).
        let Some(_permit) = self.conns.acquire_until(deadline, 0) else {
            return Err(Blame::Transient);
        };
        let tcp = self.connect_tcp(s)?;
        ctl.watch(&tcp);
        let stall = self.stall * if s.is_onion() { 2 } else { 1 };
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Blame::Transient);
        }
        let _ = tcp.set_read_timeout(Some(stall.min(left)));
        let _ = tcp.set_write_timeout(Some(stall.min(left)));
        let _ = tcp.set_nodelay(true);
        if s.tls {
            let name =
                rustls::pki_types::ServerName::try_from(s.host.clone()).map_err(|_| Blame::Hard)?;
            let conn = rustls::ClientConnection::new(Arc::clone(&self.tls), name)
                .map_err(|_| Blame::Hard)?;
            let stream = rustls::StreamOwned::new(conn, tcp);
            self.talk(s, stream, txids, deadline, ctl)
        } else {
            self.talk(s, tcp, txids, deadline, ctl)
        }
    }

    fn talk<S: Read + Write>(
        &self,
        s: &ElectrumServer,
        stream: S,
        txids: &[Hash32],
        deadline: Instant,
        ctl: &AttemptCtl,
    ) -> std::result::Result<TxBatch, Blame> {
        let mut rw = BufReader::new(stream);
        send(
            rw.get_mut(),
            &json!({"jsonrpc": "2.0", "id": 0, "method": "server.version", "params": [CLIENT_NAME, "1.4"]}),
        )?;
        let ver = read_json(&mut rw, deadline, ctl)?;
        if ver.get("result").is_none() {
            return Err(Blame::Hard);
        }
        let batch = !self
            .no_batch
            .lock()
            .map(|n| n.contains(&s.key()))
            .unwrap_or(false);
        let request = |i: usize, t: &Hash32| json!({"jsonrpc": "2.0", "id": i + 1, "method": "blockchain.transaction.get", "params": [t.to_hex()]});
        let mut out = TxBatch::new();
        if batch {
            let items: Vec<Value> = txids
                .iter()
                .enumerate()
                .map(|(i, t)| request(i, t))
                .collect();
            send(rw.get_mut(), &Value::Array(items))?;
            let resp = read_json(&mut rw, deadline, ctl)?;
            match resp {
                Value::Array(items) => {
                    for item in items {
                        collect(&item, txids, &mut out);
                    }
                    return Ok(out);
                }
                _ => {
                    // No batches here: remember and fall back to pipelining
                    // on the same connection.
                    if let Ok(mut n) = self.no_batch.lock() {
                        n.insert(s.key());
                    }
                }
            }
        }
        for (i, t) in txids.iter().enumerate() {
            send(rw.get_mut(), &request(i, t))?;
        }
        for _ in 0..txids.len() {
            let item = read_json(&mut rw, deadline, ctl)?;
            collect(&item, txids, &mut out);
        }
        Ok(out)
    }
}

/// Store `item`'s hex result under the txid its id points to.
fn collect(item: &Value, txids: &[Hash32], out: &mut TxBatch) {
    let Some(id) = item.get("id").and_then(Value::as_u64) else {
        return;
    };
    let Some(txid) = (id as usize).checked_sub(1).and_then(|i| txids.get(i)) else {
        return;
    };
    if let Some(hex) = item.get("result").and_then(Value::as_str) {
        if (hex.len() as u64) / 2 <= MAX_TX_BYTES {
            if let Ok(bytes) = hex::decode(hex) {
                out.insert(*txid, bytes);
            }
        }
    }
}

fn send<W: Write>(w: &mut W, v: &Value) -> std::result::Result<(), Blame> {
    let mut line = serde_json::to_vec(v).map_err(|_| Blame::Hard)?;
    line.push(b'\n');
    w.write_all(&line).map_err(|_| Blame::Transient)?;
    w.flush().map_err(|_| Blame::Transient)
}

fn read_json<R: BufRead>(
    r: &mut R,
    deadline: Instant,
    ctl: &AttemptCtl,
) -> std::result::Result<Value, Blame> {
    if ctl.aborted() || Instant::now() >= deadline {
        return Err(Blame::Transient);
    }
    let mut line = Vec::new();
    let n = r
        .by_ref()
        .take(MAX_LINE)
        .read_until(b'\n', &mut line)
        .map_err(|_| Blame::Transient)?;
    if n == 0 {
        return Err(Blame::Transient);
    }
    ctl.add_bytes(n);
    serde_json::from_slice(&line).map_err(|_| Blame::Hard)
}

impl TxSource for ElectrumTxSource {
    fn name(&self) -> &str {
        "electrum"
    }

    fn servers(&self) -> Vec<String> {
        self.servers
            .iter()
            .map(ElectrumServer::key)
            .filter(|k| !self.health.cooling(k))
            .collect()
    }

    fn all_servers(&self) -> Vec<String> {
        self.servers.iter().map(ElectrumServer::key).collect()
    }

    fn via_tor(&self, _server: &str) -> bool {
        self.tor_proxy.is_some()
    }

    fn fetch_from(
        &self,
        server: &str,
        txids: &[Hash32],
        deadline: Instant,
        ctl: &AttemptCtl,
    ) -> Result<TxBatch> {
        let s = self
            .server(server)
            .ok_or_else(|| RawtxError::Unavailable(format!("unknown electrum server {server}")))?;
        match self.session(s, txids, deadline, ctl) {
            Ok(batch) => {
                if !batch.is_empty() {
                    self.health.succeed(server);
                }
                Ok(batch)
            }
            Err(blame) => {
                if !ctl.aborted() {
                    self.health
                        .fail(server, blame, TRANSIENT_BASE, TRANSIENT_CAP, self.cooldown);
                }
                Err(RawtxError::Unavailable(format!(
                    "{server}: {}",
                    match blame {
                        Blame::Unreachable => "no connection",
                        Blame::Hard => "bad answer",
                        _ => "session failed",
                    }
                )))
            }
        }
    }

    fn penalize(&self, server: &str) {
        self.health.fail(
            server,
            Blame::Hard,
            TRANSIENT_BASE,
            TRANSIENT_CAP,
            self.cooldown,
        );
    }
}

/// TLS without certificate validation (signatures are still checked with
/// the presented key, so the session is a real TLS session).
fn insecure_tls_config() -> rustls::ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(NoCertCheck(Arc::clone(&provider)));
    rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth()
}

#[derive(Debug)]
struct NoCertCheck(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for NoCertCheck {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_entries_parse() {
        let s = ElectrumServer::parse("electrum.example.org:50002:s").unwrap();
        assert_eq!((s.port, s.tls), (50002, true));
        assert_eq!(s.key(), "electrum.example.org:50002:s");
        let o = ElectrumServer::parse("abcdefgh.onion:50001:t").unwrap();
        assert!(o.is_onion() && !o.tls);
        assert!(ElectrumServer::parse("host:50002").is_err());
        assert!(ElectrumServer::parse(":50002:s").is_err());
    }
}
