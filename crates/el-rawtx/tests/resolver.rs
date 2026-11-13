//! Resolver ordering, cache and decoy behaviour with synthetic blocks.

use bitcoin::absolute::LockTime;
use bitcoin::block::Header;
use bitcoin::hashes::Hash;
use bitcoin::transaction::Version;
use bitcoin::{
    Amount, Block, BlockHash, CompactTarget, OutPoint, ScriptBuf, Sequence, Transaction, TxIn,
    TxMerkleNode, TxOut, Witness,
};
use el_model::codec::SCHEMA_VERSION;
use el_model::{BlockRecord, Hash32, IndexedTip};
use el_rawtx::{BlockSource, RawtxConfig, RawtxError, Resolver, Result};
use el_store::Store;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

struct MockSource {
    name: String,
    blocks: Vec<u8>,
    calls: AtomicU64,
    corrupt: bool,
}

impl MockSource {
    fn new(name: &str, blocks: Vec<u8>, corrupt: bool) -> Arc<Self> {
        Arc::new(MockSource {
            name: name.to_string(),
            blocks,
            calls: AtomicU64::new(0),
            corrupt,
        })
    }
}

impl BlockSource for MockSource {
    fn name(&self) -> &str {
        &self.name
    }
    fn fetch_block(&self, _hash: &Hash32) -> Result<Vec<u8>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.corrupt {
            let mut b = self.blocks.clone();
            b.truncate(b.len() / 2);
            return Ok(b);
        }
        Ok(self.blocks.clone())
    }
}

struct SlowSource {
    blocks: Vec<u8>,
    calls: AtomicU64,
}

impl SlowSource {
    fn new(blocks: Vec<u8>) -> Arc<Self> {
        Arc::new(SlowSource {
            blocks,
            calls: AtomicU64::new(0),
        })
    }
}

impl BlockSource for SlowSource {
    fn name(&self) -> &str {
        "slow"
    }
    fn fetch_block(&self, _hash: &Hash32) -> Result<Vec<u8>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(100));
        Ok(self.blocks.clone())
    }
}

fn test_block() -> Block {
    let coinbase = Transaction {
        version: Version::TWO,
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

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "e-lite-resolver-{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// Store a chain of dummy headers plus the real block at `height`.
fn setup(dir: &PathBuf, height: u32) -> (Arc<Store>, Hash32, Hash32, Vec<u8>) {
    setup_with(dir, height, test_block())
}

/// Like `setup`, with any block at `height`. Returns the first txid.
fn setup_with(dir: &PathBuf, height: u32, block: Block) -> (Arc<Store>, Hash32, Hash32, Vec<u8>) {
    let store = Arc::new(Store::open(dir).unwrap());
    let bytes = bitcoin::consensus::serialize(&block);
    let hash = Hash32::from_hex(&block.block_hash().to_string()).unwrap();
    let header: [u8; 80] = bitcoin::consensus::serialize(&block.header)
        .try_into()
        .unwrap();
    let txid = Hash32::from_hex(&block.txdata[0].compute_txid().to_string()).unwrap();

    let mut batch = Store::batch();
    for h in 0..=height {
        let rec = BlockRecord {
            height: h,
            hash: if h == height {
                hash
            } else {
                Hash32([h as u8; 32])
            },
            header: if h == height { header } else { [h as u8; 80] },
            tx_count: if h == height {
                block.txdata.len() as u32
            } else {
                1
            },
            txnum_base: h as u64,
        };
        store.put_block(&mut batch, &rec).unwrap();
    }
    store.put_occurrence(&mut batch, txid, height, 0).unwrap();
    store
        .commit(
            batch,
            &IndexedTip {
                height,
                block_hash: hash,
                schema_version: SCHEMA_VERSION,
                txnum: height as u64 + 1,
            },
            &[txid],
        )
        .unwrap();
    (store, hash, txid, bytes)
}

fn config(decoys: u32, cache_mib: u64) -> RawtxConfig {
    RawtxConfig {
        enabled: true,
        cache_mib,
        decoys,
        tor_socks: None,
        allow_clearnet: true,
        fallback_clearnet: false,
        request_timeout_seconds: 15,
        tor_request_timeout_seconds: 15,
        provider_cooldown_seconds: 120,
        max_parallel_fetches: 4,
        cache_dir: None,
        use_p2p: false,
        use_esplora: false,
        p2p_v2: false,
        esplora_urls: Vec::new(),
        p2p_peers: Vec::new(),
        p2p_timeout: Duration::from_secs(1),
        chain: el_model::Chain::Regtest,
    }
}

#[test]
fn serves_from_external_source_and_then_cache() {
    let dir = tmp("cache");
    let (store, _hash, txid, bytes) = setup(&dir, 0);
    let src = MockSource::new("mock", bytes, false);
    let resolver = Resolver::new(Arc::clone(&store), &config(0, 1))
        .unwrap()
        .with_external(src.clone());
    let raw = resolver.raw_tx(txid, 0).unwrap();
    assert!(raw.len() > 20);
    assert_eq!(src.calls.load(Ordering::SeqCst), 1);
    let raw2 = resolver.raw_tx(txid, 0).unwrap();
    assert_eq!(raw, raw2);
    assert_eq!(
        src.calls.load(Ordering::SeqCst),
        1,
        "second call must hit cache"
    );
    assert_eq!(resolver.stats().cache_hits.load(Ordering::SeqCst), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn concurrent_requests_share_one_block_download() {
    use std::sync::Barrier;

    let dir = tmp("inflight");
    let (store, _hash, txid, bytes) = setup(&dir, 0);
    let src = SlowSource::new(bytes);
    let resolver = Arc::new(
        Resolver::new(Arc::clone(&store), &config(0, 0))
            .unwrap()
            .with_external(src.clone()),
    );
    let barrier = Arc::new(Barrier::new(8));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let resolver = Arc::clone(&resolver);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            resolver.raw_tx(txid, 0)
        }));
    }
    for handle in handles {
        assert!(handle.join().unwrap().is_ok());
    }
    assert_eq!(
        src.calls.load(Ordering::SeqCst),
        1,
        "concurrent requests must share one download"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetches_verified_block_by_hash_for_prune_repair() {
    let dir = tmp("repair");
    let store = Arc::new(Store::open(&dir).unwrap());
    let block = test_block();
    let bytes = bitcoin::consensus::serialize(&block);
    let hash = Hash32::from_hex(&block.block_hash().to_string()).unwrap();
    let src = MockSource::new("mock", bytes.clone(), false);
    let resolver = Resolver::new(Arc::clone(&store), &config(0, 1))
        .unwrap()
        .with_external(src.clone());
    assert!(resolver.has_repair_sources());
    let got = resolver.fetch_block_by_hash(&hash).unwrap();
    assert_eq!(got, bytes);
    assert_eq!(src.calls.load(Ordering::SeqCst), 1);
    // The repaired block is cached and reused without another network call.
    let got2 = resolver.fetch_block_by_hash(&hash).unwrap();
    assert_eq!(got2, bytes);
    assert_eq!(src.calls.load(Ordering::SeqCst), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn corrupt_source_alone_is_an_explicit_error() {
    let dir = tmp("corrupt-only");
    let (store, _hash, txid, bytes) = setup(&dir, 0);
    let bad = MockSource::new("bad", bytes, true);
    let resolver = Resolver::new(Arc::clone(&store), &config(0, 0))
        .unwrap()
        .with_external(bad.clone());
    let err = resolver.raw_tx(txid, 0).unwrap_err();
    assert!(matches!(err, RawtxError::Unavailable(_)), "{err}");
    assert!(resolver.stats().verify_failures.load(Ordering::SeqCst) >= 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn falls_back_to_a_good_source_after_a_corrupt_one() {
    let dir = tmp("corrupt-fallback");
    let (store, _hash, txid, bytes) = setup(&dir, 0);
    let bad = MockSource::new("bad", bytes.clone(), true);
    let good = MockSource::new("good", bytes, false);
    // Sources are shuffled per request; over repeated fresh resolvers (no
    // cache, no block kept in memory) the corrupt one is tried first at least
    // once and the valid one is always reached.
    let mut failures = 0;
    for _ in 0..20 {
        let resolver = Resolver::new(Arc::clone(&store), &config(0, 0))
            .unwrap()
            .with_external(bad.clone())
            .with_external(good.clone());
        assert!(resolver.raw_tx(txid, 0).is_ok());
        failures += resolver.stats().verify_failures.load(Ordering::SeqCst);
    }
    assert!(failures >= 1);
    assert!(good.calls.load(Ordering::SeqCst) >= 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prefers_local_source_over_external() {
    let dir = tmp("local");
    let (store, _hash, txid, bytes) = setup(&dir, 0);
    let local = MockSource::new("local", bytes.clone(), false);
    let external = MockSource::new("external", bytes, false);
    let resolver = Resolver::new(Arc::clone(&store), &config(0, 1))
        .unwrap()
        .with_local(local.clone())
        .with_external(external.clone());
    resolver.raw_tx(txid, 0).unwrap();
    assert_eq!(local.calls.load(Ordering::SeqCst), 1);
    assert_eq!(external.calls.load(Ordering::SeqCst), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn decoys_are_fetched_in_background() {
    let dir = tmp("decoys");
    let (store, _hash, txid, bytes) = setup(&dir, 3);
    let src = MockSource::new("mock", bytes, false);
    let resolver = Resolver::new(Arc::clone(&store), &config(2, 1))
        .unwrap()
        .with_external(src.clone());
    resolver.raw_tx(txid, 3).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while resolver.stats().decoys.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        resolver.stats().decoys.load(Ordering::SeqCst) >= 2,
        "decoys were not fetched"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn no_sources_means_explicit_error() {
    let dir = tmp("none");
    let (store, _hash, txid, _bytes) = setup(&dir, 0);
    let resolver = Resolver::new(Arc::clone(&store), &config(0, 0)).unwrap();
    let err = resolver.raw_tx(txid, 0).unwrap_err();
    // Distinct from "every source failed": nothing could be asked.
    assert!(matches!(err, RawtxError::NoSource(_)), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Live check against Blockstream's signet Esplora (clearnet, no Tor).
/// Run with: cargo test -p el-rawtx --test resolver live_esplora -- --ignored
#[test]
#[ignore]
fn live_esplora_signet_block() {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let base = "https://blockstream.info/signet/api";
    let hash_at = |height: u64| -> String {
        client
            .get(format!("{base}/block-height/{height}"))
            .send()
            .unwrap()
            .text()
            .unwrap()
            .trim()
            .to_string()
    };
    let hash = Hash32::from_hex(&hash_at(1000)).unwrap();
    let src = el_rawtx::EsploraSource::new(
        vec![base.to_string()],
        None,
        true,
        false,
        Duration::from_secs(30),
        Duration::from_secs(120),
    )
    .unwrap();
    let bytes = src.fetch_block(&hash).unwrap();
    let block: Block = bitcoin::consensus::deserialize(&bytes).unwrap();
    let got = Hash32::from_hex(&block.block_hash().to_string()).unwrap();
    assert_eq!(got, hash);
    eprintln!("live esplora signet block 1000: {} bytes", bytes.len());
}

/// Live end-to-end external resolution: fetch a real signet tip block through
/// Esplora, verify it against a locally stored header, then hit the cache.
#[test]
#[ignore]
fn live_esplora_resolver_tip() {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();
    let base = "https://blockstream.info/signet/api";
    let height: u32 = client
        .get(format!("{base}/blocks/tip/height"))
        .send()
        .unwrap()
        .text()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let hash_hex: String = client
        .get(format!("{base}/block-height/{height}"))
        .send()
        .unwrap()
        .text()
        .unwrap()
        .trim()
        .to_string();
    let raw = client
        .get(format!("{base}/block/{hash_hex}/raw"))
        .send()
        .unwrap()
        .bytes()
        .unwrap();
    let block: Block = bitcoin::consensus::deserialize(&raw).unwrap();
    let hash = Hash32::from_hex(&hash_hex).unwrap();

    let dir = tmp("live-esplora");
    let store = Arc::new(Store::open(&dir).unwrap());
    // Pick a transaction with witness if possible (exercises the commitment).
    let (pos, tx) = block
        .txdata
        .iter()
        .enumerate()
        .find(|(_, tx)| tx.input.iter().any(|i| !i.witness.is_empty()))
        .unwrap_or((0, &block.txdata[0]));
    let txid = Hash32::from_hex(&tx.compute_txid().to_string()).unwrap();
    let header: [u8; 80] = bitcoin::consensus::serialize(&block.header)
        .try_into()
        .unwrap();
    let mut batch = Store::batch();
    store
        .put_block(
            &mut batch,
            &BlockRecord {
                height,
                hash,
                header,
                tx_count: block.txdata.len() as u32,
                txnum_base: 0,
            },
        )
        .unwrap();
    store
        .put_occurrence(&mut batch, txid, height, pos as u32)
        .unwrap();
    store
        .commit(
            batch,
            &IndexedTip {
                height,
                block_hash: hash,
                schema_version: SCHEMA_VERSION,
                txnum: 1,
            },
            &[txid],
        )
        .unwrap();

    let src = Arc::new(
        el_rawtx::EsploraSource::new(
            vec![base.to_string()],
            None,
            true,
            false,
            Duration::from_secs(60),
            Duration::from_secs(120),
        )
        .unwrap(),
    );
    let resolver = Resolver::new(Arc::clone(&store), &config(0, 1))
        .unwrap()
        .with_external(src);
    let hex = resolver.raw_tx(txid, height).unwrap();
    let bytes = hex::decode(&hex).unwrap();
    let got: Transaction = bitcoin::consensus::deserialize(&bytes).unwrap();
    assert_eq!(
        Hash32::from_hex(&got.compute_txid().to_string()).unwrap(),
        txid
    );
    let _ = resolver.raw_tx(txid, height).unwrap();
    assert!(resolver.stats().cache_hits.load(Ordering::SeqCst) >= 1);
    eprintln!(
        "live esplora resolver: height {height}, tx {txid}, {} bytes",
        bytes.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Live P2P handshake against a real Bitcoin Core (regtest).
/// E_LITE_P2P_PEER=127.0.0.1:18458 E_LITE_P2P_HASH=<hash> cargo test ... --ignored
#[test]
#[ignore]
fn live_p2p_regtest() {
    let peer = std::env::var("E_LITE_P2P_PEER").expect("E_LITE_P2P_PEER");
    let hash =
        Hash32::from_hex(&std::env::var("E_LITE_P2P_HASH").expect("E_LITE_P2P_HASH")).unwrap();
    let v2 = std::env::var("E_LITE_P2P_V2")
        .map(|v| v != "0")
        .unwrap_or(true);
    let src = el_rawtx::P2pSource::new(
        vec![peer],
        None,
        Duration::from_secs(10),
        el_model::Chain::Regtest,
        v2,
        false,
        Duration::from_secs(5),
    );
    let bytes = src.fetch_block(&hash).unwrap();
    let block: Block = bitcoin::consensus::deserialize(&bytes).unwrap();
    assert_eq!(
        Hash32::from_hex(&block.block_hash().to_string()).unwrap(),
        hash
    );
    eprintln!("live p2p: {} bytes", bytes.len());
}

#[test]
fn external_sources_fail_closed_without_tor() {
    let dir = tmp("fail-closed");
    let (store, _hash, _txid, _bytes) = setup(&dir, 0);
    let mut cfg = config(0, 0);
    cfg.use_p2p = true;
    cfg.p2p_peers = vec!["127.0.0.1:1".to_string()];
    cfg.use_esplora = true;
    cfg.esplora_urls = vec!["https://example.invalid/api".to_string()];
    cfg.tor_socks = None;
    cfg.allow_clearnet = false;
    let resolver = Resolver::new(Arc::clone(&store), &cfg).unwrap();
    assert_eq!(resolver.external_sources(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn p2p_source_counts_only_once_it_has_peers() {
    // AUD-137: P2P exists even when Core had no peers at start, and the list
    // can be refreshed later. An empty P2P source is not a usable source.
    let dir = tmp("p2p-refresh");
    let store = Arc::new(Store::open(&dir).unwrap());
    let mut cfg = config(0, 0);
    cfg.use_p2p = true;
    let resolver = Resolver::new(Arc::clone(&store), &cfg).unwrap();
    assert_eq!(resolver.p2p_peers(), Some(0));
    assert_eq!(resolver.external_sources(), 0);
    assert!(!resolver.has_repair_sources());

    let count = resolver.refresh_p2p_peers(vec![
        "192.0.2.1:18444".into(),
        "192.0.2.2:18444".into(),
        "192.0.2.1:18444".into(),
    ]);
    assert_eq!(count, Some(2), "duplicates are dropped");
    assert_eq!(resolver.external_sources(), 1);
    assert!(resolver.has_repair_sources());

    assert_eq!(resolver.refresh_p2p_peers(Vec::new()), Some(0));
    assert_eq!(resolver.external_sources(), 0);

    // P2P disabled: nothing to refresh.
    let off = Resolver::new(Arc::clone(&store), &config(0, 0)).unwrap();
    assert_eq!(off.p2p_peers(), None);
    assert_eq!(off.refresh_p2p_peers(vec!["192.0.2.3:18444".into()]), None);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A block with a coinbase and `extra` plain transactions.
fn multi_tx_block(extra: u8) -> Block {
    let mut block = test_block();
    for i in 0..extra {
        block.txdata.push(Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: bitcoin::Txid::from_byte_array([i + 1; 32]),
                    vout: 0,
                },
                script_sig: ScriptBuf::from_bytes(vec![i]),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(10 + i as u64),
                script_pubkey: ScriptBuf::from_bytes(vec![0x51, i]),
            }],
        });
    }
    block.header.merkle_root = block.compute_merkle_root().unwrap();
    block
}

fn txid_of(tx: &Transaction) -> Hash32 {
    Hash32::from_hex(&tx.compute_txid().to_string()).unwrap()
}

#[test]
fn transaction_cache_serves_after_restart_without_any_source() {
    // AUD-139: transactions, not blocks, are cached on disk, and they are
    // authenticated against the indexed header after a restart.
    let dir = tmp("txcache-restart");
    let block = multi_tx_block(3);
    let wanted = txid_of(&block.txdata[2]);
    let (store, _hash, _txid, bytes) = setup_with(&dir, 0, block);
    let src = MockSource::new("mock", bytes, false);
    let first = Resolver::new(Arc::clone(&store), &config(0, 8))
        .unwrap()
        .with_external(src.clone());
    let raw = first.raw_tx(wanted, 0).unwrap();
    assert_eq!(src.calls.load(Ordering::SeqCst), 1);
    assert_eq!(first.cached_txs(), 1);
    let cache_dir = first.cache_dir().to_path_buf();
    drop(first);
    // No whole block was written for a wallet request.
    let blocks_on_disk = std::fs::read_dir(&cache_dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".blk"))
        .count();
    assert_eq!(blocks_on_disk, 0);

    let silent = MockSource::new("silent", Vec::new(), true);
    let second = Resolver::new(Arc::clone(&store), &config(0, 8))
        .unwrap()
        .with_external(silent.clone());
    assert_eq!(second.raw_tx(wanted, 0).unwrap(), raw);
    assert_eq!(silent.calls.load(Ordering::SeqCst), 0, "served from disk");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_burst_and_a_prefetch_need_one_block_download() {
    let dir = tmp("txcache-burst");
    let block = multi_tx_block(4);
    let ids: Vec<Hash32> = block.txdata.iter().map(txid_of).collect();
    let (store, _hash, _txid, bytes) = setup_with(&dir, 0, block);
    let src = MockSource::new("mock", bytes, false);
    let resolver = Resolver::new(Arc::clone(&store), &config(0, 8))
        .unwrap()
        .with_external(src.clone());
    // Prefetch caches every listed transaction from one download ...
    assert_eq!(resolver.prefetch_block_txs(0, &ids[1..3]).unwrap(), 2);
    assert_eq!(src.calls.load(Ordering::SeqCst), 1);
    assert_eq!(resolver.prefetch_block_txs(0, &ids[1..3]).unwrap(), 0);
    // ... and other transactions of the same block come from memory.
    for id in &ids {
        resolver.raw_tx(*id, 0).unwrap();
    }
    assert_eq!(src.calls.load(Ordering::SeqCst), 1);
    assert_eq!(resolver.cached_txs(), ids.len());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_cached_transaction_that_does_not_match_the_header_is_dropped() {
    let dir = tmp("txcache-forged");
    let block = multi_tx_block(1);
    let id = txid_of(&block.txdata[1]);
    let (store, _hash, _txid, bytes) = setup_with(&dir, 0, block);
    let src = MockSource::new("mock", bytes, false);
    let resolver = Resolver::new(Arc::clone(&store), &config(0, 8))
        .unwrap()
        .with_external(src.clone());
    resolver.raw_tx(id, 0).unwrap();
    let cache_dir = resolver.cache_dir().join("tx");
    drop(resolver);
    // Rewrite the stored position: the branch no longer reproduces the root.
    let hex = id.to_hex();
    let path = cache_dir.join(&hex[..2]).join(format!("{hex}.tx"));
    let mut entry = std::fs::read(&path).unwrap();
    entry[8..12].copy_from_slice(&0u32.to_be_bytes());
    std::fs::write(&path, entry).unwrap();

    let resolver = Resolver::new(Arc::clone(&store), &config(0, 8))
        .unwrap()
        .with_external(src.clone());
    resolver.raw_tx(id, 0).unwrap();
    assert!(resolver.stats().verify_failures.load(Ordering::SeqCst) >= 1);
    assert_eq!(
        src.calls.load(Ordering::SeqCst),
        2,
        "refetched after rejection"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cache_budget_is_split_between_blocks_and_transactions() {
    assert_eq!(el_rawtx::cache_split(0), (0, 0));
    assert_eq!(el_rawtx::cache_split(1), (1, 0));
    assert_eq!(el_rawtx::cache_split(8), (2, 6));
    assert_eq!(el_rawtx::cache_split(256), (64, 192));
    assert_eq!(el_rawtx::cache_split(1024), (64, 960));
}
