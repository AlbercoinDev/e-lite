//! Sources of loose transactions by txid (AUD-163).
//!
//! A `TxSource` returns raw transactions for txids it is asked for, from one
//! named server per call. Nothing it returns is trusted: the resolver checks
//! every real transaction against the local index (txid, position, merkle
//! branch up to the indexed header) before caching or serving it. Decoy
//! transactions are only parsed for their outputs.

use crate::esplora::isolated_tor_client_with;
use crate::health::{Blame, HealthMap, TRANSIENT_BASE, TRANSIENT_CAP};
use crate::{AttemptCtl, RawtxError, Result};
use el_model::Hash32;
use std::collections::HashMap;
use std::io::Read;
use std::sync::mpsc;
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

/// Most transactions one Esplora-tx call fetches at once (each request is its
/// own Tor circuit).
const ESPLORA_TX_PARALLEL: usize = 8;
/// Largest raw transaction accepted (a standard transaction is < 400 kWU).
pub(crate) const MAX_TX_BYTES: u64 = 4 * 1024 * 1024;

/// `GET <base>/tx/:txid/raw` on Esplora providers, one Tor circuit per
/// request (a fresh SOCKS username, AUD-135).
pub struct EsploraTxSource {
    bases: Vec<String>,
    tor_proxy: Option<String>,
    direct: Option<reqwest::blocking::Client>,
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
        let direct = (allow_clearnet && tor_proxy.is_none())
            .then(|| {
                reqwest::blocking::Client::builder()
                    .connect_timeout(connect)
                    .timeout(stall)
                    .user_agent(crate::HTTP_USER_AGENT)
                    .build()
                    .ok()
            })
            .flatten();
        let bases: Vec<String> = bases
            .into_iter()
            .map(|b| b.trim_end_matches('/').to_string())
            .filter(|b| {
                let onion = b.to_ascii_lowercase().contains(".onion");
                (tor_proxy.is_some() || (direct.is_some() && !onion))
                    && (b.starts_with("https://") || (onion && b.starts_with("http://")))
            })
            .collect();
        if bases.is_empty() {
            return None;
        }
        Some(EsploraTxSource {
            bases,
            tor_proxy,
            direct,
            connect,
            stall,
            cooldown,
            health: HealthMap::new(),
            conns,
        })
    }

    fn client(&self, base: &str) -> Option<reqwest::blocking::Client> {
        match &self.tor_proxy {
            Some(socks) => {
                let factor = if base.contains(".onion") { 2 } else { 1 };
                isolated_tor_client_with(socks, self.connect * factor, self.stall * factor)
            }
            None => self.direct.clone(),
        }
    }

    fn fetch_one(
        client: &reqwest::blocking::Client,
        base: &str,
        txid: &Hash32,
        deadline: Instant,
        ctl: &AttemptCtl,
    ) -> std::result::Result<Vec<u8>, Blame> {
        let url = format!("{base}/tx/{}/raw", txid.to_hex());
        let mut resp = client.get(&url).send().map_err(|e| {
            if e.is_connect() || e.is_timeout() {
                Blame::Unreachable
            } else {
                Blame::Transient
            }
        })?;
        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
        {
            return Err(Blame::RateLimited(None));
        }
        if status.is_server_error() {
            return Err(Blame::Transient);
        }
        if !status.is_success() {
            return Err(Blame::Hard);
        }
        let mut bytes = Vec::new();
        let mut buf = [0u8; 16 * 1024];
        loop {
            if ctl.aborted() || Instant::now() >= deadline {
                return Err(Blame::Transient);
            }
            match resp.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    ctl.add_bytes(n);
                    bytes.extend_from_slice(&buf[..n]);
                    if bytes.len() as u64 > MAX_TX_BYTES {
                        return Err(Blame::Hard);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return Err(Blame::Transient),
            }
        }
        Ok(bytes)
    }
}

impl TxSource for EsploraTxSource {
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

    fn fetch_from(
        &self,
        server: &str,
        txids: &[Hash32],
        deadline: Instant,
        ctl: &AttemptCtl,
    ) -> Result<TxBatch> {
        let (tx, rx) = mpsc::channel();
        let mut next = 0usize;
        let mut running = 0usize;
        let mut out = TxBatch::new();
        let mut worst: Option<Blame> = None;
        while next < txids.len() || running > 0 {
            while running < ESPLORA_TX_PARALLEL && next < txids.len() {
                let Some(client) = self.client(server) else {
                    return Err(RawtxError::Unavailable(format!("{server}: no client")));
                };
                let txid = txids[next];
                let base = server.to_string();
                let tx = tx.clone();
                let ctl = ctl.clone();
                // One connection per request, from the shared pool: Tor must
                // not be asked for 8 sessions x 8 circuits (AUD-167).
                let Some(permit) = self.conns.acquire_until(deadline, 0) else {
                    // No slot before the deadline: leave the rest for the next
                    // attempt instead of blaming the server. If nothing is
                    // running either, the caller sees a stalled session and
                    // the deadline ends it.
                    break;
                };
                std::thread::spawn(move || {
                    let _permit = permit;
                    let r = Self::fetch_one(&client, &base, &txid, deadline, &ctl);
                    let _ = tx.send((txid, r));
                });
                next += 1;
                running += 1;
            }
            let wait = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait.max(Duration::from_millis(1))) {
                Ok((txid, Ok(bytes))) => {
                    running -= 1;
                    out.insert(txid, bytes);
                }
                Ok((_, Err(blame))) => {
                    running -= 1;
                    // Over Tor a 429 is one exit (AUD-160); several pause it.
                    let pause = match blame {
                        Blame::RateLimited(_) => self.health.circuit_limited(server),
                        _ => true,
                    };
                    if pause && worst.is_none() {
                        worst = Some(blame);
                    }
                }
                Err(_) => break,
            }
        }
        if !ctl.aborted() {
            match (out.is_empty(), worst) {
                (false, _) => self.health.succeed(server),
                (true, Some(blame)) => {
                    self.health
                        .fail(server, blame, TRANSIENT_BASE, TRANSIENT_CAP, self.cooldown);
                }
                (true, None) => {}
            }
        }
        if out.is_empty() {
            return Err(RawtxError::Unavailable(format!("{server}: no transaction")));
        }
        Ok(out)
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
