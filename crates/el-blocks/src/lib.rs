//! Read-only bulk access to Bitcoin Core's block files.
//!
//! Core stores raw blocks in `blocks/blkNNNNN.dat` as
//! `magic[4] || size_le[4] || block[size]`. Since Core 28 the files are
//! obfuscated by default: the 8-byte key in `blocks/xor.dat` is XORed over the
//! whole file, repeating and aligned to the absolute file offset. The key is a
//! sequence of zeros for an existing (pre-XOR) blocksdir or `-blocksxor=0`.
//!
//! This module never writes to the Core datadir. It only builds a sidecar
//! locator (`blockfiles.idx`) in e-lite's own data directory. Core remains the
//! authority for the active chain: callers must resolve heights with
//! `getblockhash` and pass only those hashes here.

use el_model::{Chain, Hash32};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const SIDECAR_NAME: &str = "blockfiles.idx";
pub const HEADER_LEN: usize = 80;
pub const MAX_BLOCK_SIZE: u32 = 8 * 1024 * 1024;

const SIDECAR_MAGIC: &[u8; 4] = b"ELBF";
const SIDECAR_VERSION: u32 = 1;
const LOC_RECORD: usize = 48; // hash[32] + file u32 + offset u64 + size u32
const SCAN_POLL: Duration = Duration::from_millis(200);
const SIDECAR_RECORDS_PER_SYNC: usize = 4096;

#[derive(Debug)]
pub enum BulkError {
    Io(String),
    MissingBlocksDir(PathBuf),
    XorKey(String),
    Sidecar(String),
    Corrupt {
        file: u32,
        offset: u64,
        reason: String,
    },
    NotAvailable(String),
}

impl fmt::Display for BulkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BulkError::Io(m) => write!(f, "block files: {m}"),
            BulkError::MissingBlocksDir(p) => {
                write!(f, "block files directory does not exist: {}", p.display())
            }
            BulkError::XorKey(m) => write!(f, "block files xor key: {m}"),
            BulkError::Sidecar(m) => write!(f, "block files locator: {m}"),
            BulkError::Corrupt {
                file,
                offset,
                reason,
            } => write!(
                f,
                "corrupt block file blk{file:05}.dat at offset {offset}: {reason}"
            ),
            BulkError::NotAvailable(m) => write!(f, "bulk block source unavailable: {m}"),
        }
    }
}

impl std::error::Error for BulkError {}

impl From<io::Error> for BulkError {
    fn from(e: io::Error) -> Self {
        BulkError::Io(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, BulkError>;

/// Location of one raw block inside a Core block file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockLocation {
    pub file: u32,
    pub offset: u64,
    pub size: u32,
}

/// The XOR key Core uses for `blk*.dat` (empty/zero for old datadirs).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct XorKey([u8; 8]);

impl fmt::Debug for XorKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print the key itself.
        if self.is_zero() {
            write!(f, "XorKey(zero)")
        } else {
            write!(f, "XorKey(set)")
        }
    }
}

impl XorKey {
    pub fn load(blocks_dir: &Path) -> Result<Self> {
        let path = blocks_dir.join("xor.dat");
        match std::fs::read(&path) {
            Ok(v) if v.len() == 8 => {
                let mut key = [0u8; 8];
                key.copy_from_slice(&v);
                Ok(XorKey(key))
            }
            Ok(v) => Err(BulkError::XorKey(format!(
                "{} is {} bytes, expected 8",
                path.display(),
                v.len()
            ))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(XorKey([0u8; 8])),
            Err(e) => Err(BulkError::XorKey(format!("{}: {e}", path.display()))),
        }
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; 8]
    }

    fn fingerprint(&self) -> Hash32 {
        let mut h = Sha256::new();
        h.update(b"e-lite-xor-v1\0");
        h.update(self.0);
        let mut out = [0u8; 32];
        out.copy_from_slice(&h.finalize());
        Hash32(out)
    }

    /// XOR in place using the absolute file offset for key alignment.
    fn apply(&self, offset: u64, buf: &mut [u8]) {
        if self.is_zero() {
            return;
        }
        let mut k = (offset % 8) as usize;
        for b in buf.iter_mut() {
            *b ^= self.0[k];
            k += 1;
            if k == 8 {
                k = 0;
            }
        }
    }
}

/// Derive Core's blocks directory from a datadir.
pub fn default_blocks_dir(core_datadir: &Path, chain: Chain) -> PathBuf {
    let sub = chain.core_subdir();
    if sub.is_empty() {
        core_datadir.join("blocks")
    } else {
        core_datadir.join(sub).join("blocks")
    }
}

fn header_hash(header: &[u8]) -> Result<Hash32> {
    if header.len() != HEADER_LEN {
        return Err(BulkError::Corrupt {
            file: 0,
            offset: 0,
            reason: format!("header is {} bytes", header.len()),
        });
    }
    let mut h = Sha256::new();
    h.update(header);
    let first = h.finalize();
    let mut h2 = Sha256::new();
    h2.update(first);
    let digest = h2.finalize();
    let mut raw = [0u8; 32];
    raw.copy_from_slice(&digest);
    Ok(Hash32(raw).reversed())
}

fn read_at(file: &File, offset: u64, buf: &mut [u8]) -> Result<bool> {
    match file.read_exact_at(buf, offset) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn list_block_files(dir: &Path) -> Result<Vec<(u32, PathBuf)>> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir(dir)
        .map_err(|e| BulkError::Io(format!("cannot list {}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(num) = name
            .strip_prefix("blk")
            .and_then(|n| n.strip_suffix(".dat"))
        else {
            continue;
        };
        if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(n) = num.parse::<u32>() {
            out.push((n, entry.path()));
        }
    }
    out.sort_by_key(|(n, _)| *n);
    Ok(out)
}

fn file_name(num: u32) -> String {
    format!("blk{num:05}.dat")
}

// ---------------------------------------------------------------- locator map

/// Concurrent hash -> location map. The scanner inserts, readers look up.
#[derive(Default)]
pub struct BlockFileIndex {
    inner: Mutex<HashMap<Hash32, BlockLocation>>,
    cv: Condvar,
}

impl BlockFileIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map(|m| m.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn insert(&self, hash: Hash32, loc: BlockLocation) {
        if let Ok(mut m) = self.inner.lock() {
            if m.insert(hash, loc).is_none() {
                self.cv.notify_all();
            }
        }
    }

    pub fn get(&self, hash: &Hash32) -> Option<BlockLocation> {
        self.inner.lock().ok().and_then(|m| m.get(hash).copied())
    }

    pub fn wait_for(&self, hash: &Hash32, timeout: Duration) -> Option<BlockLocation> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.inner.lock().ok()?;
        loop {
            if let Some(loc) = guard.get(hash) {
                return Some(*loc);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (next, _) = self.cv.wait_timeout(guard, deadline - now).ok()?;
            guard = next;
        }
    }
}

// -------------------------------------------------------------------- sidecar

fn sidecar_header(chain: Chain, genesis: &Hash32, xor: &XorKey) -> Vec<u8> {
    let name = chain.name().as_bytes();
    let mut out = Vec::with_capacity(8 + 1 + name.len() + 64);
    out.extend_from_slice(SIDECAR_MAGIC);
    out.extend_from_slice(&SIDECAR_VERSION.to_le_bytes());
    out.push(name.len() as u8);
    out.extend_from_slice(name);
    out.extend_from_slice(genesis.as_bytes());
    out.extend_from_slice(xor.fingerprint().as_bytes());
    out
}

struct SidecarHeader {
    chain: Chain,
    genesis: Hash32,
    len: usize,
}

fn parse_sidecar_header(buf: &[u8]) -> Option<SidecarHeader> {
    if buf.len() < 9 || &buf[0..4] != SIDECAR_MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(buf[4..8].try_into().ok()?);
    if version != SIDECAR_VERSION {
        return None;
    }
    let name_len = buf[8] as usize;
    let name_end = 9 + name_len;
    if buf.len() < name_end + 64 {
        return None;
    }
    let chain = std::str::from_utf8(&buf[9..name_end])
        .ok()
        .and_then(Chain::from_name)?;
    let genesis = Hash32::from_slice(&buf[name_end..name_end + 32]).ok()?;
    // The XOR fingerprint is part of the raw header and is compared by the
    // caller; it is not needed as a typed field here.
    Hash32::from_slice(&buf[name_end + 32..name_end + 64]).ok()?;
    Some(SidecarHeader {
        chain,
        genesis,
        len: name_end + 64,
    })
}

struct Sidecar {
    out: BufWriter<File>,
    records: usize,
    last_sync: Instant,
}

impl Sidecar {
    fn create(path: &Path, header: &[u8]) -> Result<Self> {
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(path)
            .map_err(|e| BulkError::Sidecar(format!("{}: {e}", path.display())))?;
        file.write_all(header)?;
        file.sync_data()?;
        file.seek(SeekFrom::Start(header.len() as u64))?;
        Ok(Sidecar {
            out: BufWriter::new(file),
            records: 0,
            last_sync: Instant::now(),
        })
    }

    fn open_append(path: &Path) -> Result<Self> {
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)
            .map_err(|e| BulkError::Sidecar(format!("{}: {e}", path.display())))?;
        file.seek(SeekFrom::End(0))?;
        Ok(Sidecar {
            out: BufWriter::new(file),
            records: 0,
            last_sync: Instant::now(),
        })
    }

    fn append(&mut self, hash: &Hash32, loc: &BlockLocation) -> Result<()> {
        let mut rec = [0u8; LOC_RECORD];
        rec[0..32].copy_from_slice(hash.as_bytes());
        rec[32..36].copy_from_slice(&loc.file.to_le_bytes());
        rec[36..44].copy_from_slice(&loc.offset.to_le_bytes());
        rec[44..48].copy_from_slice(&loc.size.to_le_bytes());
        self.out.write_all(&rec)?;
        self.records += 1;
        if self.records >= SIDECAR_RECORDS_PER_SYNC
            || self.last_sync.elapsed() >= Duration::from_secs(1)
        {
            self.flush_sync()?;
        }
        Ok(())
    }

    fn flush_sync(&mut self) -> Result<()> {
        self.out.flush()?;
        self.out.get_ref().sync_data()?;
        self.records = 0;
        self.last_sync = Instant::now();
        Ok(())
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        let _ = self.flush_sync();
    }
}

struct LoadedSidecar {
    map: HashMap<Hash32, BlockLocation>,
    cursor: (u32, u64),
}

fn load_sidecar(
    path: &Path,
    chain: Chain,
    genesis: &Hash32,
    xor: &XorKey,
) -> Result<LoadedSidecar> {
    if !path.exists() {
        return Ok(LoadedSidecar {
            map: HashMap::new(),
            cursor: (0, 0),
        });
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| BulkError::Sidecar(format!("{}: {e}", path.display())))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let expected = sidecar_header(chain, genesis, xor);
    let Some(header) = parse_sidecar_header(&buf) else {
        return Ok(LoadedSidecar {
            map: HashMap::new(),
            cursor: (0, 0),
        });
    };
    if header.len != expected.len()
        || buf[..header.len] != expected[..]
        || header.chain != chain
        || header.genesis != *genesis
    {
        return Ok(LoadedSidecar {
            map: HashMap::new(),
            cursor: (0, 0),
        });
    }
    let mut map = HashMap::new();
    let mut cursor = (0u32, 0u64);
    let mut off = header.len;
    while off + LOC_RECORD <= buf.len() {
        let rec = &buf[off..off + LOC_RECORD];
        let Ok(hash) = Hash32::from_slice(&rec[0..32]) else {
            break;
        };
        let file_no = u32::from_le_bytes(rec[32..36].try_into().unwrap());
        let offset = u64::from_le_bytes(rec[36..44].try_into().unwrap());
        let size = u32::from_le_bytes(rec[44..48].try_into().unwrap());
        // The locator is derived data: a corrupt or torn record truncates the
        // valid prefix and the scanner rebuilds from there.
        if !(81..=MAX_BLOCK_SIZE).contains(&size) || offset > (1 << 40) {
            break;
        }
        let loc = BlockLocation {
            file: file_no,
            offset,
            size,
        };
        map.insert(hash, loc);
        cursor = (file_no, offset + 8 + size as u64);
        off += LOC_RECORD;
    }
    if buf.len() != off {
        // A torn trailing record: keep the valid prefix.
        let _ = file.set_len(off as u64);
        let _ = file.sync_data();
    }
    Ok(LoadedSidecar { map, cursor })
}

// -------------------------------------------------------------------- scanner

struct Scanner {
    dir: PathBuf,
    magic: [u8; 4],
    xor: XorKey,
    index: Arc<BlockFileIndex>,
    stop: Arc<AtomicBool>,
    errors: Arc<Mutex<Option<String>>>,
    sidecar_path: PathBuf,
    start: (u32, u64),
}

impl Scanner {
    fn set_error(&self, msg: impl Into<String>) {
        if let Ok(mut e) = self.errors.lock() {
            *e = Some(msg.into());
        }
    }

    fn peek_magic(&self, file: &File, offset: u64) -> Result<Option<(u32, [u8; 4])>> {
        let mut hdr = [0u8; 8];
        if !read_at(file, offset, &mut hdr)? {
            return Ok(None);
        }
        self.xor.apply(offset, &mut hdr);
        let mut magic = [0u8; 4];
        magic.copy_from_slice(&hdr[0..4]);
        let size = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
        if magic != self.magic || !(81..=MAX_BLOCK_SIZE).contains(&size) {
            return Ok(None);
        }
        Ok(Some((size, magic)))
    }

    fn run(self) {
        if let Err(e) = self.run_inner() {
            self.set_error(e.to_string());
            eprintln!("[e-lite] bulk scanner: {e}");
        }
    }

    fn run_inner(&self) -> Result<()> {
        let mut files = list_block_files(&self.dir)?;
        let mut file_idx = files
            .iter()
            .position(|(n, _)| *n >= self.start.0)
            .unwrap_or(files.len());
        let mut offset = if file_idx < files.len() && files[file_idx].0 == self.start.0 {
            self.start.1
        } else {
            0
        };
        let mut side = Sidecar::open_append(&self.sidecar_path)?;

        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            if file_idx >= files.len() {
                thread::sleep(SCAN_POLL);
                files = list_block_files(&self.dir)?;
                if let Some(pos) = files.iter().position(|(n, _)| *n >= self.start.0) {
                    if file_idx >= files.len() || files[file_idx].0 < self.start.0 {
                        file_idx = pos;
                        offset = if files[pos].0 == self.start.0 {
                            self.start.1
                        } else {
                            0
                        };
                    }
                }
                continue;
            }

            let (num, path) = files[file_idx].clone();
            let file = match File::open(&path) {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    file_idx += 1;
                    offset = 0;
                    continue;
                }
                Err(e) => {
                    self.set_error(format!("cannot open {}: {e}", path.display()));
                    thread::sleep(SCAN_POLL);
                    continue;
                }
            };

            // Scan complete records. The last record is only persisted once the
            // next record starts, so a partial write is retried instead of lost.
            loop {
                if self.stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let Some((size, _)) = self.peek_magic(&file, offset)? else {
                    break;
                };
                let mut h = [0u8; HEADER_LEN];
                if !read_at(&file, offset + 8, &mut h)? {
                    break;
                }
                self.xor.apply(offset + 8, &mut h);
                let hash = header_hash(&h)?;
                let loc = BlockLocation {
                    file: num,
                    offset,
                    size,
                };
                let next = offset + 8 + size as u64;
                if self.peek_magic(&file, next)?.is_some() {
                    self.index.insert(hash, loc);
                    side.append(&hash, &loc)?;
                    offset = next;
                } else {
                    // Tip of the written area: expose it to readers, but do not
                    // advance the durable cursor until the next record appears.
                    self.index.insert(hash, loc);
                    break;
                }
            }

            // At the tail: wait, then either retry the same offset (Core is
            // still writing this file) or move to the next rotated file.
            thread::sleep(SCAN_POLL);
            files = list_block_files(&self.dir)?;
            let newer = files.iter().any(|(n, _)| *n > num);
            if newer {
                if let Some(pos) = files.iter().position(|(n, _)| *n > num) {
                    file_idx = pos;
                    offset = 0;
                }
            }
        }
    }
}

// --------------------------------------------------------------------- reader

struct Reader {
    dir: PathBuf,
    magic: [u8; 4],
    xor: XorKey,
    index: Arc<BlockFileIndex>,
    files: Mutex<HashMap<u32, Arc<File>>>,
}

impl Reader {
    fn clear_cache(&self) {
        if let Ok(mut map) = self.files.lock() {
            map.clear();
        }
    }

    fn open_file(&self, num: u32) -> Result<Option<Arc<File>>> {
        if let Ok(map) = self.files.lock() {
            if let Some(f) = map.get(&num) {
                return Ok(Some(Arc::clone(f)));
            }
        }
        let path = self.dir.join(file_name(num));
        let file = match File::open(&path) {
            Ok(f) => Arc::new(f),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if let Ok(mut map) = self.files.lock() {
            if map.len() >= 8 {
                map.clear();
            }
            map.insert(num, Arc::clone(&file));
        }
        Ok(Some(file))
    }

    fn fetch(&self, expected: &Hash32) -> Result<Option<Vec<u8>>> {
        let Some(loc) = self.index.get(expected) else {
            return Ok(None);
        };
        let Some(file) = self.open_file(loc.file)? else {
            return Ok(None);
        };
        let mut hdr = [0u8; 8];
        if !read_at(&file, loc.offset, &mut hdr)? {
            return Ok(None);
        }
        self.xor.apply(loc.offset, &mut hdr);
        if hdr[0..4] != self.magic {
            return Ok(None);
        }
        let size = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
        if size != loc.size || !(81..=MAX_BLOCK_SIZE).contains(&size) {
            return Ok(None);
        }
        let mut buf = vec![0u8; size as usize];
        if !read_at(&file, loc.offset + 8, &mut buf)? {
            return Ok(None);
        }
        self.xor.apply(loc.offset + 8, &mut buf);
        let hash = header_hash(&buf[..HEADER_LEN])?;
        if hash != *expected {
            return Ok(None);
        }
        Ok(Some(buf))
    }
}

// ----------------------------------------------------------------- bulk source

/// Probe result for `doctor`.
#[derive(Debug, Clone)]
pub struct Probe {
    pub dir: PathBuf,
    pub files: usize,
    pub bytes: u64,
    pub xor_present: bool,
    pub first_magic_ok: bool,
    pub error: Option<String>,
}

/// Read-only probe of a Core blocks directory.
pub fn probe(dir: &Path, chain: Chain) -> Probe {
    let mut out = Probe {
        dir: dir.to_path_buf(),
        files: 0,
        bytes: 0,
        xor_present: dir.join("xor.dat").exists(),
        first_magic_ok: false,
        error: None,
    };
    match list_block_files(dir) {
        Ok(files) => {
            out.files = files.len();
            for (_, p) in &files {
                if let Ok(m) = std::fs::metadata(p) {
                    out.bytes += m.len();
                }
            }
            if let Some((_, first)) = files.first() {
                match XorKey::load(dir) {
                    Ok(key) => match File::open(first) {
                        Ok(file) => {
                            let mut hdr = [0u8; 8];
                            if let Ok(true) = read_at(&file, 0, &mut hdr) {
                                key.apply(0, &mut hdr);
                                out.first_magic_ok = hdr[0..4] == chain.magic();
                            }
                        }
                        Err(e) => out.error = Some(e.to_string()),
                    },
                    Err(e) => out.error = Some(e.to_string()),
                }
            } else {
                out.error = Some("no blk*.dat files yet".to_string());
            }
        }
        Err(e) => out.error = Some(e.to_string()),
    }
    out
}

/// A running bulk source: locator map + scanner + reader.
pub struct BulkSource {
    dir: PathBuf,
    index: Arc<BlockFileIndex>,
    reader: Reader,
    stop: Arc<AtomicBool>,
    scanner: Mutex<Option<JoinHandle<()>>>,
    errors: Arc<Mutex<Option<String>>>,
    xor_present: bool,
}

impl BulkSource {
    pub fn open(
        blocks_dir: PathBuf,
        cache_dir: &Path,
        chain: Chain,
        genesis: Hash32,
    ) -> Result<Self> {
        if !blocks_dir.is_dir() {
            return Err(BulkError::MissingBlocksDir(blocks_dir));
        }
        let xor = XorKey::load(&blocks_dir)?;
        // Fail fast if the first record belongs to another network. An empty or
        // partially written first record is allowed: the scanner will retry.
        if let Some((_, first)) = list_block_files(&blocks_dir)?.first() {
            if let Ok(file) = File::open(first) {
                let mut hdr = [0u8; 8];
                if read_at(&file, 0, &mut hdr)? {
                    xor.apply(0, &mut hdr);
                    let got = [hdr[0], hdr[1], hdr[2], hdr[3]];
                    let known_other = [
                        Chain::Mainnet,
                        Chain::Testnet4,
                        Chain::Signet,
                        Chain::Regtest,
                    ]
                    .iter()
                    .any(|c| c.magic() == got && *c != chain);
                    if known_other {
                        return Err(BulkError::NotAvailable(format!(
                            "{} does not contain {} blocks (wrong network magic)",
                            blocks_dir.display(),
                            chain.name()
                        )));
                    }
                }
            }
        }
        let sidecar_path = cache_dir.join(SIDECAR_NAME);
        let loaded = load_sidecar(&sidecar_path, chain, &genesis, &xor)?;
        let header = sidecar_header(chain, &genesis, &xor);
        let needs_header = match std::fs::read(&sidecar_path) {
            Ok(buf) => !buf.starts_with(&header),
            Err(_) => true,
        };
        if needs_header {
            let side = Sidecar::create(&sidecar_path, &header)?;
            drop(side);
        }

        // If the locator was loaded, validate its cursor against the actual
        // file. A wiped/reindexed datadir invalidates the whole sidecar.
        let index = Arc::new(BlockFileIndex::new());
        let mut start = loaded.cursor;
        if !loaded.map.is_empty() {
            let mut valid = true;
            if let Some((last_hash, last_loc)) =
                loaded.map.iter().max_by_key(|(_, l)| (l.file, l.offset))
            {
                let path = blocks_dir.join(file_name(last_loc.file));
                match File::open(&path) {
                    Ok(file) => {
                        let mut h = [0u8; HEADER_LEN];
                        if read_at(&file, last_loc.offset + 8, &mut h).unwrap_or(false) {
                            xor.apply(last_loc.offset + 8, &mut h);
                            valid = header_hash(&h).map(|h| h == *last_hash).unwrap_or(false);
                        } else {
                            valid = false;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        // A pruned file is expected with a pruned node. Keep the
                        // entries (some are still valid) and let the scanner
                        // advance to the first file that exists.
                        start = (last_loc.file.saturating_add(1), 0);
                    }
                    Err(_) => valid = false,
                }
            }
            if valid {
                for (h, loc) in &loaded.map {
                    index.insert(*h, *loc);
                }
            } else {
                start = (0, 0);
                let side = Sidecar::create(&sidecar_path, &header)?;
                drop(side);
            }
        }

        let stop = Arc::new(AtomicBool::new(false));
        let errors = Arc::new(Mutex::new(None));
        let scanner = Scanner {
            dir: blocks_dir.clone(),
            magic: chain.magic(),
            xor,
            index: Arc::clone(&index),
            stop: Arc::clone(&stop),
            errors: Arc::clone(&errors),
            sidecar_path,
            start,
        };
        let handle = {
            thread::Builder::new()
                .name("el-blocks-scan".to_string())
                .spawn(move || scanner.run())
                .map_err(|e| BulkError::Io(format!("cannot start block scanner: {e}")))?
        };
        let reader = Reader {
            dir: blocks_dir.clone(),
            magic: chain.magic(),
            xor,
            index: Arc::clone(&index),
            files: Mutex::new(HashMap::new()),
        };
        Ok(BulkSource {
            dir: blocks_dir,
            index,
            reader,
            stop,
            scanner: Mutex::new(Some(handle)),
            errors,
            xor_present: !xor.is_zero(),
        })
    }

    /// Fetch a raw block if the locator knows it. `None` means "fall back to
    /// RPC or retry"; it is never an error to be missing.
    pub fn fetch(&self, hash: &Hash32) -> Result<Option<Vec<u8>>> {
        self.reader.fetch(hash)
    }

    pub fn wait_for(&self, hash: &Hash32, timeout: Duration) -> Option<BlockLocation> {
        self.index.wait_for(hash, timeout)
    }

    pub fn blocks_dir(&self) -> &Path {
        &self.dir
    }

    /// Drop open block-file descriptors. Call after Core prunes so deleted
    /// files do not keep disk space alive through an open handle.
    pub fn clear_cache(&self) {
        self.reader.clear_cache();
    }

    pub fn entries(&self) -> usize {
        self.index.len()
    }

    pub fn xor_present(&self) -> bool {
        self.xor_present
    }

    pub fn last_error(&self) -> Option<String> {
        self.errors.lock().ok().and_then(|e| e.clone())
    }
}

impl Drop for BulkSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut guard) = self.scanner.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::blockdata::constants::genesis_block;
    use bitcoin::Network;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "e-lite-blocks-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn block_bytes() -> Vec<u8> {
        let block = genesis_block(Network::Regtest);
        bitcoin::consensus::serialize(&block)
    }

    fn write_record(file: &mut File, key: &XorKey, offset: u64, block: &[u8]) {
        let mut rec = Vec::with_capacity(8 + block.len());
        rec.extend_from_slice(&Chain::Regtest.magic());
        rec.extend_from_slice(&(block.len() as u32).to_le_bytes());
        rec.extend_from_slice(block);
        key.apply(offset, &mut rec);
        file.write_all_at(&rec, offset).unwrap();
    }

    #[test]
    fn xor_key_alignment_round_trip() {
        let key = XorKey([1, 2, 3, 4, 5, 6, 7, 8]);
        let mut buf = b"hello world".to_vec();
        let orig = buf.clone();
        key.apply(5, &mut buf);
        assert_ne!(buf, orig);
        key.apply(5, &mut buf);
        assert_eq!(buf, orig);
    }

    #[test]
    fn header_hash_matches_rust_bitcoin() {
        let genesis = genesis_block(Network::Regtest);
        let header = bitcoin::consensus::serialize(&genesis.header);
        assert_eq!(
            header_hash(&header).unwrap(),
            Hash32::from_hex(&genesis.block_hash().to_string()).unwrap()
        );
    }

    #[test]
    fn scanner_and_reader_round_trip_with_xor() {
        let dir = tmp("round");
        let key = XorKey([9, 8, 7, 6, 5, 4, 3, 2]);
        std::fs::write(dir.join("xor.dat"), key.0).unwrap();
        let block = block_bytes();
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("blk00000.dat"))
            .unwrap();
        write_record(&mut f, &key, 0, &block);
        // A second record, which makes the first one durable for the scanner.
        write_record(&mut f, &key, (8 + block.len()) as u64, &block);
        drop(f);

        let hash =
            Hash32::from_hex(&genesis_block(Network::Regtest).block_hash().to_string()).unwrap();
        let loaded = load_sidecar(&dir.join(SIDECAR_NAME), Chain::Regtest, &hash, &key).unwrap();
        assert!(loaded.map.is_empty());
        let index = Arc::new(BlockFileIndex::new());
        let stop = Arc::new(AtomicBool::new(false));
        let errors = Arc::new(Mutex::new(None));
        let scanner = Scanner {
            dir: dir.clone(),
            magic: Chain::Regtest.magic(),
            xor: key,
            index: Arc::clone(&index),
            stop: Arc::clone(&stop),
            errors,
            sidecar_path: dir.join(SIDECAR_NAME),
            start: (0, 0),
        };
        // Create the sidecar first, then run the scanner in a thread.
        let header = sidecar_header(Chain::Regtest, &hash, &key);
        Sidecar::create(&dir.join(SIDECAR_NAME), &header).unwrap();
        let handle = thread::spawn(move || scanner.run());
        let loc = index
            .wait_for(&hash, Duration::from_secs(3))
            .expect("scanner found genesis");
        assert_eq!(loc.file, 0);
        let reader = Reader {
            dir: dir.clone(),
            magic: Chain::Regtest.magic(),
            xor: key,
            index,
            files: Mutex::new(HashMap::new()),
        };
        let got = reader.fetch(&hash).unwrap().expect("block bytes");
        assert_eq!(got, block);
        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_reports_magic() {
        let dir = tmp("probe");
        let key = XorKey([0u8; 8]);
        let block = block_bytes();
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("blk00000.dat"))
            .unwrap();
        write_record(&mut f, &key, 0, &block);
        drop(f);
        let p = probe(&dir, Chain::Regtest);
        assert_eq!(p.files, 1);
        assert!(p.first_magic_ok, "{p:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
