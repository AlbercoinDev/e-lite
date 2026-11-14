//! Consistent index snapshots: tar.gz of the database plus a manifest with the
//! network identity, committed tip and canonical digest.
//!
//! Format 2 adds an optional Core section: `export-snapshot --with-core`
//! bundles Bitcoin Core's datadir for the same network under `bitcoin/`
//! (inside the archive, restored as `<data-dir>/bitcoin`). The index part
//! keeps the format-1 layout (data-dir contents at the archive root), so a
//! restored snapshot opens without a schema step and older imports stay valid.
//!
//! Consistency model:
//! - The index is flushed and its committed tip/digest recorded first.
//! - Only Core chain data is copied (`blocks/`, `chainstate/`, `indexes/` of
//!   the exported network): wallets, private keys and configuration never
//!   enter the archive, whatever their name (AUD-126).
//! - The Core datadir is only copied while Core is stopped; the export command
//!   probes Core (short-lived start + RPC + stop) to record tip, prune height
//!   and version in the manifest, then copies the files.
//! - Import validates the index exactly as format 1 did, plus the Core
//!   coherence rules: `index tip <= Core tip` and, when pruned,
//!   `prune_height <= index tip - 288` (the undo window e-lite relies on).

use el_model::codec::SCHEMA_VERSION;
use el_model::{Chain, Hash32};
use el_store::Store;
use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{BufReader, Write};
use std::path::{Component, Path, PathBuf};

type E = Box<dyn std::error::Error + Send + Sync>;

pub const MANIFEST_NAME: &str = "e-lite-snapshot.json";
/// Format 1: index only (no `core` section, no per-file table).
pub const FORMAT: u32 = 1;
/// Format 2: index plus an optional Core section and per-file sizes/hashes.
pub const FORMAT_V2: u32 = 2;
/// Format 3: same content as 2, plus a copy of the manifest as the **first**
/// archive entry (`HEAD_NAME`) so a wrong network/schema is rejected before
/// anything is extracted, and one gzip member per section so the index and
/// Core can use different compression levels (AUD-140/141).
pub const FORMAT_V3: u32 = 3;
/// Early manifest: no per-file table and no Core byte count yet.
pub const HEAD_NAME: &str = "e-lite-snapshot.head.json";

/// gzip writer whose level can change between tar entries. Each level change
/// starts a new gzip member; `MultiGzDecoder` reads them as one stream.
struct LeveledGz {
    enc: Option<GzEncoder<File>>,
}

impl LeveledGz {
    fn new(file: File, level: u32) -> Self {
        LeveledGz {
            enc: Some(GzEncoder::new(file, Compression::new(level.clamp(0, 9)))),
        }
    }

    fn set_level(&mut self, level: u32) -> std::io::Result<()> {
        let file = self.enc.take().expect("encoder present").finish()?;
        self.enc = Some(GzEncoder::new(file, Compression::new(level.clamp(0, 9))));
        Ok(())
    }

    fn finish(mut self) -> std::io::Result<File> {
        self.enc.take().expect("encoder present").finish()
    }
}

impl Write for LeveledGz {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.enc.as_mut().expect("encoder present").write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.enc.as_mut().expect("encoder present").flush()
    }
}

fn append_manifest<W: Write>(
    builder: &mut tar::Builder<W>,
    name: &str,
    manifest: &Manifest,
) -> Result<(), E> {
    let json = serde_json::to_vec_pretty(manifest)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(json.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();
    builder.append_data(&mut header, name, json.as_slice())?;
    Ok(())
}

/// Checks that need only the manifest: format, network and schema.
fn check_manifest_header(manifest: &Manifest, chain: Chain) -> Result<(), E> {
    if !(FORMAT..=FORMAT_V3).contains(&manifest.format) {
        return Err(format!("unsupported snapshot format {}", manifest.format).into());
    }
    if manifest.network != chain.name() {
        return Err(format!(
            "snapshot belongs to {}, config says {}",
            manifest.network,
            chain.name()
        )
        .into());
    }
    if manifest.schema > SCHEMA_VERSION {
        return Err(format!(
            "snapshot schema v{} is newer than supported v{}",
            manifest.schema, SCHEMA_VERSION
        )
        .into());
    }
    Ok(())
}

/// Undo/reorg margin: e-lite keeps Core's prune cut at or below
/// `index - 288`, so a restored pair must preserve at least this window.
pub const UNDO_MARGIN_BLOCKS: u64 = 288;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreSnapshot {
    /// Core's `getnetworkinfo` subversion at export time.
    pub version: String,
    pub tip_height: u64,
    pub tip_hash: String,
    pub pruned: bool,
    /// Core's prune height (lowest still-served block); 0 when not pruned.
    pub prune_height: u64,
    /// Bytes actually packed into the `bitcoin/` section.
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// Archive-relative path (`bitcoin/...` for the Core section).
    pub path: String,
    pub size: u64,
    /// Present only when the export ran with `--hash-files`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub version: String,
    pub network: String,
    pub genesis: String,
    pub height: u32,
    pub hash: String,
    pub schema: u32,
    pub txnum: u64,
    pub digest: String,
    /// Absent in format 1 and in format 2 exports without `--with-core`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core: Option<CoreSnapshot>,
    /// Per-file table for the Core section (sizes always, hashes optional).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Clone)]
pub struct SnapshotInfo {
    pub path: PathBuf,
    pub height: u32,
    pub hash: String,
    pub txnum: u64,
    pub digest: String,
    pub bytes: u64,
    pub core: Option<CoreSnapshot>,
    /// Core datadir entries left out of a `--with-core` export (wallets,
    /// keys, configuration, other networks). Empty on import.
    pub core_skipped: Vec<String>,
}

pub struct ExportOptions {
    /// e-lite version recorded in the manifest.
    pub version: String,
    pub with_core: bool,
    /// gzip level 0..=9 for the index section. Default 0 (stored): the SSTs
    /// are already LZ4-compressed and `txids.dat` is random, so gzip only
    /// costs time there (AUD-140).
    pub index_level: u32,
    /// gzip level 0..=9 for the Core section. Default 3: `blk*.dat` gains
    /// ~10-20 %.
    pub core_level: u32,
    /// SHA-256 every Core file into the manifest (sizes are always recorded).
    /// On by default (AUD-143).
    pub hash_files: bool,
    /// Probed Core facts; `--with-core` requires `Some`.
    pub core: Option<CoreSnapshot>,
    /// Core datadir to pack under `bitcoin/`; required with `with_core`.
    pub core_dir: Option<PathBuf>,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions {
            version: String::new(),
            with_core: false,
            index_level: 0,
            core_level: 3,
            hash_files: true,
            core: None,
            core_dir: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct ImportOptions {
    /// Skip the per-file SHA-256 check of the Core section. Hashes recorded
    /// in the manifest are verified by default (AUD-143).
    pub skip_hashes: bool,
    /// Refuse the snapshot unless the recomputed index digest equals this
    /// value. The manifest digest travels inside the archive, so it only
    /// detects corruption; a digest the operator recorded at export time
    /// also detects a tampered archive (AUD-142).
    pub expect_digest: Option<String>,
}

pub fn export(
    store: &Store,
    chain: Chain,
    output: &Path,
    options: &ExportOptions,
) -> Result<SnapshotInfo, E> {
    store.flush_all()?;
    let tip = store
        .committed_tip()?
        .ok_or("cannot export an empty index; run `e-lite index` first")?;
    let identity = store
        .network_identity()?
        .ok_or("database is not initialized")?;
    let digest = store.canonical_digest()?;
    let mut manifest = Manifest {
        format: FORMAT_V3,
        version: options.version.clone(),
        network: identity.chain.name().to_string(),
        genesis: identity.genesis.to_hex(),
        height: tip.height,
        hash: tip.block_hash.to_hex(),
        schema: tip.schema_version,
        txnum: tip.txnum,
        digest: digest.to_hex(),
        core: None,
        files: Vec::new(),
    };
    if manifest.network != chain.name() {
        return Err(format!(
            "database belongs to {}, config says {}",
            manifest.network,
            chain.name()
        )
        .into());
    }
    if options.with_core && options.core.is_none() {
        return Err("export with Core requested but no Core facts were probed".into());
    }
    if let Some(core) = &options.core {
        validate_core_coherence(&manifest, core).map_err(|m| -> E { m.into() })?;
        manifest.core = options.core.clone();
    }

    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = output.with_extension("tmp");
    let mut core_skipped = Vec::new();
    {
        let file = File::create(&tmp)?;
        let mut builder = tar::Builder::new(LeveledGz::new(file, options.index_level));
        // First entry: everything the importer can check before extracting.
        append_manifest(&mut builder, HEAD_NAME, &manifest)?;
        let data_dir = store.path();
        add_tree(&mut builder, data_dir, data_dir)?;
        let mut core_bytes = 0u64;
        if let Some(core_dir) = options.core_dir.as_deref() {
            if options.core_level != options.index_level {
                builder.get_mut().set_level(options.core_level)?;
            }
            let walk = add_core_datadir(&mut builder, core_dir, chain, options.hash_files)?;
            core_bytes = walk.bytes;
            manifest.files = walk.files;
            core_skipped = walk.skipped;
        }
        if let Some(core) = manifest.core.as_mut() {
            core.bytes = core_bytes;
        }
        append_manifest(&mut builder, MANIFEST_NAME, &manifest)?;
        builder.into_inner()?.finish()?.sync_all()?;
    }
    fs::rename(&tmp, output)?;
    let bytes = fs::metadata(output)?.len();
    Ok(SnapshotInfo {
        path: output.to_path_buf(),
        height: tip.height,
        hash: tip.block_hash.to_hex(),
        txnum: tip.txnum,
        digest: digest.to_hex(),
        bytes,
        core: manifest.core.clone(),
        core_skipped,
    })
}

/// Coherence rules between the index tip recorded in the manifest and the
/// probed Core state. Shared by export and import.
pub fn validate_core_coherence(manifest: &Manifest, core: &CoreSnapshot) -> Result<(), String> {
    if (manifest.height as u64) > core.tip_height {
        return Err(format!(
            "index tip {} is ahead of Core tip {}; the pair is inconsistent",
            manifest.height, core.tip_height
        ));
    }
    if core.pruned
        && core.prune_height > 0
        && core.prune_height + UNDO_MARGIN_BLOCKS > manifest.height as u64
    {
        // Cut deeper than index - 288 breaks the reorg window e-lite assumes.
        return Err(format!(
            "Core prune height {} leaves less than the {}-block undo window below the index tip {}",
            core.prune_height, UNDO_MARGIN_BLOCKS, manifest.height
        ));
    }
    Ok(())
}

/// Files that are never part of an index snapshot: rebuildable caches,
/// runtime markers, Tor/TLS secrets and logs.
fn excluded_from_index(name: &str) -> bool {
    matches!(
        name,
        "rawtx-cache"
            | MANIFEST_NAME
            | HEAD_NAME
            | "tor"
            | "tls"
            | "LOG"
            | "LOCK"
            | "e-lite.log"
            | "e-lite.log.1"
            | "banner.txt"
            | "core-paused-by-e-lite"
            | "core-child.pid"
            | "bitcoind.log"
            | "bitcoin"
            // The decoy-wallet secret (AUD-163): whoever holds a snapshot
            // must not be able to recompute this node's decoys.
            | el_rawtx::decoy_wallets::DECOY_KEY_FILE
            | ".decoy.key.tmp"
    )
}

/// Chain-data directories of a Core datadir. Only these are copied, at the
/// datadir root (mainnet) or inside the network subdirectory (`testnet4/`,
/// `signet/`, `regtest/`). Everything else at those two levels — legacy
/// `wallet.dat` files with arbitrary names, `onion_v3_private_key`,
/// `i2p_private_key`, `bitcoin.conf` (may hold `rpcpassword`),
/// `settings.json`, other networks' subdirectories — is skipped (AUD-126).
const CORE_DATA_DIRS: [&str; 3] = ["blocks", "chainstate", "indexes"];

/// Where `add_core_tree` is inside the Core datadir.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CoreLevel {
    /// The datadir root.
    Root,
    /// `<root>/<network subdir>` for every network except mainnet.
    Network,
    /// Inside `blocks/`, `chainstate/` or `indexes/`.
    Data,
}

/// Allowlist decision for one entry. Returns the level to descend into for a
/// directory, `Some(Data)` for a file inside chain data, or `None` to skip.
fn core_entry_level(level: CoreLevel, name: &str, is_dir: bool, subdir: &str) -> Option<CoreLevel> {
    match level {
        CoreLevel::Root if !subdir.is_empty() => {
            (is_dir && name == subdir).then_some(CoreLevel::Network)
        }
        CoreLevel::Root | CoreLevel::Network => {
            (is_dir && CORE_DATA_DIRS.contains(&name)).then_some(CoreLevel::Data)
        }
        CoreLevel::Data => (!excluded_from_core(name)).then_some(CoreLevel::Data),
    }
}

/// Names never copied even inside chain data: wallets and runtime state that
/// a chain-only restore must not carry (stale auth, peer state, logs). The
/// allowlist above already keeps these out of the root; this is the second
/// line of defence for unusual layouts.
fn excluded_from_core(name: &str) -> bool {
    matches!(
        name,
        "wallets"
            | "wallet.dat"
            | "database"
            | "db.log"
            | ".walletlock"
            | "onion_v3_private_key"
            | "onion_private_key"
            | "i2p_private_key"
            | "bitcoin.conf"
            | "settings.json"
            | "mempool.dat"
            | "peers.dat"
            | "banlist.dat"
            | ".cookie"
            | "anchors.dat"
            | "debug.log"
            | ".lock"
            | "gauges"
            | "bitcoin.conf.bak"
    )
}

fn sha256_file(path: &Path) -> Result<String, E> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    std::io::copy(&mut reader, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

/// Index section of the archive: data-dir contents at the root, minus the
/// rebuildable caches, secrets, logs and the nested Core datadir.
fn add_tree<W: Write>(builder: &mut tar::Builder<W>, base: &Path, dir: &Path) -> Result<(), E> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // The rawtx cache is rebuildable and can be large. Logs, runtime
        // markers, Tor keys, TLS private keys and the nested Core datadir are
        // not part of the index.
        if excluded_from_index(&name) {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(base)?.to_path_buf();
        if entry.file_type()?.is_dir() {
            builder.append_dir(&rel, &path)?;
            add_tree(builder, base, &path)?;
        } else {
            builder.append_path_with_name(&path, &rel)?;
        }
    }
    Ok(())
}

/// What the Core walk packed and what it deliberately left out.
#[derive(Debug, Default)]
struct CoreWalk {
    files: Vec<FileEntry>,
    /// Archive-relative names skipped by the allowlist (root/network level)
    /// or the denylist, reported so the operator knows what to keep apart.
    skipped: Vec<String>,
    bytes: u64,
}

/// Walk the Core datadir of `chain`, append its chain data under `bitcoin/`
/// and record the per-file table (sizes always, SHA-256 when `hash_files`).
fn add_core_datadir<W: Write>(
    builder: &mut tar::Builder<W>,
    dir: &Path,
    chain: Chain,
    hash_files: bool,
) -> Result<CoreWalk, E> {
    let mut walk = CoreWalk::default();
    add_core_tree(
        builder,
        dir,
        "bitcoin",
        CoreLevel::Root,
        chain.core_subdir(),
        hash_files,
        &mut walk,
    )?;
    Ok(walk)
}

/// `prefix` is the archive path of `dir` (`bitcoin`, then `bitcoin/regtest`,
/// ...); `level` drives the allowlist.
fn add_core_tree<W: Write>(
    builder: &mut tar::Builder<W>,
    dir: &Path,
    prefix: &str,
    level: CoreLevel,
    subdir: &str,
    hash_files: bool,
    walk: &mut CoreWalk,
) -> Result<(), E> {
    let mut entries = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let path = entry.path();
        let tar_name = format!("{prefix}/{name}");
        let is_dir = entry.file_type()?.is_dir();
        let Some(next) = core_entry_level(level, &name, is_dir, subdir) else {
            walk.skipped.push(tar_name);
            continue;
        };
        if is_dir {
            builder.append_dir(&tar_name, &path)?;
            add_core_tree(builder, &path, &tar_name, next, subdir, hash_files, walk)?;
        } else {
            let size = entry.metadata()?.len();
            let sha256 = if hash_files {
                Some(sha256_file(&path)?)
            } else {
                None
            };
            walk.files.push(FileEntry {
                path: tar_name.clone(),
                size,
                sha256,
            });
            walk.bytes += size;
            builder.append_path_with_name(&path, &tar_name)?;
        }
    }
    Ok(())
}

/// Verify the extracted Core files against the manifest: sizes are always
/// compared; SHA-256 only for entries that carry one and when requested.
pub fn verify_core_files(target: &Path, manifest: &Manifest, verify_hashes: bool) -> Result<(), E> {
    let core_dir = target.join("bitcoin");
    for entry in &manifest.files {
        let path = target.join(&entry.path);
        let Ok(md) = fs::metadata(&path) else {
            return Err(format!("snapshot file {} is missing", entry.path).into());
        };
        if md.len() != entry.size {
            return Err(format!(
                "snapshot file {} is {} bytes, manifest says {}",
                entry.path,
                md.len(),
                entry.size
            )
            .into());
        }
        if verify_hashes {
            let Some(expected) = &entry.sha256 else {
                continue;
            };
            let actual = sha256_file(&path)?;
            if &actual != expected {
                return Err(format!(
                    "snapshot file {} hash mismatch: {} != {}",
                    entry.path, actual, expected
                )
                .into());
            }
        }
    }
    if !manifest.files.is_empty() && !core_dir.is_dir() {
        return Err("manifest lists Core files but bitcoin/ is missing".into());
    }
    Ok(())
}

/// Extraction re-checks free space after every this many bytes.
const FREE_SPACE_CHECK_BYTES: u64 = 256 * 1024 * 1024;
/// Never extract below this much free space on the target filesystem.
const IMPORT_FREE_FLOOR: u64 = 1024 * 1024 * 1024;

fn check_free_space(dir: &Path, needed: u64, written: u64) -> Result<(), E> {
    let Some(free) = el_chain::disk_free_bytes(dir) else {
        return Ok(()); // unknown filesystem: do not block the restore
    };
    if free < needed {
        let gib = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
        return Err(format!(
            "not enough free space for the restore: {:.1} GiB free, need at least {:.1} GiB \
             more ({:.1} GiB extracted so far); the partial restore is removed",
            gib(free),
            gib(needed),
            gib(written)
        )
        .into());
    }
    Ok(())
}

/// Staging directory next to `target` (same filesystem, so the final rename
/// is atomic): `<parent>/.<name>.import-partial`.
pub fn staging_dir(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "data".to_string());
    target.with_file_name(format!(".{name}.import-partial"))
}

/// Extract a snapshot and verify it, then move it into `target` (must be new
/// or empty). Extraction and every check run in a staging directory: an
/// interrupted or rejected import never leaves a half-restored `target`, and
/// a retry does not trip over "target not empty" (AUD-141).
pub fn import(
    input: &Path,
    target: &Path,
    chain: Chain,
    options: &ImportOptions,
) -> Result<SnapshotInfo, E> {
    if target.exists() {
        let non_empty = fs::read_dir(target)?.next().is_some();
        if non_empty {
            return Err(format!(
                "target {} is not empty; choose a new --data-dir",
                target.display()
            )
            .into());
        }
    }
    let expect_digest = options
        .expect_digest
        .as_deref()
        .map(|hex| Hash32::from_hex(hex.trim()).map_err(|e| format!("--expect-digest: {e}")))
        .transpose()?;
    let staging = staging_dir(target);
    if staging.exists() {
        // Only e-lite creates this name: it is the leftover of an import
        // that was interrupted (crash, power loss) before cleanup.
        eprintln!(
            "[e-lite] removing the leftover of an interrupted import: {}",
            staging.display()
        );
        fs::remove_dir_all(&staging)?;
    }
    if let Some(parent) = staging.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir(&staging)?;

    let result = import_into(input, &staging, chain, options, expect_digest);
    let mut info = match result {
        Ok(info) => info,
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(e);
        }
    };
    if target.exists() {
        fs::remove_dir(target)?; // empty, checked above
    }
    if let Err(e) = fs::rename(&staging, target) {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!(
            "cannot move the restored data into {}: {e}",
            target.display()
        )
        .into());
    }
    info.path = target.to_path_buf();
    Ok(info)
}

/// Extract and verify into `target` (the staging directory).
fn import_into(
    input: &Path,
    target: &Path,
    chain: Chain,
    options: &ImportOptions,
    expect_digest: Option<Hash32>,
) -> Result<SnapshotInfo, E> {
    // The uncompressed size is only known at the end of the archive, so the
    // free space is checked before (at least the archive size) and while
    // extracting: a restore must fail cleanly, not fill the disk (AUD-141).
    let archive_bytes = fs::metadata(input)?.len();
    check_free_space(target, archive_bytes, 0)?;
    let mut written = 0u64;
    let mut next_check = FREE_SPACE_CHECK_BYTES;

    let file = File::open(input)?;
    let mut archive = tar::Archive::new(MultiGzDecoder::new(file));
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()?.as_os_str() == HEAD_NAME {
            // Format 3: reject a wrong network/format/schema before the bulk
            // of the archive is extracted. The entry itself is not unpacked.
            let mut json = Vec::new();
            std::io::Read::read_to_end(&mut std::io::Read::take(&mut entry, 1 << 20), &mut json)?;
            let head: Manifest = serde_json::from_slice(&json)?;
            check_manifest_header(&head, chain)?;
            continue;
        }
        written = written.saturating_add(entry.header().size().unwrap_or(0));
        if written >= next_check {
            check_free_space(target, IMPORT_FREE_FLOOR, written)?;
            next_check = written.saturating_add(FREE_SPACE_CHECK_BYTES);
        }
        let kind = entry.header().entry_type();
        // Symlinks/hardlinks and device nodes would let a malicious archive
        // escape the target directory or create special files. Regular files,
        // directories and GNU sparse regular files (Core preallocates
        // `blk*.dat` with holes) are the only accepted entry types; the
        // manifest's size/hash table covers the unpacked content.
        if !kind.is_file() && !kind.is_dir() && !matches!(kind, tar::EntryType::GNUSparse) {
            return Err(format!("unsafe snapshot entry type: {kind:?}").into());
        }
        let path = entry.path()?.to_path_buf();
        if path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::RootDir))
        {
            return Err(format!("unsafe path in snapshot: {}", path.display()).into());
        }
        entry.unpack_in(target)?;
    }

    let manifest_path = target.join(MANIFEST_NAME);
    let manifest: Manifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    check_manifest_header(&manifest, chain)?;

    // Format 2: verify the Core section before trusting the pair. Sizes are
    // checked always; hashes only when the export recorded them and the
    // operator asked for verification.
    if manifest.format >= FORMAT_V2 {
        verify_core_files(target, &manifest, !options.skip_hashes)?;
    }

    let store = Store::open(target)?;
    let identity = store
        .network_identity()?
        .ok_or("snapshot has no network identity")?;
    if identity.genesis.to_hex() != manifest.genesis {
        return Err("snapshot genesis does not match its manifest".into());
    }
    if identity.chain != chain {
        return Err("snapshot network does not match the configured network".into());
    }
    let tip = store
        .committed_tip()?
        .ok_or("snapshot has no committed tip")?;
    if tip.height != manifest.height || tip.block_hash.to_hex() != manifest.hash {
        return Err("snapshot tip does not match its manifest".into());
    }
    let digest = store.canonical_digest()?;
    let expected = Hash32::from_hex(&manifest.digest)?;
    if digest != expected {
        return Err(
            format!("snapshot digest mismatch: recomputed {digest}, manifest {expected}").into(),
        );
    }
    if let Some(wanted) = expect_digest {
        if digest != wanted {
            return Err(format!(
                "snapshot digest {digest} is not the expected {wanted}; the archive is not the \
                 one you exported (refusing)"
            )
            .into());
        }
    }
    if let Some(core) = &manifest.core {
        validate_core_coherence(&manifest, core).map_err(|m| -> E { m.into() })?;
    }
    let bytes = fs::metadata(input)?.len();
    Ok(SnapshotInfo {
        path: target.to_path_buf(),
        height: tip.height,
        hash: tip.block_hash.to_hex(),
        txnum: tip.txnum,
        digest: digest.to_hex(),
        bytes,
        core: manifest.core.clone(),
        core_skipped: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn sample_manifest() -> Manifest {
        Manifest {
            format: FORMAT_V2,
            version: "0.1.0-test".to_string(),
            network: Chain::Regtest.name().to_string(),
            genesis: "0f9188f13cb7b2c71f2a335e3a4fc328bf5beb4360cfaf49b1de24b1bd3a4b19".to_string(),
            height: 900,
            hash: "aa".repeat(32),
            schema: SCHEMA_VERSION,
            txnum: 10,
            digest: "bb".repeat(32),
            core: Some(CoreSnapshot {
                version: "/Satoshi:30.0.0/".to_string(),
                tip_height: 910,
                tip_hash: "cc".repeat(32),
                pruned: true,
                prune_height: 612,
                bytes: 1234,
            }),
            files: vec![FileEntry {
                path: "bitcoin/chainstate/MANIFEST-000001".to_string(),
                size: 16,
                sha256: None,
            }],
        }
    }

    #[test]
    fn manifest_v2_round_trips_and_v1_has_no_core() {
        let m = sample_manifest();
        let json = serde_json::to_vec(&m).unwrap();
        let back: Manifest = serde_json::from_slice(&json).unwrap();
        assert_eq!(back.format, FORMAT_V2);
        assert!(back.core.is_some());
        assert_eq!(back.files.len(), 1);
        // Format 1 stays valid: no core section, no files table.
        let v1 = r#"{"format":1,"version":"x","network":"regtest","genesis":"00",
            "height":1,"hash":"00","schema":1,"txnum":1,"digest":"00"}"#;
        let m1: Manifest = serde_json::from_str(v1).unwrap();
        assert_eq!(m1.format, FORMAT);
        assert!(m1.core.is_none());
        assert!(m1.files.is_empty());
    }

    #[test]
    fn core_coherence_rules_are_enforced() {
        let m = sample_manifest(); // index tip 900, core tip 910, prune 612
        assert!(validate_core_coherence(&m, m.core.as_ref().unwrap()).is_ok());
        // Index ahead of Core: inconsistent pair.
        let mut m2 = m.clone();
        m2.height = 950;
        assert!(validate_core_coherence(&m2, m2.core.as_ref().unwrap())
            .unwrap_err()
            .contains("ahead of Core tip"));
        // Prune cut deeper than index - 288: the undo window is gone.
        let mut m3 = m.clone();
        m3.height = 1000;
        let mut core = m3.core.clone().unwrap();
        core.tip_height = 1100; // Core is ahead of the index here
        core.prune_height = 790; // 1000 - 288 = 712 < 790
        assert!(validate_core_coherence(&m3, &core)
            .unwrap_err()
            .contains("undo window"));
        // Exactly at the margin is fine.
        core.prune_height = 712;
        assert!(validate_core_coherence(&m3, &core).is_ok());
        // A non-pruned Core never violates the margin.
        core.pruned = false;
        core.prune_height = 0;
        assert!(validate_core_coherence(&m3, &core).is_ok());
    }

    #[test]
    fn index_walk_excludes_the_nested_core_datadir() {
        assert!(excluded_from_index("bitcoin"));
        assert!(excluded_from_index("rawtx-cache"));
        assert!(excluded_from_index("tor"));
        assert!(excluded_from_index("tls"));
        assert!(excluded_from_index("core-child.pid"));
        assert!(excluded_from_index("decoy.key"));
        assert!(!excluded_from_index("chainstate")); // index CF dirs stay
        assert!(!excluded_from_index("txids.dat"));
    }

    #[test]
    fn core_walk_skips_runtime_state_and_wallets() {
        for name in [
            "wallets",
            "mempool.dat",
            "peers.dat",
            "banlist.dat",
            ".cookie",
            "anchors.dat",
            "debug.log",
            ".lock",
            "wallet.dat",
            "onion_v3_private_key",
            "i2p_private_key",
            "bitcoin.conf",
            "settings.json",
        ] {
            assert!(excluded_from_core(name), "{name} must be excluded");
        }
        // Chain data is copied.
        for name in ["blocks", "chainstate", "indexes", "blk00000.dat", "xor.dat"] {
            assert!(!excluded_from_core(name), "{name} must be copied");
        }
    }

    #[test]
    fn core_allowlist_only_descends_into_chain_data() {
        use CoreLevel::*;
        // Mainnet: chain data lives at the root; other networks are skipped.
        assert_eq!(core_entry_level(Root, "blocks", true, ""), Some(Data));
        assert_eq!(core_entry_level(Root, "chainstate", true, ""), Some(Data));
        assert_eq!(core_entry_level(Root, "indexes", true, ""), Some(Data));
        assert_eq!(core_entry_level(Root, "testnet4", true, ""), None);
        assert_eq!(core_entry_level(Root, "blocks", false, ""), None);
        // A legacy wallet may have any name; at the root it is never copied.
        assert_eq!(core_entry_level(Root, "my-savings.dat", false, ""), None);
        // Other networks: only their subdirectory, and mainnet data is skipped.
        assert_eq!(
            core_entry_level(Root, "regtest", true, "regtest"),
            Some(Network)
        );
        assert_eq!(core_entry_level(Root, "blocks", true, "regtest"), None);
        assert_eq!(
            core_entry_level(Network, "blocks", true, "regtest"),
            Some(Data)
        );
        assert_eq!(core_entry_level(Network, "wallets", true, "regtest"), None);
        // Inside chain data the denylist still applies.
        assert_eq!(
            core_entry_level(Data, "blk00000.dat", false, ""),
            Some(Data)
        );
        assert_eq!(core_entry_level(Data, "wallet.dat", false, ""), None);
    }

    #[test]
    fn mainnet_core_walk_leaves_wallets_keys_and_config_out() {
        let dir = std::env::temp_dir().join(format!(
            "e-lite-coremain-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let core = dir.join("bitcoin");
        std::fs::create_dir_all(core.join("blocks/index")).unwrap();
        std::fs::create_dir_all(core.join("chainstate")).unwrap();
        std::fs::create_dir_all(core.join("testnet4/blocks")).unwrap();
        std::fs::write(core.join("blocks/blk00000.dat"), b"block").unwrap();
        std::fs::write(core.join("blocks/index/000001.ldb"), b"idx").unwrap();
        std::fs::write(core.join("chainstate/000002.ldb"), b"utxo").unwrap();
        std::fs::write(core.join("testnet4/blocks/blk00000.dat"), b"other").unwrap();
        for secret in [
            "wallet.dat",
            "savings.dat",
            "onion_v3_private_key",
            "i2p_private_key",
            "bitcoin.conf",
            "settings.json",
        ] {
            std::fs::write(core.join(secret), b"secret").unwrap();
        }

        let archive_path = dir.join("core.tar.gz");
        let walk = {
            let file = File::create(&archive_path).unwrap();
            let enc = GzEncoder::new(file, Compression::default());
            let mut tar = tar::Builder::new(enc);
            let walk = add_core_datadir(&mut tar, &core, Chain::Mainnet, false).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
            walk
        };
        assert_eq!(walk.bytes, 5 + 3 + 4);
        let mut packed: Vec<_> = walk.files.iter().map(|f| f.path.as_str()).collect();
        packed.sort();
        assert_eq!(
            packed,
            vec![
                "bitcoin/blocks/blk00000.dat",
                "bitcoin/blocks/index/000001.ldb",
                "bitcoin/chainstate/000002.ldb",
            ]
        );
        for skipped in [
            "bitcoin/wallet.dat",
            "bitcoin/savings.dat",
            "bitcoin/onion_v3_private_key",
            "bitcoin/i2p_private_key",
            "bitcoin/bitcoin.conf",
            "bitcoin/settings.json",
            "bitcoin/testnet4",
        ] {
            assert!(
                walk.skipped.iter().any(|s| s == skipped),
                "{skipped} must be reported as skipped"
            );
        }
        // The archive itself must not contain a single secret byte.
        let file = File::open(&archive_path).unwrap();
        for entry in tar::Archive::new(MultiGzDecoder::new(file))
            .entries()
            .unwrap()
        {
            let mut entry = entry.unwrap();
            let mut body = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut body).unwrap();
            assert_ne!(body, b"secret", "{}", entry.path().unwrap().display());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sha256_is_stable_and_hex() {
        let dir = std::env::temp_dir().join(format!("e-lite-sha-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f");
        std::fs::write(&path, b"e-lite").unwrap();
        let h = sha256_file(&path).unwrap();
        // 32 bytes as lowercase hex.
        assert_eq!(h.len(), 64);
        assert!(h
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // Same content, same hash; different content, different hash.
        assert_eq!(sha256_file(&path).unwrap(), h);
        std::fs::write(&path, b"other").unwrap();
        assert_ne!(sha256_file(&path).unwrap(), h);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compression_level_is_applied_and_the_export_clamps() {
        assert_eq!(Compression::new(3).level(), 3);
        assert_eq!(Compression::new(0).level(), 0);
        assert_eq!(Compression::new(9).level(), 9);
        // flate2 does not clamp itself; the export clamps before constructing.
        assert_eq!(Compression::new(12u32.clamp(0, 9)).level(), 9);
    }

    #[test]
    fn core_tree_keeps_nested_paths_and_skips_runtime_state() {
        let dir = std::env::temp_dir().join(format!(
            "e-lite-coretree-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let core = dir.join("bitcoin");
        std::fs::create_dir_all(core.join("regtest/chainstate")).unwrap();
        std::fs::create_dir_all(core.join("regtest/blocks")).unwrap();
        std::fs::create_dir_all(core.join("wallets")).unwrap();
        std::fs::write(core.join("regtest/chainstate/MANIFEST-000001"), b"abc").unwrap();
        std::fs::write(core.join("regtest/blocks/blk00000.dat"), b"block").unwrap();
        std::fs::write(core.join("bitcoin.conf"), b"prune=1\n").unwrap();
        std::fs::write(core.join("regtest/.cookie"), b"user:pass").unwrap();
        std::fs::write(core.join("wallets/wallet.dat"), b"secret").unwrap();
        std::fs::create_dir_all(core.join("regtest/wallets/w")).unwrap();
        std::fs::write(core.join("regtest/wallets/w/wallet.dat"), b"secret").unwrap();

        let archive_path = dir.join("core.tar.gz");
        {
            let file = File::create(&archive_path).unwrap();
            let enc = GzEncoder::new(file, Compression::default());
            let mut tar = tar::Builder::new(enc);
            let walk = add_core_datadir(&mut tar, &core, Chain::Regtest, true).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
            assert_eq!(
                walk.bytes,
                3 + 5,
                "wallets, .cookie and bitcoin.conf must not count"
            );
            assert!(walk.files.iter().all(|f| f.sha256.is_some()));
        }
        let file = File::open(&archive_path).unwrap();
        let mut names = Vec::new();
        for entry in tar::Archive::new(MultiGzDecoder::new(file))
            .entries()
            .unwrap()
        {
            let entry = entry.unwrap();
            if entry.header().entry_type().is_file() {
                names.push(entry.path().unwrap().display().to_string());
            }
        }
        names.sort();
        assert_eq!(
            names,
            vec![
                "bitcoin/regtest/blocks/blk00000.dat",
                "bitcoin/regtest/chainstate/MANIFEST-000001",
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_rejects_symlink_entries() {
        let dir = std::env::temp_dir().join(format!(
            "e-lite-snapshot-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let archive_path = dir.join("evil.tar.gz");
        {
            let file = File::create(&archive_path).unwrap();
            let enc = GzEncoder::new(file, Compression::default());
            let mut tar = tar::Builder::new(enc);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_mode(0o777);
            header.set_mtime(0);
            header.set_link_name("/tmp").unwrap();
            header.set_cksum();
            tar.append_data(&mut header, "evil", std::io::empty())
                .unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let target = dir.join("target");
        let err = import(
            &archive_path,
            &target,
            Chain::Regtest,
            &ImportOptions::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("unsafe snapshot entry type"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_rejects_a_snapshot_from_another_network() {
        let dir = std::env::temp_dir().join(format!(
            "e-lite-snapshot-net-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut manifest = sample_manifest();
        manifest.network = Chain::Mainnet.name().to_string();
        let archive_path = dir.join("other-network.tar.gz");
        {
            let file = File::create(&archive_path).unwrap();
            let enc = GzEncoder::new(file, Compression::default());
            let mut tar = tar::Builder::new(enc);
            let json = serde_json::to_vec(&manifest).unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_size(json.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_cksum();
            tar.append_data(&mut header, MANIFEST_NAME, json.as_slice())
                .unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let target = dir.join("target");
        let err = import(
            &archive_path,
            &target,
            Chain::Regtest,
            &ImportOptions::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("snapshot belongs to mainnet"));
        // AUD-141: a rejected import leaves no partial target behind.
        assert!(!target.exists(), "target must not be created");
        assert!(!staging_dir(&target).exists(), "staging must be cleaned");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_rejects_an_unknown_format() {
        let dir = std::env::temp_dir().join(format!(
            "e-lite-snapshot-fmt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut manifest = sample_manifest();
        manifest.format = 99;
        let archive_path = dir.join("future.tar.gz");
        {
            let file = File::create(&archive_path).unwrap();
            let enc = GzEncoder::new(file, Compression::default());
            let mut tar = tar::Builder::new(enc);
            let json = serde_json::to_vec(&manifest).unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_size(json.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_cksum();
            tar.append_data(&mut header, MANIFEST_NAME, json.as_slice())
                .unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let target = dir.join("target");
        let err = import(
            &archive_path,
            &target,
            Chain::Regtest,
            &ImportOptions::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("unsupported snapshot format 99"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn exported_regtest_snapshot(dir: &Path) -> (PathBuf, String) {
        let data = dir.join("source");
        let store = Store::open(&data).unwrap();
        store
            .init_identity(&el_model::NetworkIdentity::new(
                Chain::Regtest,
                Chain::Regtest.known_genesis().unwrap(),
            ))
            .unwrap();
        let tip = el_model::IndexedTip {
            height: 5,
            block_hash: Hash32([0x5a; 32]),
            schema_version: SCHEMA_VERSION,
            txnum: 0,
        };
        store.commit_tip_only(Store::batch(), &tip).unwrap();
        let archive = dir.join("snap.tar.gz");
        let info = export(&store, Chain::Regtest, &archive, &ExportOptions::default()).unwrap();
        (archive, info.digest)
    }

    #[test]
    fn import_is_atomic_and_checks_the_expected_digest() {
        let dir = std::env::temp_dir().join(format!(
            "e-lite-snapshot-atomic-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let (archive, digest) = exported_regtest_snapshot(&dir);
        let target = dir.join("restored");

        // AUD-142: a digest the operator did not record is refused, and the
        // refusal leaves nothing behind (AUD-141).
        let wrong = ImportOptions {
            expect_digest: Some("00".repeat(32)),
            ..Default::default()
        };
        let err = import(&archive, &target, Chain::Regtest, &wrong).unwrap_err();
        assert!(err.to_string().contains("not the expected"), "{err}");
        assert!(!target.exists());
        assert!(!staging_dir(&target).exists());
        let bad_hex = ImportOptions {
            expect_digest: Some("xyz".into()),
            ..Default::default()
        };
        assert!(import(&archive, &target, Chain::Regtest, &bad_hex)
            .unwrap_err()
            .to_string()
            .contains("--expect-digest"));

        // The leftover of an interrupted import is replaced, and an empty
        // existing target is accepted.
        std::fs::create_dir_all(staging_dir(&target).join("junk")).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        let right = ImportOptions {
            expect_digest: Some(digest.to_uppercase()),
            ..Default::default()
        };
        let info = import(&archive, &target, Chain::Regtest, &right).unwrap();
        assert_eq!(info.digest, digest);
        assert_eq!(info.path, target);
        assert!(target.join(MANIFEST_NAME).is_file());
        assert!(!target.join("junk").exists());
        assert!(!staging_dir(&target).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_refuses_to_fill_the_disk() {
        let dir = std::env::temp_dir();
        assert!(check_free_space(&dir, 0, 0).is_ok());
        let err = check_free_space(&dir, u64::MAX, 5 << 30)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not enough free space"), "{err}");
        assert!(err.contains("5.0 GiB extracted"), "{err}");
    }

    #[test]
    fn format_3_puts_the_manifest_first_and_rejects_early() {
        // AUD-140/141: head manifest first, one gzip member per level, and a
        // wrong network is refused from the head.
        let dir = std::env::temp_dir().join(format!(
            "e-lite-snapshot-v3-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let (archive, digest) = exported_regtest_snapshot(&dir);
        let file = File::open(&archive).unwrap();
        let mut names = Vec::new();
        for entry in tar::Archive::new(MultiGzDecoder::new(file))
            .entries()
            .unwrap()
        {
            names.push(entry.unwrap().path().unwrap().display().to_string());
        }
        assert_eq!(names.first().map(String::as_str), Some(HEAD_NAME));
        assert_eq!(names.last().map(String::as_str), Some(MANIFEST_NAME));

        // The head alone is enough to refuse another network: an archive
        // that is cut right after it still gives the precise error.
        let err = import(
            &archive,
            &dir.join("other"),
            Chain::Signet,
            &ImportOptions::default(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("snapshot belongs to regtest"),
            "{err}"
        );

        // Round trip; the head is not left in the restored data dir and a
        // re-export does not carry the old manifests as index files.
        let target = dir.join("restored");
        let info = import(&archive, &target, Chain::Regtest, &ImportOptions::default()).unwrap();
        assert_eq!(info.digest, digest);
        assert!(!target.join(HEAD_NAME).exists());
        assert!(excluded_from_index(MANIFEST_NAME) && excluded_from_index(HEAD_NAME));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gzip_level_can_change_between_sections() {
        let dir = std::env::temp_dir().join(format!(
            "e-lite-leveled-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("two-members.gz");
        let zeros = vec![0u8; 200_000];
        {
            let mut gz = LeveledGz::new(File::create(&path).unwrap(), 0);
            gz.write_all(&zeros).unwrap();
            gz.set_level(6).unwrap();
            gz.write_all(&zeros).unwrap();
            gz.finish().unwrap();
        }
        // Stored member (~200 kB) + compressed member (tiny).
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size > 200_000 && size < 210_000, "{size}");
        let mut out = Vec::new();
        std::io::Read::read_to_end(
            &mut MultiGzDecoder::new(File::open(&path).unwrap()),
            &mut out,
        )
        .unwrap();
        assert_eq!(out.len(), 400_000);
        let defaults = ExportOptions::default();
        assert_eq!((defaults.index_level, defaults.core_level), (0, 3));
        assert!(defaults.hash_files, "hashes are recorded by default");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
