//! Sources of loose transactions by txid (AUD-163).
//!
//! A `TxSource` returns raw transactions for txids it is asked for, from one
//! named server per call. Nothing it returns is trusted: the resolver checks
//! every real transaction against the local index (txid, position, merkle
//! branch up to the indexed header) before caching or serving it. Decoy
//! transactions are only parsed for their outputs.

use crate::health::{Blame, HealthMap, TRANSIENT_BASE, TRANSIENT_CAP};
use crate::{AttemptCtl, RawtxError, Result};
use el_model::Hash32;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Raw transactions a server returned, keyed by the txid asked for.
pub type TxBatch = HashMap<Hash32, Vec<u8>>;

pub trait TxSource: Send + Sync {
    /// `electrum` or `esplora-tx`.
    fn name(&self) -> &str;
    /// Servers usable now (not cooling down), each a key for `fetch_from`.
    fn servers(&self) -> Vec<String>;
    /// Every configured server, cooling or not.
    fn all_servers(&self) -> Vec<String>;
    /// Whether the server is reached through Tor.
    fn via_tor(&self, server: &str) -> bool;
    fn requires_qualification(&self) -> bool {
        false
    }
    /// Electrum batches many txids on one stream. Esplora needs one isolated
    /// connection per transaction and is scheduled accordingly.
    fn max_batch_size(&self) -> usize {
        usize::MAX
    }
    /// Ask one server for `txids` in one session (or one burst of requests).
    /// May return only some of them. Updates the server's health itself.
    fn fetch_from(
        &self,
        server: &str,
        txids: &[Hash32],
        deadline: Instant,
        ctl: &AttemptCtl,
    ) -> Result<TxBatch>;
    /// The server returned something that does not verify.
    fn penalize(&self, server: &str);
}

/// Largest raw transaction accepted (a standard transaction is < 400 kWU).
pub(crate) const MAX_TX_BYTES: u64 = 4 * 1024 * 1024;

/// `GET <base>/tx/:txid/raw` on Esplora providers, one Tor circuit per
/// request (a fresh SOCKS username, AUD-135).
pub struct EsploraTxSource {
    bases: Vec<String>,
    tor_proxy: Option<String>,
    allow_clearnet: bool,
    connect: Duration,
    stall: Duration,
    cooldown: Duration,
    health: HealthMap,
    /// Shared pool of connections for the whole per-txid path (AUD-167).
    conns: Arc<crate::Semaphore>,
}

impl EsploraTxSource {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        bases: Vec<String>,
        tor_proxy: Option<String>,
        allow_clearnet: bool,
        connect: Duration,
        stall: Duration,
        cooldown: Duration,
        conns: Arc<crate::Semaphore>,
    ) -> Option<Self> {
        let bases: Vec<String> = bases
            .into_iter()
            .map(|b| b.trim_end_matches('/').to_string())
            .filter(|b| {
                let onion = b.to_ascii_lowercase().contains(".onion");
                (tor_proxy.is_some() || (allow_clearnet && !onion))
                    && (b.starts_with("https://") || (onion && b.starts_with("http://")))
            })
            .collect();
        if bases.is_empty() {
            return None;
        }
        Some(EsploraTxSource {
            bases,
            tor_proxy,
            allow_clearnet,
            connect,
            stall,
            cooldown,
            health: HealthMap::new(),
            conns,
        })
    }

    fn client(&self, base: &str) -> Option<reqwest::Client> {
        let factor = if base.contains(".onion") { 2 } else { 1 };
        let builder = reqwest::Client::builder()
            .connect_timeout((self.connect * factor).min(self.stall * factor))
            .read_timeout(self.stall * factor)
            .user_agent(crate::HTTP_USER_AGENT)
            .pool_max_idle_per_host(0)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy();
        match &self.tor_proxy {
            Some(socks) => {
                let user = format!("el{:016x}", rand::random::<u64>());
                builder
                    .proxy(reqwest::Proxy::all(format!("socks5h://{user}:x@{socks}")).ok()?)
                    .build()
                    .ok()
            }
            None if self.allow_clearnet => builder.build().ok(),
            None => None,
        }
    }

    async fn fetch_one(
        client: reqwest::Client,
        base: &str,
        txid: &Hash32,
        ctl: &AttemptCtl,
    ) -> std::result::Result<Vec<u8>, Blame> {
        let url = format!("{base}/tx/{}/raw", txid.to_hex());
        let mut resp = client.get(&url).send().await.map_err(|e| {
            if e.is_connect() || e.is_timeout() {
                Blame::Unreachable
            } else {
                Blame::Transient
            }
        })?;
        ctl.mark_connected();
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            ctl.ignore_failure_for_health();
            return Err(Blame::Transient);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
        {
            let after = crate::esplora::retry_after(resp.headers());
            ctl.mark_rate_limit(after);
            return Err(Blame::RateLimited(after));
        }
        if status.is_server_error() {
            return Err(Blame::Transient);
        }
        if !status.is_success() {
            return Err(Blame::Hard);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|_| Blame::Transient)? {
            if ctl.aborted() {
                return Err(Blame::Transient);
            }
            if bytes.len().saturating_add(chunk.len()) as u64 > MAX_TX_BYTES {
                return Err(Blame::Hard);
            }
            ctl.add_bytes(chunk.len());
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

impl TxSource for EsploraTxSource {
    fn requires_qualification(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "esplora-tx"
    }

    fn servers(&self) -> Vec<String> {
        self.bases
            .iter()
            .filter(|b| !self.health.cooling(b))
            .cloned()
            .collect()
    }

    fn all_servers(&self) -> Vec<String> {
        self.bases.clone()
    }

    fn via_tor(&self, _server: &str) -> bool {
        self.tor_proxy.is_some()
    }

    fn max_batch_size(&self) -> usize {
        1
    }

    fn fetch_from(
        &self,
        server: &str,
        txids: &[Hash32],
        deadline: Instant,
        ctl: &AttemptCtl,
    ) -> Result<TxBatch> {
        let Some((txid, rest)) = txids.split_first() else {
            return Ok(TxBatch::new());
        };
        for txid in rest {
            ctl.skip(*txid);
        }
        let busy = || RawtxError::Unavailable(format!("{server}: {}", crate::LOCAL_BUSY));
        let (_stream, _conn) = if self.via_tor(server) {
            (ctl.take_stream(None).map_err(|_| busy())?, None)
        } else {
            (None, Some(self.conns.try_acquire(0).ok_or_else(busy)?))
        };
        let result = {
            // No detached worker: on deadline the HTTP future is dropped,
            // then its runtime closes the socket before the permit is freed.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| busy())?;
            runtime.block_on(async {
                let client = self.client(server).ok_or(Blame::Hard)?;
                tokio::time::timeout_at(deadline.into(), Self::fetch_one(client, server, txid, ctl))
                    .await
                    .unwrap_or(Err(Blame::Transient))
            })
        };
        match result {
            Ok(bytes) => {
                if ctl.updates_health() {
                    self.health.succeed(server);
                }
                Ok(TxBatch::from([(*txid, bytes)]))
            }
            Err(blame) => {
                if ctl.updates_health()
                    && (!matches!(blame, Blame::RateLimited(_))
                        || self.health.circuit_limited(server))
                {
                    self.health
                        .fail(server, blame, TRANSIENT_BASE, TRANSIENT_CAP, self.cooldown);
                }
                let why = match blame {
                    Blame::Hard => "HTTP request rejected",
                    Blame::Unreachable => "HTTP connection failed",
                    Blame::RateLimited(_) => "HTTP rate limited",
                    Blame::Transient => "HTTP attempt timed out or stalled",
                };
                Err(RawtxError::Unavailable(format!("{server}: {why}")))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::governor::{Lane, TorGovernor};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    #[test]
    fn esplora_deadline_closes_the_tor_socket_and_releases_the_permit() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy = listener.local_addr().unwrap().to_string();
        let remote = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut h = [0; 2];
            sock.read_exact(&mut h).unwrap();
            let mut bytes = vec![0; h[1] as usize];
            sock.read_exact(&mut bytes).unwrap();
            assert!(
                bytes.contains(&2),
                "Tor connection must use isolated credentials"
            );
            sock.write_all(&[5, 2]).unwrap();
            sock.read_exact(&mut h).unwrap();
            bytes.resize(h[1] as usize, 0);
            sock.read_exact(&mut bytes).unwrap();
            let mut n = [0];
            sock.read_exact(&mut n).unwrap();
            bytes.resize(n[0] as usize, 0);
            sock.read_exact(&mut bytes).unwrap();
            sock.write_all(&[1, 0]).unwrap();
            let mut req = [0; 5];
            sock.read_exact(&mut req).unwrap();
            assert_eq!(req[3], 3);
            bytes.resize(req[4] as usize + 2, 0);
            sock.read_exact(&mut bytes).unwrap();
            sock.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).unwrap();
            let mut r = BufReader::new(sock);
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            assert!(line.starts_with("GET /tx/"));
            while {
                line.clear();
                r.read_line(&mut line).unwrap() != 0 && line != "\r\n"
            } {}
            // Never send the HTTP response; the client must cancel the
            // stalled request and actively close instead of detaching it.
            assert_eq!(r.read(&mut [0]).unwrap(), 0);
        });
        let endpoint = "http://example.onion";
        let src = EsploraTxSource::new(
            vec![endpoint.into()],
            Some(proxy),
            false,
            Duration::from_secs(5),
            Duration::from_secs(5),
            Duration::from_secs(30),
            crate::Semaphore::new(1),
        )
        .unwrap();
        let governor = TorGovernor::new(1);
        let stream = governor.try_acquire(Lane::Wallet).unwrap();
        let ctl = AttemptCtl::new().with_streams(Arc::clone(&governor), Lane::Wallet, Some(stream));
        let start = Instant::now();
        let err = src
            .fetch_from(
                endpoint,
                &[Hash32([0x31; 32])],
                start + Duration::from_millis(500),
                &ctl,
            )
            .unwrap_err();
        assert!(err.to_string().contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(governor.in_use(), 0);
        remote.join().unwrap();
    }
}
