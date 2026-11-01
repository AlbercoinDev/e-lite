//! Minimal Bitcoin Core JSON-RPC client (loopback, cookie auth, no TLS).
//!
//! Implemented directly over `std::net` so the indexer stays synchronous and the
//! dependency surface stays small. Core remains the authority for the active
//! chain; this client never invents data.

use el_model::Chain;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug)]
pub struct CoreError(pub String);

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "core error: {}", self.0)
    }
}

impl std::error::Error for CoreError {}

impl From<std::io::Error> for CoreError {
    fn from(e: std::io::Error) -> Self {
        CoreError(format!("io: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;

fn err<T>(m: impl Into<String>) -> Result<T> {
    Err(CoreError(m.into()))
}

#[derive(Clone, Debug)]
pub struct CoreConfig {
    pub rpc_url: String,
    pub cookie_file: Option<PathBuf>,
    pub core_datadir: Option<PathBuf>,
    pub chain: Chain,
    pub timeout: Duration,
}

impl CoreConfig {
    pub fn resolve_cookie(&self) -> Result<PathBuf> {
        if let Some(p) = &self.cookie_file {
            return Ok(p.clone());
        }
        match &self.core_datadir {
            Some(d) => {
                let sub = self.chain.core_subdir();
                let p = if sub.is_empty() {
                    d.join(".cookie")
                } else {
                    d.join(sub).join(".cookie")
                };
                Ok(p)
            }
            None => err("neither cookie_file nor core_datadir configured"),
        }
    }
}

#[derive(Clone)]
pub struct CoreClient {
    host: String,
    port: u16,
    auth: std::sync::Arc<std::sync::Mutex<String>>,
    cookie_path: Option<PathBuf>,
    timeout: Duration,
}

impl CoreClient {
    pub fn connect(cfg: &CoreConfig) -> Result<Self> {
        let (host, port) = parse_url(&cfg.rpc_url)?;
        let cookie_path = cfg.resolve_cookie()?;
        let cookie = std::fs::read_to_string(&cookie_path).map_err(|e| {
            CoreError(format!(
                "cannot read Core cookie {}: {e}",
                cookie_path.display()
            ))
        })?;
        let auth = cookie.trim().to_string();
        if auth.is_empty() {
            return err("Core cookie is empty");
        }
        Ok(CoreClient {
            host,
            port,
            auth: std::sync::Arc::new(std::sync::Mutex::new(auth)),
            cookie_path: Some(cookie_path),
            timeout: cfg.timeout,
        })
    }

    fn post(&self, body: &str) -> Result<Vec<u8>> {
        let addr = format!("{}:{}", self.host, self.port);
        let sockaddr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| CoreError(format!("cannot resolve {addr}")))?;
        let mut stream = TcpStream::connect_timeout(&sockaddr, self.timeout)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;

        let auth = self
            .auth
            .lock()
            .map_err(|_| CoreError("core auth lock poisoned".into()))?
            .clone();
        let req = format!(
            "POST / HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Basic {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            base64(auth.as_bytes()),
            body.len(),
            body
        );
        stream.write_all(req.as_bytes())?;
        stream.flush()?;
        read_http_body(&mut stream)
    }

    /// Core rotates its cookie on restart; reload it before retrying a 401.
    fn refresh_auth(&self) -> Result<()> {
        let Some(path) = &self.cookie_path else {
            return err("core cookie path is unknown");
        };
        let cookie = std::fs::read_to_string(path)
            .map_err(|e| CoreError(format!("cannot reload Core cookie {}: {e}", path.display())))?;
        let cookie = cookie.trim().to_string();
        if cookie.is_empty() {
            return err("Core cookie is empty");
        }
        *self
            .auth
            .lock()
            .map_err(|_| CoreError("core auth lock poisoned".into()))? = cookie;
        Ok(())
    }

    fn call<T: DeserializeOwned>(&self, method: &str, params: serde_json::Value) -> Result<T> {
        let body = serde_json::json!({
            "jsonrpc": "1.0",
            "id": "e-lite",
            "method": method,
            "params": params,
        })
        .to_string();
        let bytes = match self.post(&body) {
            Ok(b) => b,
            Err(e) if e.0.contains("401") => {
                self.refresh_auth()?;
                self.post(&body)?
            }
            Err(e) => return Err(e),
        };
        let parsed: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| CoreError(format!("invalid JSON from Core: {e}")))?;
        if !parsed["error"].is_null() {
            return err(format!(
                "{} failed: {} (code {})",
                method, parsed["error"]["message"], parsed["error"]["code"]
            ));
        }
        serde_json::from_value(parsed["result"].clone())
            .map_err(|e| CoreError(format!("cannot decode {method} result: {e}")))
    }

    /// JSON-RPC batch: one HTTP request for many calls of the same method.
    pub fn batch(
        &self,
        method: &str,
        params_list: Vec<serde_json::Value>,
    ) -> Result<Vec<std::result::Result<serde_json::Value, String>>> {
        if params_list.is_empty() {
            return Ok(Vec::new());
        }
        let requests: Vec<serde_json::Value> = params_list
            .iter()
            .enumerate()
            .map(|(i, p)| {
                serde_json::json!({
                    "jsonrpc": "1.0",
                    "id": i,
                    "method": method,
                    "params": p,
                })
            })
            .collect();
        let body = serde_json::to_string(&requests)
            .map_err(|e| CoreError(format!("cannot encode batch: {e}")))?;
        let bytes = match self.post(&body) {
            Ok(b) => b,
            Err(e) if e.0.contains("401") => {
                self.refresh_auth()?;
                self.post(&body)?
            }
            Err(e) => return Err(e),
        };
        let parsed: Vec<serde_json::Value> = serde_json::from_slice(&bytes)
            .map_err(|e| CoreError(format!("invalid batch JSON from Core: {e}")))?;
        let mut out: Vec<std::result::Result<serde_json::Value, String>> =
            vec![Err("missing batch result".to_string()); params_list.len()];
        for item in parsed {
            let id = item["id"].as_u64().unwrap_or(u64::MAX) as usize;
            if id >= out.len() {
                continue;
            }
            if item["error"].is_null() {
                out[id] = Ok(item["result"].clone());
            } else {
                out[id] = Err(item["error"]["message"]
                    .as_str()
                    .unwrap_or("core error")
                    .to_string());
            }
        }
        Ok(out)
    }

    pub fn network_info(&self) -> Result<NetworkInfo> {
        self.call("getnetworkinfo", serde_json::json!([]))
    }

    pub fn blockchain_info(&self) -> Result<BlockchainInfo> {
        self.call("getblockchaininfo", serde_json::json!([]))
    }

    pub fn block_hash(&self, height: u64) -> Result<String> {
        self.call("getblockhash", serde_json::json!([height]))
    }

    /// Raw block hex with `verbosity=0`.
    pub fn block_hex(&self, hash: &str) -> Result<String> {
        self.call("getblock", serde_json::json!([hash, 0]))
    }

    pub fn raw_transaction_hex(&self, txid: &str, blockhash: &str) -> Result<String> {
        self.call(
            "getrawtransaction",
            serde_json::json!([txid, false, blockhash]),
        )
    }

    /// Raw transaction without requiring a block hash (works for mempool txs).
    pub fn raw_transaction_hex_opt(&self, txid: &str, blockhash: Option<&str>) -> Result<String> {
        let params = match blockhash {
            Some(h) => serde_json::json!([txid, false, h]),
            None => serde_json::json!([txid, false]),
        };
        self.call("getrawtransaction", params)
    }

    /// Raw transactions for a batch of txids (mempool or txindex). Missing
    /// transactions yield `None` instead of failing the whole batch.
    pub fn batch_raw_transactions(&self, txids: &[String]) -> Result<Vec<Option<String>>> {
        let params: Vec<serde_json::Value> = txids.iter().map(|t| serde_json::json!([t])).collect();
        let results = self.batch("getrawtransaction", params)?;
        Ok(results
            .into_iter()
            .map(|r| r.ok().and_then(|v| v.as_str().map(|s| s.to_string())))
            .collect())
    }

    /// Verbose transaction (for Electrum `transaction.get(..., true)`).
    pub fn raw_transaction_json(
        &self,
        txid: &str,
        blockhash: Option<&str>,
    ) -> Result<serde_json::Value> {
        let params = match blockhash {
            Some(h) => serde_json::json!([txid, true, h]),
            None => serde_json::json!([txid, true]),
        };
        self.call("getrawtransaction", params)
    }

    pub fn send_raw_transaction(&self, tx_hex: &str) -> Result<String> {
        self.call("sendrawtransaction", serde_json::json!([tx_hex]))
    }

    pub fn raw_mempool_verbose(&self) -> Result<std::collections::HashMap<String, MempoolEntry>> {
        self.call("getrawmempool", serde_json::json!([true]))
    }

    pub fn mempool_info(&self) -> Result<MempoolInfo> {
        self.call("getmempoolinfo", serde_json::json!([]))
    }

    pub fn estimate_smart_fee(&self, target: u32) -> Result<EstimateSmartFee> {
        self.call("estimatesmartfee", serde_json::json!([target]))
    }

    pub fn block_stats(&self, height: u64) -> Result<serde_json::Value> {
        self.call("getblockstats", serde_json::json!([height]))
    }

    /// Ask Core to prune block files up to (not including) `height`.
    /// Returns the height Core actually pruned to.
    pub fn prune_blockchain(&self, height: u64) -> Result<i64> {
        self.call("pruneblockchain", serde_json::json!([height]))
    }

    pub fn set_network_active(&self, active: bool) -> Result<bool> {
        self.call("setnetworkactive", serde_json::json!([active]))
    }

    /// Peer addresses known to the local Core (no public DNS lookup).
    pub fn node_addresses(&self, count: u32) -> Result<Vec<NodeAddress>> {
        self.call("getnodeaddresses", serde_json::json!([count]))
    }

    pub fn mempool_txids(&self) -> Result<Vec<String>> {
        self.call("getrawmempool", serde_json::json!([false]))
    }

    pub fn ping(&self) -> Result<()> {
        let _: serde_json::Value = self.call("getblockcount", serde_json::json!([]))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MempoolFees {
    #[serde(default)]
    pub base: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MempoolEntry {
    #[serde(default)]
    pub vsize: u64,
    #[serde(default)]
    pub fees: Option<MempoolFees>,
    #[serde(default)]
    pub depends: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MempoolInfo {
    #[serde(default)]
    pub mempoolminfee: f64,
    #[serde(default)]
    pub minrelaytxfee: f64,
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EstimateSmartFee {
    #[serde(default)]
    pub feerate: Option<f64>,
    #[serde(default)]
    pub blocks: Option<u32>,
    #[serde(default)]
    pub errors: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NodeAddress {
    pub address: String,
    pub port: u16,
    #[serde(default)]
    pub services: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NetworkInfo {
    pub version: u64,
    #[serde(default)]
    pub subversion: String,
    #[serde(default)]
    pub relayfee: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BlockchainInfo {
    pub chain: String,
    pub blocks: u64,
    pub headers: u64,
    #[serde(default)]
    pub pruned: bool,
    #[serde(default)]
    pub pruneheight: u64,
    #[serde(default)]
    pub initialblockdownload: bool,
    #[serde(default)]
    pub signet_challenge: Option<String>,
    #[serde(default)]
    pub verificationprogress: f64,
}

fn parse_url(url: &str) -> Result<(String, u16)> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        CoreError(format!(
            "unsupported RPC URL (need http://127.0.0.1:port): {url}"
        ))
    })?;
    let rest = rest.trim_end_matches('/');
    if rest.contains('/') {
        return err("RPC URL must not contain a path");
    }
    let (host, port) = if let Some((h, p)) = rest.rsplit_once(':') {
        (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| CoreError("bad RPC port".into()))?,
        )
    } else {
        (rest.to_string(), 8332)
    };
    Ok((host, port))
}

fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Read exactly one HTTP response without waiting for the peer to close.
/// Core keeps connections open briefly after the body, so reading to EOF would
/// add ~50 ms per RPC call on loopback.
fn read_http_body(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return err("Core closed the connection without a response");
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 64 * 1024 {
            return err("HTTP headers from Core are too large");
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
    let status_ok = headers
        .lines()
        .next()
        .map(|l| l.contains(" 200 ") || l.ends_with(" 200"))
        .unwrap_or(false);
    let mut body = buf[header_end + 4..].to_vec();

    if headers.contains("transfer-encoding: chunked") {
        loop {
            if let Ok(decoded) = decode_chunked(&body) {
                body = decoded;
                break;
            }
            let n = stream.read(&mut tmp)?;
            if n == 0 {
                return err("truncated chunked body from Core");
            }
            body.extend_from_slice(&tmp[..n]);
        }
    } else if let Some(len) = content_length(&headers) {
        while body.len() < len {
            let n = stream.read(&mut tmp)?;
            if n == 0 {
                return err("truncated HTTP body from Core");
            }
            body.extend_from_slice(&tmp[..n]);
        }
        body.truncate(len);
    } else {
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest)?;
        body.extend_from_slice(&rest);
    }

    if !status_ok {
        let text = String::from_utf8_lossy(&body);
        let status_line = headers.lines().next().unwrap_or("").trim().to_string();
        return err(format!(
            "HTTP error from Core ({status_line}): {}",
            text.trim()
        ));
    }
    Ok(body)
}

fn content_length(headers: &str) -> Option<usize> {
    headers.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        if k.trim() == "content-length" {
            v.trim().parse().ok()
        } else {
            None
        }
    })
}

#[cfg(test)]
fn decode_http_body(resp: &[u8]) -> Result<Vec<u8>> {
    let sep = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| CoreError("malformed HTTP response (no header terminator)".into()))?;
    let headers = String::from_utf8_lossy(&resp[..sep]).to_ascii_lowercase();
    let status_ok = headers
        .lines()
        .next()
        .map(|l| l.contains(" 200 ") || l.ends_with(" 200"))
        .unwrap_or(false);
    let mut body = resp[sep + 4..].to_vec();
    if headers.contains("transfer-encoding: chunked") {
        body = decode_chunked(&body)?;
    }
    if !status_ok {
        let text = String::from_utf8_lossy(&body);
        let status_line = headers.lines().next().unwrap_or("").trim().to_string();
        return err(format!(
            "HTTP error from Core ({status_line}): {}",
            text.trim()
        ));
    }
    Ok(body)
}

fn decode_chunked(buf: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    loop {
        let line_end = buf[pos..]
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| CoreError("truncated chunked body".into()))?
            + pos;
        let line = std::str::from_utf8(&buf[pos..line_end])
            .map_err(|_| CoreError("bad chunk header".into()))?;
        let size_hex = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| CoreError(format!("bad chunk size: {size_hex}")))?;
        pos = line_end + 2;
        if size == 0 {
            break;
        }
        if pos + size > buf.len() {
            return err("truncated chunk data");
        }
        out.extend_from_slice(&buf[pos..pos + size]);
        pos += size + 2;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"user:pass"), "dXNlcjpwYXNz");
    }

    #[test]
    fn parses_url() {
        assert_eq!(
            parse_url("http://127.0.0.1:18443").unwrap(),
            ("127.0.0.1".to_string(), 18443)
        );
        assert!(parse_url("https://127.0.0.1:18443").is_err());
        assert!(parse_url("http://127.0.0.1:18443/wallet/x").is_err());
    }

    #[test]
    fn decodes_chunked_and_content_length() {
        let plain = b"HTTP/1.1 200 OK\r\nContent-Length: 15\r\n\r\n{\"result\":null}";
        assert_eq!(decode_http_body(plain).unwrap(), b"{\"result\":null}");

        let chunked =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n6\r\n{\"resu\r\n9\r\nlt\":null}\r\n0\r\n\r\n";
        assert_eq!(decode_http_body(chunked).unwrap(), b"{\"result\":null}");

        let bad = b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 4\r\n\r\nnope";
        assert!(decode_http_body(bad).is_err());
    }

    #[test]
    fn talks_to_a_stub_core() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).unwrap();
            let body = r#"{"result":{"chain":"regtest","blocks":101,"headers":101},"error":null,"id":"e-lite"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).unwrap();
        });
        let cookie = std::env::temp_dir().join(format!("e-lite-cookie-{}", std::process::id()));
        std::fs::write(&cookie, "u:p").unwrap();
        let cfg = CoreConfig {
            rpc_url: format!("http://127.0.0.1:{port}"),
            cookie_file: Some(cookie.clone()),
            core_datadir: None,
            chain: Chain::Regtest,
            timeout: Duration::from_secs(5),
        };
        let client = CoreClient::connect(&cfg).unwrap();
        let info = client.blockchain_info().unwrap();
        assert_eq!(info.chain, "regtest");
        assert_eq!(info.blocks, 101);
        handle.join().unwrap();
        let _ = std::fs::remove_file(&cookie);
    }
}
