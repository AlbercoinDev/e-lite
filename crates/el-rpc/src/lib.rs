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
    /// Explicit RPC credentials (Core's `rpcuser`/`rpcpassword` or `rpcauth`).
    pub rpc_user: Option<String>,
    pub rpc_password: Option<String>,
    pub chain: Chain,
    pub timeout: Duration,
}

impl CoreConfig {
    /// `None` means "use rpc_user/rpc_password instead of a cookie".
    pub fn resolve_cookie(&self) -> Result<Option<PathBuf>> {
        if self.rpc_user.is_some() && self.rpc_password.is_some() {
            return Ok(None);
        }
        if let Some(p) = &self.cookie_file {
            return Ok(Some(p.clone()));
        }
        match &self.core_datadir {
            Some(d) => {
                let sub = self.chain.core_subdir();
                let p = if sub.is_empty() {
                    d.join(".cookie")
                } else {
                    d.join(sub).join(".cookie")
                };
                Ok(Some(p))
            }
            None => err("neither cookie_file, core_datadir nor rpc_user/rpc_password configured"),
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
        let (host, port) = parse_url(&cfg.rpc_url, cfg.chain.default_rpc_port())?;
        let cookie_path = cfg.resolve_cookie()?;
        // Core deletes its cookie on shutdown. Do not fail construction: a
        // server with a valid index must be able to start while Core is down,
        // and the cookie is (re)loaded lazily once Core is back.
        let auth = match (&cfg.rpc_user, &cfg.rpc_password) {
            (Some(user), Some(password)) => format!("{user}:{password}"),
            _ => match &cookie_path {
                Some(path) => std::fs::read_to_string(path)
                    .map(|cookie| cookie.trim().to_string())
                    .unwrap_or_default(),
                None => String::new(),
            },
        };
        Ok(CoreClient {
            host,
            port,
            auth: std::sync::Arc::new(std::sync::Mutex::new(auth)),
            cookie_path,
            timeout: cfg.timeout,
        })
    }

    fn post(&self, body: &str) -> Result<(u16, Vec<u8>)> {
        let addr = format!("{}:{}", self.host, self.port);
        let sockaddr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| CoreError(format!("cannot resolve {addr}")))?;
        let mut stream = TcpStream::connect_timeout(&sockaddr, self.timeout)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;

        let mut auth = self
            .auth
            .lock()
            .map_err(|_| CoreError("core auth lock poisoned".into()))?
            .clone();
        if auth.is_empty() {
            // Core may have just come back and written a fresh cookie.
            if self.refresh_auth().is_ok() {
                auth = self
                    .auth
                    .lock()
                    .map_err(|_| CoreError("core auth lock poisoned".into()))?
                    .clone();
            }
        }
        let req = format!(
            "POST / HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Basic {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            base64(auth.as_bytes()),
            body.len(),
            body
        );
        stream.write_all(req.as_bytes())?;
        stream.flush()?;
        read_http_response(&mut stream)
    }

    /// Core rotates its cookie on restart; reload it before retrying a 401.
    fn refresh_auth(&self) -> Result<()> {
        let Some(path) = &self.cookie_path else {
            return err("explicit RPC credentials cannot be refreshed");
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
        let (mut status, mut bytes) = self.post(&body)?;
        if status == 401 {
            self.refresh_auth()?;
            let retry = self.post(&body)?;
            status = retry.0;
            bytes = retry.1;
        }
        let parsed: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| CoreError(format!("invalid JSON from Core: {e}")))?;
        if !parsed["error"].is_null() {
            return err(format!(
                "{} failed: {} (code {})",
                method, parsed["error"]["message"], parsed["error"]["code"]
            ));
        }
        if !(200..300).contains(&status) {
            return err(format!(
                "HTTP {status} from Core: {}",
                String::from_utf8_lossy(&bytes).trim()
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
        let (mut status, mut bytes) = self.post(&body)?;
        if status == 401 {
            self.refresh_auth()?;
            let retry = self.post(&body)?;
            status = retry.0;
            bytes = retry.1;
        }
        if !(200..300).contains(&status) {
            // Core returns a JSON-RPC error object (with HTTP 500) for most
            // batch failures; let the item loop surface it when possible.
            if serde_json::from_slice::<Vec<serde_json::Value>>(&bytes).is_err() {
                return err(format!(
                    "HTTP {status} from Core: {}",
                    String::from_utf8_lossy(&bytes).trim()
                ));
            }
        }
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

    pub fn block_count(&self) -> Result<u64> {
        self.call("getblockcount", serde_json::json!([]))
    }

    /// Hashes for a contiguous height window in one HTTP request. Stops at the
    /// first height Core does not have yet (the batch is ordered by id).
    pub fn block_hashes(&self, start: u64, count: u64) -> Result<Vec<String>> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let params: Vec<serde_json::Value> =
            (0..count).map(|i| serde_json::json!([start + i])).collect();
        let results = self.batch("getblockhash", params)?;
        let mut out = Vec::with_capacity(results.len());
        for item in results {
            match item {
                Ok(v) => match v.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => break,
                },
                Err(_) => break,
            }
        }
        Ok(out)
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

    /// Decode a raw transaction without needing the block or txindex. Works on
    /// a pruned node, unlike `getrawtransaction` for old blocks.
    pub fn decode_raw_transaction(&self, tx_hex: &str) -> Result<serde_json::Value> {
        self.call("decoderawtransaction", serde_json::json!([tx_hex]))
    }

    pub fn send_raw_transaction(&self, tx_hex: &str) -> Result<String> {
        self.call("sendrawtransaction", serde_json::json!([tx_hex]))
    }

    /// Submit a child-with-parents package (Bitcoin Core 28+).
    pub fn submit_package(&self, raw_txs: &[String]) -> Result<serde_json::Value> {
        self.call("submitpackage", serde_json::json!([raw_txs]))
    }

    /// Test mempool acceptance without broadcasting.
    pub fn test_mempool_accept(&self, raw_txs: &[String]) -> Result<serde_json::Value> {
        self.call("testmempoolaccept", serde_json::json!([raw_txs]))
    }

    pub fn raw_mempool_verbose(&self) -> Result<std::collections::HashMap<String, MempoolEntry>> {
        self.call("getrawmempool", serde_json::json!([true]))
    }

    pub fn mempool_info(&self) -> Result<MempoolInfo> {
        self.call("getmempoolinfo", serde_json::json!([]))
    }

    pub fn estimate_smart_fee(&self, target: u32, mode: Option<&str>) -> Result<EstimateSmartFee> {
        let params = match mode {
            Some(mode) => serde_json::json!([target, mode]),
            None => serde_json::json!([target]),
        };
        self.call("estimatesmartfee", params)
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
    /// Entry time from Core (epoch seconds); 0 when unavailable.
    #[serde(default)]
    pub time: u64,
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
    pub incrementalrelayfee: f64,
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
    #[serde(default = "d_true")]
    pub networkactive: bool,
    #[serde(default)]
    pub connections: u64,
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
    pub prune_target_size: u64,
    #[serde(default)]
    pub size_on_disk: u64,
    #[serde(default)]
    pub initialblockdownload: bool,
    #[serde(default)]
    pub signet_challenge: Option<String>,
    #[serde(default)]
    pub verificationprogress: f64,
}

fn d_true() -> bool {
    true
}

fn parse_url(url: &str, default_port: u16) -> Result<(String, u16)> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        CoreError(format!(
            "unsupported RPC URL (need http://host:port): {url}"
        ))
    })?;
    let rest = rest.trim_end_matches('/');
    if rest.contains('/') {
        return err("RPC URL must not contain a path");
    }
    // IPv6 literals are bracketed: [::1]:8332 or [::1].
    if let Some(stripped) = rest.strip_prefix('[') {
        let (host, tail) = stripped
            .split_once(']')
            .ok_or_else(|| CoreError("bad IPv6 RPC URL".into()))?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p
                .parse::<u16>()
                .map_err(|_| CoreError("bad RPC port".into()))?,
            None => default_port,
        };
        return Ok((format!("[{host}]"), port));
    }
    let (host, port) = if let Some((h, p)) = rest.rsplit_once(':') {
        (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| CoreError("bad RPC port".into()))?,
        )
    } else {
        (rest.to_string(), default_port)
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
fn read_http_response(stream: &mut TcpStream) -> Result<(u16, Vec<u8>)> {
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
    let raw_headers = String::from_utf8_lossy(&buf[..header_end]);
    let status = raw_headers
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| CoreError("malformed HTTP status line from Core".into()))?;
    let headers = raw_headers.to_ascii_lowercase();
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

    Ok((status, body))
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
            parse_url("http://127.0.0.1:18443", 8332).unwrap(),
            ("127.0.0.1".to_string(), 18443)
        );
        assert_eq!(
            parse_url("http://127.0.0.1", 38332).unwrap(),
            ("127.0.0.1".to_string(), 38332)
        );
        assert_eq!(
            parse_url("http://[::1]", 48332).unwrap(),
            ("[::1]".to_string(), 48332)
        );
        assert!(parse_url("https://127.0.0.1:18443", 8332).is_err());
        assert!(parse_url("http://127.0.0.1:18443/wallet/x", 8332).is_err());
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
    fn talks_to_a_stub_core_with_explicit_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(
                req.contains("Authorization: Basic dXNlcjpwYXNz"),
                "missing explicit Basic auth: {req}"
            );
            let body = r#"{"result":{"chain":"regtest","blocks":101,"headers":101},"error":null,"id":"e-lite"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).unwrap();
        });
        let cfg = CoreConfig {
            rpc_url: format!("http://127.0.0.1:{port}"),
            cookie_file: None,
            core_datadir: None,
            rpc_user: Some("user".to_string()),
            rpc_password: Some("pass".to_string()),
            chain: Chain::Regtest,
            timeout: Duration::from_secs(5),
        };
        let client = CoreClient::connect(&cfg).unwrap();
        let info = client.blockchain_info().unwrap();
        assert_eq!(info.blocks, 101);
        handle.join().unwrap();
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
            rpc_user: None,
            rpc_password: None,
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
