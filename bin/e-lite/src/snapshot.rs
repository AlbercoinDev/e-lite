//! Consistent index snapshots: tar.gz of the database plus a manifest with the
//! network identity, committed tip and canonical digest.

use el_model::codec::SCHEMA_VERSION;
use el_model::{Chain, Hash32};
use el_store::Store;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

type E = Box<dyn std::error::Error + Send + Sync>;

pub const MANIFEST_NAME: &str = "e-lite-snapshot.json";
const FORMAT: u32 = 1;

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
}

#[derive(Debug, Clone)]
pub struct SnapshotInfo {
    pub path: PathBuf,
    pub height: u32,
    pub hash: String,
    pub txnum: u64,
    pub digest: String,
    pub bytes: u64,
}

pub fn export(
    store: &Store,
    chain: Chain,
    output: &Path,
    version: &str,
) -> Result<SnapshotInfo, E> {
    store.flush_all()?;
    let tip = store
        .committed_tip()?
        .ok_or("cannot export an empty index; run `e-lite index` first")?;
    let identity = store
        .network_identity()?
        .ok_or("database is not initialized")?;
    let digest = store.canonical_digest()?;
    let manifest = Manifest {
        format: FORMAT,
        version: version.to_string(),
        network: identity.chain.name().to_string(),
        genesis: identity.genesis.to_hex(),
        height: tip.height,
        hash: tip.block_hash.to_hex(),
        schema: tip.schema_version,
        txnum: tip.txnum,
        digest: digest.to_hex(),
    };
    if manifest.network != chain.name() {
        return Err(format!(
            "database belongs to {}, config says {}",
            manifest.network,
            chain.name()
        )
        .into());
    }

    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = output.with_extension("tmp");
    {
        let file = File::create(&tmp)?;
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let data_dir = store.path();
        add_tree(&mut builder, data_dir, data_dir)?;
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
    })
}

fn add_tree<W: Write>(builder: &mut tar::Builder<W>, base: &Path, dir: &Path) -> Result<(), E> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // The rawtx cache is rebuildable and can be large. Logs and runtime
        // markers are not part of the index either.
        if name == "rawtx-cache"
            || name == "LOG"
            || name == "LOCK"
            || name == "e-lite.log"
            || name == "e-lite.log.1"
            || name == "banner.txt"
            || name == "core-paused-by-e-lite"
        {
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

/// Extract a snapshot into `target` (must be new or empty) and verify it.
pub fn import(input: &Path, target: &Path, chain: Chain) -> Result<SnapshotInfo, E> {
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
        // escape the target directory or create special files. Snapshots only
        // contain regular files and directories.
        if !kind.is_file() && !kind.is_dir() {
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
    if manifest.format != FORMAT {
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
    let bytes = fs::metadata(input)?.len();
    Ok(SnapshotInfo {
        path: target.to_path_buf(),
        height: tip.height,
        hash: tip.block_hash.to_hex(),
        txnum: tip.txnum,
        digest: digest.to_hex(),
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

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
        let err = import(&archive_path, &target, Chain::Regtest).unwrap_err();
        assert!(err.to_string().contains("unsafe snapshot entry type"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
