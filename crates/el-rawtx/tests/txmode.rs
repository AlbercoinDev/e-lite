//! Per-txid mode (AUD-163) against a synthetic index and in-memory sources.

use bitcoin::absolute::LockTime;
use bitcoin::block::Header;
use bitcoin::hashes::Hash;
use bitcoin::transaction::Version;
use bitcoin::{
    Amount, Block, BlockHash, CompactTarget, OutPoint, ScriptBuf, Sequence, Transaction, TxIn,
    TxMerkleNode, TxOut, Witness,
};
use el_model::codec::SCHEMA_VERSION;
use el_model::{BlockRecord, Hash32, HistoryEffect, IndexedTip, ScriptHash};
use el_rawtx::{
    AttemptCtl, PrivacyParams, RawtxConfig, Resolver, Result, TxBatch, TxRef, TxSource,
};
use el_store::Store;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "e-lite-txmode-{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn script(n: u32) -> ScriptBuf {
    let mut bytes = vec![0x00, 0x14];
    bytes.extend_from_slice(&[0u8; 16]);
    bytes.extend_from_slice(&n.to_le_bytes());
    ScriptBuf::from_bytes(bytes)
}

fn tx(tag: u32, pay_to: &[u32]) -> Transaction {
    Transaction {
        version: Version::TWO,
        lock_time: LockTime::from_consensus(tag),
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            script_sig: ScriptBuf::from_bytes(tag.to_le_bytes().to_vec()),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: pay_to
            .iter()
            .map(|s| TxOut {
                value: Amount::from_sat(1000),
                script_pubkey: script(*s),
            })
            .collect(),
    }
}

fn txid_of(t: &Transaction) -> Hash32 {
    Hash32::from_hex(&t.compute_txid().to_string()).unwrap()
}

struct Chain {
    store: Arc<Store>,
    dir: PathBuf,
    raw: HashMap<Hash32, Vec<u8>>,
    /// The real wallet's address and its history.
    real: (ScriptHash, Vec<TxRef>),
}

/// 40 blocks. Address 1 is the "real" wallet (5 transactions in blocks
/// 1..=5); addresses 100.. each have 3-8 transactions spread over the other
/// blocks, so decoys of a similar size exist.
fn chain(name: &str) -> Chain {
    let dir = tmp(name);
    let store = Arc::new(Store::open(&dir).unwrap());
    let mut batch = Store::batch();
    let mut all_txids = Vec::new();
    let mut raw = HashMap::new();
    let mut real_hist = Vec::new();
    let mut txnum = 0u64;
    let mut prev = BlockHash::all_zeros();
    let mut tag = 0u32;
    for height in 0..40u32 {
        let mut txs = vec![tx(1_000_000 + height, &[999_999])]; // coinbase-like
        let mut owners: Vec<Vec<u32>> = vec![vec![]];
        if (1..=5).contains(&height) {
            tag += 1;
            txs.push(tx(tag, &[1, 1_000 + height]));
            owners.push(vec![1, 1_000 + height]);
        }
        for a in 0..6u32 {
            let addr = 100 + (height * 7 + a * 3) % 30;
            tag += 1;
            txs.push(tx(tag, &[addr]));
            owners.push(vec![addr]);
        }
        let mut block = Block {
            header: Header {
                version: bitcoin::block::Version::from_consensus(1),
                prev_blockhash: prev,
                merkle_root: TxMerkleNode::all_zeros(),
                time: height,
                bits: CompactTarget::from_consensus(0x207fffff),
                nonce: 0,
            },
            txdata: txs,
        };
        block.header.merkle_root = block.compute_merkle_root().unwrap();
        prev = block.block_hash();
        let hash = Hash32::from_hex(&block.block_hash().to_string()).unwrap();
        let header: [u8; 80] = bitcoin::consensus::serialize(&block.header)
            .try_into()
            .unwrap();
        store
            .put_block(
                &mut batch,
                &BlockRecord {
                    height,
                    hash,
                    header,
                    tx_count: block.txdata.len() as u32,
                    txnum_base: txnum,
                },
            )
            .unwrap();
        for (pos, t) in block.txdata.iter().enumerate() {
            let id = txid_of(t);
            raw.insert(id, bitcoin::consensus::serialize(t));
            store
                .put_occurrence(&mut batch, id, height, pos as u32)
                .unwrap();
            all_txids.push(id);
            for owner in &owners[pos] {
                let sh = ScriptHash::from_script(script(*owner).as_bytes());
                store
                    .put_history(
                        &mut batch,
                        sh,
                        height,
                        pos as u32,
                        &HistoryEffect::default(),
                    )
                    .unwrap();
                if *owner == 1 {
                    real_hist.push(TxRef {
                        txid: id,
                        height,
                        pos: pos as u32,
                    });
                }
            }
        }
        txnum += block.txdata.len() as u64;
        if height == 39 {
            store
                .commit(
                    std::mem::replace(&mut batch, Store::batch()),
                    &IndexedTip {
                        height,
                        block_hash: hash,
                        schema_version: SCHEMA_VERSION,
                        txnum,
                    },
                    &all_txids,
                )
                .unwrap();
        }
    }
    let real = ScriptHash::from_script(script(1).as_bytes());
    Chain {
        store,
        dir,
        raw,
        real: (real, real_hist),
    }
}

/// In-memory servers. `liar` answers every txid with another transaction.
struct FakeSource {
    raw: HashMap<Hash32, Vec<u8>>,
    servers: Vec<String>,
    liar: Option<String>,
    calls: Mutex<Vec<(String, Vec<Hash32>)>>,
}

impl FakeSource {
    fn new(raw: &HashMap<Hash32, Vec<u8>>, n: usize, liar: Option<&str>) -> Arc<Self> {
        Arc::new(FakeSource {
            raw: raw.clone(),
            servers: (0..n).map(|i| format!("s{i}")).collect(),
            liar: liar.map(str::to_string),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<(String, Vec<Hash32>)> {
        self.calls.lock().unwrap().clone()
    }

    fn sent(&self) -> Vec<Hash32> {
        self.calls().into_iter().flat_map(|(_, t)| t).collect()
    }
}

impl TxSource for FakeSource {
    fn name(&self) -> &str {
        "fake"
    }
    fn servers(&self) -> Vec<String> {
        self.servers.clone()
    }
    fn all_servers(&self) -> Vec<String> {
        self.servers.clone()
    }
    fn via_tor(&self, _server: &str) -> bool {
        true
    }
    fn fetch_from(
        &self,
        server: &str,
        txids: &[Hash32],
        _deadline: Instant,
        _ctl: &AttemptCtl,
    ) -> Result<TxBatch> {
        self.calls
            .lock()
            .unwrap()
            .push((server.to_string(), txids.to_vec()));
        let other = self.raw.values().next().unwrap().clone();
        Ok(txids
            .iter()
            .filter_map(|t| {
                let bytes = if self.liar.as_deref() == Some(server) {
                    Some(other.clone())
                } else {
                    self.raw.get(t).cloned()
                };
                bytes.map(|b| (*t, b))
            })
            .collect())
    }
    fn penalize(&self, _server: &str) {}
}

fn config(privacy: PrivacyParams) -> RawtxConfig {
    RawtxConfig {
        enabled: true,
        cache_mib: 8,
        decoys: 0,
        tor_socks: None,
        allow_clearnet: false,
        fallback_clearnet: false,
        request_timeout_seconds: 5,
        tor_request_timeout_seconds: 5,
        tor_connect_timeout_seconds: 5,
        tor_stall_timeout_seconds: 5,
        provider_cooldown_seconds: 60,
        max_parallel_fetches: 4,
        cache_dir: None,
        use_p2p: false,
        use_esplora: false,
        p2p_v2: false,
        esplora_urls: Vec::new(),
        p2p_peers: Vec::new(),
        p2p_timeout: Duration::from_secs(1),
        p2p_tor_timeout: Duration::from_secs(1),
        chain: el_model::Chain::Regtest,
        privacy,
        electrum_servers: Vec::new(),
    }
}

fn resolver(c: &Chain, privacy: PrivacyParams, src: Arc<FakeSource>, secret: [u8; 32]) -> Resolver {
    let r = Resolver::new(Arc::clone(&c.store), &config(privacy))
        .unwrap()
        .with_tx_sources(vec![src as Arc<dyn TxSource>], secret);
    // The per-txid path is gated on Core's prune height (AUD-167); these
    // fixtures model a Core that has pruned nothing.
    r.set_prune_height(u32::MAX);
    r
}

/// Wait until the per-txid path has nothing queued or running, so the test
/// does not drop the store under a background session (SIGABRT in CI).
fn settle(r: &Resolver) {
    for _ in 0..500 {
        if r.tx_idle() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Wait until every real transaction is served (they were queued by
/// `want_history`).
fn load(r: &Resolver, c: &Chain, ctx: u64) -> Vec<String> {
    r.want_history(ctx, c.real.0, &c.real.1);
    c.real
        .1
        .iter()
        .map(|t| r.raw_tx_for(ctx, t.txid, t.height).unwrap())
        .collect()
}

#[test]
fn a_wallet_loads_by_txid_with_decoy_wallets() {
    let c = chain("load");
    let src = FakeSource::new(&c.raw, 4, None);
    let r = resolver(&c, PrivacyParams::balanced(), Arc::clone(&src), [7; 32]);
    let got = load(&r, &c, 1);
    for (t, hex) in c.real.1.iter().zip(&got) {
        assert_eq!(hex, &hex::encode(&c.raw[&t.txid]));
    }
    let real: HashSet<Hash32> = c.real.1.iter().map(|t| t.txid).collect();
    let sent = src.sent();
    let decoys: Vec<_> = sent.iter().filter(|t| !real.contains(t)).collect();
    assert!(
        decoys.len() >= real.len() * 2,
        "{} decoys for {} real",
        decoys.len(),
        real.len()
    );
    let st = r.stats();
    // The fixture may list the same transaction for two of the wallet's
    // addresses, so the counter can exceed the unique set; every unique
    // transaction must be verified at least once.
    assert!(
        st.tx_verified.load(Ordering::Relaxed) >= real.len() as u64,
        "{} verified for {} unique",
        st.tx_verified.load(Ordering::Relaxed),
        real.len()
    );
    // Only real transactions are cached: a second load asks for none of
    // them again (decoy sessions still queued behind the per-server limit
    // may start meanwhile).
    let real_sent = |src: &FakeSource| src.sent().iter().filter(|t| real.contains(t)).count();
    let before = real_sent(&src);
    let again = load(&r, &c, 1);
    // Let the second load's decoy sessions finish before counting the cache:
    // every background write is a decoy (never real), but the count must be
    // stable to assert on it.
    settle(&r);
    assert_eq!(again, got);
    assert_eq!(real_sent(&src), before, "served from the cache");
    assert_eq!(r.cached_txs(), real.len());
    settle(&r);
    let _ = std::fs::remove_dir_all(&c.dir);
}

#[test]
fn no_server_gets_more_than_its_share_and_sessions_are_short() {
    let c = chain("spread");
    let src = FakeSource::new(&c.raw, 4, None);
    let mut p = PrivacyParams::balanced();
    p.session_size = 3;
    // No decoys: in this fixture a decoy address can share a transaction with
    // the wallet, so counting txids by membership cannot tell a real request
    // from a decoy one (the spread of the real group is what this test
    // measures; `a_wallet_loads_by_txid_with_decoy_wallets` covers decoys).
    p.decoy_wallets = 0;
    let r = resolver(&c, p, Arc::clone(&src), [7; 32]);
    load(&r, &c, 1);
    // Measure the whole load, not a snapshot in the middle of the batches.
    settle(&r);
    let real: HashSet<Hash32> = c.real.1.iter().map(|t| t.txid).collect();
    let mut per_server: HashMap<String, usize> = HashMap::new();
    for (server, txids) in src.calls() {
        assert!(txids.len() <= 3, "session of {}", txids.len());
        *per_server.entry(server).or_default() += txids.iter().filter(|t| real.contains(t)).count();
    }
    let max = per_server.values().copied().max().unwrap_or(0);
    assert!(
        max <= real.len().div_ceil(3),
        "{max} of {} real txs on one server",
        real.len()
    );
    settle(&r);
    let _ = std::fs::remove_dir_all(&c.dir);
}

#[test]
fn the_share_holds_across_many_batches() {
    // One transaction per batch: the per-server limit must hold over the
    // whole load, not per batch (fast servers would otherwise get them all).
    let c = chain("batches");
    let src = FakeSource::new(&c.raw, 4, None);
    let mut p = PrivacyParams::balanced();
    p.batch_window_ms = 0;
    p.decoy_wallets = 0;
    let r = resolver(&c, p, Arc::clone(&src), [7; 32]);
    for t in &c.real.1 {
        r.raw_tx_for(1, t.txid, t.height).unwrap();
    }
    let mut per_server: HashMap<String, usize> = HashMap::new();
    for (server, txids) in src.calls() {
        *per_server.entry(server).or_default() += txids.len();
    }
    let max = per_server.values().copied().max().unwrap_or(0);
    assert!(max <= c.real.1.len().div_ceil(3), "{per_server:?}");
    settle(&r);
    let _ = std::fs::remove_dir_all(&c.dir);
}

#[test]
fn a_lying_server_is_caught_by_the_index_and_bypassed() {
    let c = chain("liar");
    let src = FakeSource::new(&c.raw, 4, Some("s0"));
    let r = resolver(&c, PrivacyParams::balanced(), Arc::clone(&src), [7; 32]);
    let got = load(&r, &c, 1);
    for (t, hex) in c.real.1.iter().zip(&got) {
        assert_eq!(
            hex,
            &hex::encode(&c.raw[&t.txid]),
            "never the liar's answer"
        );
    }
    let lied = src.calls().iter().any(|(s, _)| s == "s0");
    if lied {
        assert!(r.stats().tx_verify_failed.load(Ordering::Relaxed) >= 1);
    }
    settle(&r);
    let _ = std::fs::remove_dir_all(&c.dir);
}

#[test]
fn decoy_wallets_are_stable_for_one_secret() {
    let decoys_for = |secret: [u8; 32]| {
        let c = chain("stable");
        let src = FakeSource::new(&c.raw, 4, None);
        let r = resolver(&c, PrivacyParams::balanced(), Arc::clone(&src), secret);
        load(&r, &c, 1);
        let real: HashSet<Hash32> = c.real.1.iter().map(|t| t.txid).collect();
        let set: HashSet<Hash32> = src
            .sent()
            .into_iter()
            .filter(|t| !real.contains(t))
            .collect();
        settle(&r);
        let _ = std::fs::remove_dir_all(&c.dir);
        set
    };
    let a = decoys_for([7; 32]);
    assert!(!a.is_empty());
    for _ in 0..4 {
        assert_eq!(
            decoys_for([7; 32]),
            a,
            "same secret, same decoys (no intersection between loads)"
        );
    }
    let other = decoys_for([8; 32]);
    assert_ne!(a, other, "another secret picks other decoys");
}

#[test]
fn block_mode_never_sends_a_txid() {
    let c = chain("block");
    let src = FakeSource::new(&c.raw, 4, None);
    let r = resolver(&c, PrivacyParams::max(), Arc::clone(&src), [7; 32]);
    assert_eq!(r.want_history(1, c.real.0, &c.real.1), 0);
    // No block source either: the request fails without asking anyone.
    let t = c.real.1[0];
    assert!(r.raw_tx_for(1, t.txid, t.height).is_err());
    assert!(src.calls().is_empty());
    settle(&r);
    let _ = std::fs::remove_dir_all(&c.dir);
}

#[test]
fn the_txid_path_is_gated_on_core_prune_height() {
    let c = chain("prunegate");
    let src = FakeSource::new(&c.raw, 1, None);
    let r = Resolver::new(Arc::clone(&c.store), &config(PrivacyParams::balanced()))
        .unwrap()
        .with_tx_sources(vec![Arc::clone(&src) as Arc<dyn TxSource>], [7; 32]);
    let t = c.real.1[0];
    // Unknown prune height: never send the txid.
    assert!(r.raw_tx_for(1, t.txid, t.height).is_err());
    assert!(
        src.calls().is_empty(),
        "no txid may leave with prune_height=0"
    );
    // Core still has the block (height >= prune height): local path only.
    r.set_prune_height(t.height);
    assert!(r.raw_tx_for(1, t.txid, t.height).is_err());
    assert!(
        src.calls().is_empty(),
        "no txid may leave for an unpruned block"
    );
    // Pruned below the block: the per-txid path is allowed.
    r.set_prune_height(t.height + 1);
    assert!(r.raw_tx_for(1, t.txid, t.height).is_ok());
    assert!(!src.calls().is_empty());
    settle(&r);
    let _ = std::fs::remove_dir_all(&c.dir);
}

#[test]
fn without_decoys_only_real_txids_are_sent() {
    let c = chain("nodecoys");
    let src = FakeSource::new(&c.raw, 2, None);
    let mut p = PrivacyParams::balanced();
    p.decoy_wallets = 0;
    let r = resolver(&c, p, Arc::clone(&src), [7; 32]);
    load(&r, &c, 1);
    let real: HashSet<Hash32> = c.real.1.iter().map(|t| t.txid).collect();
    assert!(src.sent().iter().all(|t| real.contains(t)));
    settle(&r);
    let _ = std::fs::remove_dir_all(&c.dir);
}

/// Live per-txid load over Tor against the public servers of the template:
/// N mainnet blocks, M transactions of each, all requested at once (like a
/// wallet batch), verified against an index built from the blocks' txids.
/// E_LITE_LIVE_TOR=127.0.0.1:9050 [E_LITE_LIVE_BLOCKS=12] [E_LITE_LIVE_PER_BLOCK=20]
/// cargo test -p el-rawtx --test txmode live_tx_mode -- --ignored --nocapture
#[test]
#[ignore]
fn live_tx_mode_mainnet() {
    let Ok(socks) = std::env::var("E_LITE_LIVE_TOR") else {
        eprintln!("set E_LITE_LIVE_TOR");
        return;
    };
    let env = |k: &str, d: usize| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let blocks = env("E_LITE_LIVE_BLOCKS", 12);
    let per_block = env("E_LITE_LIVE_PER_BLOCK", 20);
    let get = |url: &str| -> String {
        for _ in 0..8 {
            if let Some(c) = el_rawtx::isolated_tor_client(&socks, Duration::from_secs(40)) {
                if let Ok(r) = c.get(url).send() {
                    if r.status().is_success() {
                        if let Ok(t) = r.text() {
                            return t;
                        }
                    }
                }
            }
        }
        panic!("cannot fetch metadata");
    };
    let api = "https://blockstream.info/api";
    let dir = tmp("live");
    let store = Arc::new(Store::open(&dir).unwrap());
    let mut batch = Store::batch();
    let mut all = Vec::new();
    let mut wanted = Vec::new();
    let mut txnum = 0u64;
    let mut height = 700_000
        + (SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
            % 50_000);
    let mut last = (0u32, Hash32([0; 32]));
    for _ in 0..blocks {
        let hash = get(&format!("{api}/block-height/{height}"))
            .trim()
            .to_string();
        let header = hex::decode(get(&format!("{api}/block/{hash}/header")).trim()).unwrap();
        let ids: Vec<String> =
            serde_json::from_str(&get(&format!("{api}/block/{hash}/txids"))).unwrap();
        let ids: Vec<Hash32> = ids.iter().map(|t| Hash32::from_hex(t).unwrap()).collect();
        let h32 = Hash32::from_hex(&hash).unwrap();
        store
            .put_block(
                &mut batch,
                &BlockRecord {
                    height,
                    hash: h32,
                    header: header.try_into().unwrap(),
                    tx_count: ids.len() as u32,
                    txnum_base: txnum,
                },
            )
            .unwrap();
        let step = (ids.len() / per_block).max(1);
        for (pos, id) in ids.iter().enumerate() {
            if pos > 0
                && pos % step == 0
                && wanted.iter().filter(|(_, h, _)| *h == height).count() < per_block
            {
                store
                    .put_occurrence(&mut batch, *id, height, pos as u32)
                    .unwrap();
                wanted.push((*id, height, pos as u32));
            }
        }
        txnum += ids.len() as u64;
        all.extend(ids);
        last = (height, h32);
        height += 1_000 + (txnum as u32 % 997);
    }
    store
        .commit(
            batch,
            &IndexedTip {
                height: last.0,
                block_hash: last.1,
                schema_version: SCHEMA_VERSION,
                txnum,
            },
            &all,
        )
        .unwrap();
    let template = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../public-servers.toml"
    ))
    .unwrap();
    let electrum: Vec<String> = template
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('"') && (l.ends_with(":s\",") || l.ends_with(":t\",")))
        .map(|l| l.trim_matches(|c| c == '"' || c == ',').to_string())
        .collect();
    let mut privacy = PrivacyParams::balanced();
    privacy.parallel_sessions = env("E_LITE_LIVE_PARALLEL", privacy.parallel_sessions);
    privacy.session_size = env("E_LITE_LIVE_SESSION", privacy.session_size);
    let mut cfg = config(privacy);
    cfg.chain = el_model::Chain::Mainnet;
    cfg.tor_socks = Some(socks.clone());
    cfg.tor_connect_timeout_seconds = 10;
    cfg.tor_stall_timeout_seconds = 12;
    cfg.electrum_servers = electrum.clone();
    cfg.use_esplora = true;
    cfg.esplora_urls = vec![api.to_string()];
    let r = Arc::new(Resolver::new(Arc::clone(&store), &cfg).unwrap());
    eprintln!(
        "{} txs in {} blocks, {} electrum servers, tx mode over {:?} servers",
        wanted.len(),
        blocks,
        electrum.len(),
        r.tx_mode_servers()
    );
    let started = Instant::now();
    let handles: Vec<_> = wanted
        .iter()
        .map(|(id, h, _)| {
            let r = Arc::clone(&r);
            let (id, h) = (*id, *h);
            std::thread::spawn(move || {
                let t = Instant::now();
                (r.raw_tx_for(1, id, h).is_ok(), t.elapsed())
            })
        })
        .collect();
    let mut lat: Vec<f64> = Vec::new();
    let mut failed = 0;
    for h in handles {
        let (ok, took) = h.join().unwrap();
        if ok {
            lat.push(took.as_secs_f64());
        } else {
            failed += 1;
        }
    }
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| {
        lat.get(((lat.len() as f64 - 1.0) * q) as usize)
            .copied()
            .unwrap_or(0.0)
    };
    let st = r.stats();
    eprintln!(
        "tx mode: {} ok, {failed} failed in {:.1}s; p50 {:.1}s p90 {:.1}s max {:.1}s; sessions {} real {} decoys {} verified {} rejected {} fallback {}",
        lat.len(),
        started.elapsed().as_secs_f64(),
        p(0.5),
        p(0.9),
        p(1.0),
        st.tx_sessions.load(Ordering::Relaxed),
        st.tx_real.load(Ordering::Relaxed),
        st.tx_decoys.load(Ordering::Relaxed),
        st.tx_verified.load(Ordering::Relaxed),
        st.tx_verify_failed.load(Ordering::Relaxed),
        st.tx_fallback.load(Ordering::Relaxed),
    );
    let _ = std::fs::remove_dir_all(&dir);
}
