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
//! - The Core datadir is only copied while Core is stopped; the export command
//!   probes Core (short-lived start + RPC + stop) to record tip, prune height
//!   and version in the manifest, then copies the files.
//! - Import validates the index exactly as format 1 did, plus the Core
//!   coherence rules: `index tip <= Core tip` and, when pruned,
//!   `prune_height <= index tip - 288` (the undo window e-lite relies on).

use el_model::codec::SCHEMA_VERSION;
use el_model::{Chain, Hash32};
use el_store::Store;
use flate2::read::GzDecoder;
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
}

pub struct ExportOptions {
    /// e-lite version recorded in the manifest.
    pub version: String,
    pub with_core: bool,
    /// gzip level 0..=9; 3 is the default (balanced), 0 stores without
    /// compressing (useful when the Core section dominates the archive).
    pub compress_level: u32,
    /// SHA-256 every Core file into the manifest (sizes are always recorded).
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
            compress_level: 3,
            hash_files: false,
            core: None,
            core_dir: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct ImportOptions {
    /// Verify manifest SHA-256 hashes against the extracted files.
    pub verify_hashes: bool,
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
        format: FORMAT_V2,
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
    {
        let file = File::create(&tmp)?;
        // The default level 3 is the balanced point for the index (and gains
        // ~10-20 % on Core's blk*.dat); 0 stores without compressing.
        let encoder = GzEncoder::new(file, Compression::new(options.compress_level.clamp(0, 9)));
        let mut builder = tar::Builder::new(encoder);
        let data_dir = store.path();
        add_tree(&mut builder, data_dir, data_dir)?;
        let mut core_bytes = 0u64;
        if let Some(core_dir) = options.core_dir.as_deref() {
            core_bytes = add_core_tree(
                &mut builder,
                core_dir,
                "bitcoin",
                &mut manifest.files,
                options.hash_files,
            )?;
        }
        if let Some(core) = manifest.core.as_mut() {
            core.bytes = core_bytes;
        }
        let json = serde_json::to_vec_pretty(&manifest)?;
        let mut header = tar::Header::new_gnu();
        header.set_size(json.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        builder.append_data(&mut header, MANIFEST_NAME, json.as_slice())?;
        let encoder = builder.into_inner()?;
        encoder.finish()?;
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
    )
}

/// Files of a Core datadir that are never copied: wallets and runtime state
/// that a chain-only restore must not carry (stale auth, peer state, logs).
fn excluded_from_core(name: &str) -> bool {
    matches!(
        name,
        "wallets"
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

/// Walk the Core datadir, append its files under `bitcoin/` and record the
/// per-file table (sizes always, SHA-256 when `hash_files`). `prefix` is the
/// archive path of `dir` (`bitcoin`, then `bitcoin/regtest`, ...).
fn add_core_tree<W: Write>(
    builder: &mut tar::Builder<W>,
    dir: &Path,
    prefix: &str,
    files: &mut Vec<FileEntry>,
    hash_files: bool,
) -> Result<u64, E> {
    let mut total = 0u64;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if excluded_from_core(&name) {
            continue;
        }
        let path = entry.path();
        let tar_name = format!("{prefix}/{name}");
        if entry.file_type()?.is_dir() {
            builder.append_dir(&tar_name, &path)?;
            total += add_core_tree(builder, &path, &tar_name, files, hash_files)?;
        } else {
            let size = entry.metadata()?.len();
            let sha256 = if hash_files {
                Some(sha256_file(&path)?)
            } else {
                None
            };
            files.push(FileEntry {
                path: tar_name.clone(),
                size,
                sha256,
            });
            total += size;
            builder.append_path_with_name(&path, &tar_name)?;
        }
    }
    Ok(total)
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

/// Extract a snapshot into `target` (must be new or empty) and verify it.
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
    fs::create_dir_all(target)?;

    let file = File::open(input)?;
    let mut archive = tar::Archive::new(GzDecoder::new(file));
    for entry in archive.entries()? {
        let mut entry = entry?;
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
    if manifest.format != FORMAT && manifest.format != FORMAT_V2 {
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

    // Format 2: verify the Core section before trusting the pair. Sizes are
    // checked always; hashes only when the export recorded them and the
    // operator asked for verification.
    if manifest.format >= FORMAT_V2 {
        verify_core_files(target, &manifest, options.verify_hashes)?;
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
        ] {
            assert!(excluded_from_core(name), "{name} must be excluded");
        }
        // Chain data is copied.
        for name in ["blocks", "chainstate", "indexes", "bitcoin.conf"] {
            assert!(!excluded_from_core(name), "{name} must be copied");
        }
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

        let archive_path = dir.join("core.tar.gz");
        {
            let file = File::create(&archive_path).unwrap();
            let enc = GzEncoder::new(file, Compression::default());
            let mut tar = tar::Builder::new(enc);
            let mut files = Vec::new();
            let total = add_core_tree(&mut tar, &core, "bitcoin", &mut files, true).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
            assert_eq!(total, 3 + 5 + 8, "wallets and .cookie must not count");
        }
        let file = File::open(&archive_path).unwrap();
        let mut names = Vec::new();
        for entry in tar::Archive::new(GzDecoder::new(file)).entries().unwrap() {
            let entry = entry.unwrap();
            if entry.header().entry_type().is_file() {
                names.push(entry.path().unwrap().display().to_string());
            }
        }
        names.sort();
        assert_eq!(
            names,
            vec![
                "bitcoin/bitcoin.conf",
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
}
