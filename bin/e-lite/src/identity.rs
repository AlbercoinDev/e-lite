//! Identity backup (AUD-143): what makes an instance *this* instance for the
//! wallets that use it, kept apart from the chain-data snapshot.
//!
//! A snapshot is public chain data and `import-snapshot` refuses secrets on
//! purpose. Restoring only a snapshot therefore yields a node with a new
//! onion address, a new local CA (wallets must import trust again) and new
//! decoy wallets. `export-identity` packs exactly those pieces:
//!
//! * `<data-dir>/tls/` — the local CA and the server certificate and keys;
//! * `<tor-dir>/onion/` — the onion service keys and hostname;
//! * `<data-dir>/decoy.key` — the secret the decoy wallets derive from;
//! * `config.toml`, `public-servers.toml`, `privacy.toml`.
//!
//! The archive contains private keys and is not encrypted: it is created
//! `0600` and the operator is told to encrypt it before it leaves the host.

use crate::config::Config;
use el_model::Chain;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

type E = Box<dyn std::error::Error + Send + Sync>;

const MANIFEST: &str = "e-lite-identity.json";
const FORMAT: u32 = 1;
/// No identity file is anywhere near this; a larger entry is not ours.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_FILES: usize = 64;

/// Where each archive entry lives on this host.
pub struct Layout {
    pub data_dir: PathBuf,
    pub onion_dir: PathBuf,
    pub config: PathBuf,
    pub public_servers: PathBuf,
    pub privacy: PathBuf,
}

impl Layout {
    pub fn of(cfg: &Config, chain: Chain, config_path: Option<&Path>) -> Self {
        let config = config_path
            .map(Path::to_path_buf)
            .unwrap_or_else(|| crate::config::home().join(".config/e-lite/config.toml"));
        Layout {
            data_dir: cfg.data_dir(chain),
            onion_dir: cfg.tor_dir(chain).join("onion"),
            public_servers: crate::public_servers::target_path(cfg, config_path),
            privacy: crate::privacy::target_path(cfg, config_path),
            config,
        }
    }

    /// The host path of an archive entry, or `None` for a name that is not
    /// part of an identity. This is the allowlist: nothing else is written.
    fn target(&self, entry: &str) -> Option<PathBuf> {
        let plain = |name: &str| {
            !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
        };
        if let Some(name) = entry.strip_prefix("data/tls/") {
            return plain(name).then(|| self.data_dir.join("tls").join(name));
        }
        if let Some(name) = entry.strip_prefix("data/tor/onion/") {
            return plain(name).then(|| self.onion_dir.join(name));
        }
        match entry {
            "data/decoy.key" => Some(self.data_dir.join("decoy.key")),
            "config/config.toml" => Some(self.config.clone()),
            "config/public-servers.toml" => Some(self.public_servers.clone()),
            "config/privacy.toml" => Some(self.privacy.clone()),
            _ => None,
        }
    }

    /// Every identity file present on this host, by archive name.
    fn present(&self) -> Result<BTreeMap<String, PathBuf>, E> {
        let mut out = BTreeMap::new();
        for (prefix, dir) in [
            ("data/tls/", self.data_dir.join("tls")),
            ("data/tor/onion/", self.onion_dir.clone()),
        ] {
            let Ok(read) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in read {
                let entry = entry?;
                // Regular files only: no directory, link or socket is part
                // of an identity (`symlink_metadata` does not follow links).
                if !entry.path().symlink_metadata()?.is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.ends_with(".tmp") {
                    continue;
                }
                out.insert(format!("{prefix}{name}"), entry.path());
            }
        }
        let singles = [
            ("data/decoy.key", self.data_dir.join("decoy.key")),
            ("config/config.toml", self.config.clone()),
            ("config/public-servers.toml", self.public_servers.clone()),
            ("config/privacy.toml", self.privacy.clone()),
        ];
        for (name, path) in singles {
            if path.symlink_metadata().is_ok_and(|m| m.is_file()) {
                out.insert(name.to_string(), path);
            }
        }
        Ok(out)
    }
}

/// Whether a running e-lite holds this data directory: RocksDB keeps a
/// `fcntl` write lock on `<data-dir>/LOCK` for as long as the database is
/// open. Probing the lock does not open the database.
pub fn node_is_running(data_dir: &Path) -> bool {
    use std::os::unix::io::AsRawFd;
    let Ok(file) = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(data_dir.join("LOCK"))
    else {
        return false;
    };
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as libc::c_short;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    // F_GETLK only asks: it never takes the lock.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut lock) };
    rc == 0 && lock.l_type != libc::F_UNLCK as libc::c_short
}

fn refuse_while_running(layout: &Layout, what: &str) -> Result<(), E> {
    if node_is_running(&layout.data_dir) {
        return Err(format!(
            "e-lite is running on {}; stop it before {what}",
            layout.data_dir.display()
        )
        .into());
    }
    Ok(())
}

pub struct Exported {
    pub files: Vec<String>,
    pub bytes: u64,
}

/// Write the identity of this instance to `output` (a new file, `0600`).
pub fn export(layout: &Layout, chain: Chain, version: &str, output: &Path) -> Result<Exported, E> {
    refuse_while_running(layout, "exporting its identity")?;
    let files = layout.present()?;
    if !files.keys().any(|name| name.starts_with("data/")) {
        return Err(format!(
            "no identity found under {} (no tls/, onion keys or decoy.key): nothing to export",
            layout.data_dir.display()
        )
        .into());
    }
    if output.exists() {
        return Err(format!("{} already exists; choose another name", output.display()).into());
    }
    let tmp = output.with_extension("identity-tmp");
    let result = (|| -> Result<Exported, E> {
        let file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        let mut tar = tar::Builder::new(GzEncoder::new(file, flate2::Compression::default()));
        let mut bytes = 0u64;
        let mut listed = Vec::new();
        let mut contents = Vec::new();
        for (name, path) in &files {
            let data = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            if data.len() as u64 > MAX_FILE_BYTES {
                return Err(
                    format!("{} is too large to be an identity file", path.display()).into(),
                );
            }
            let mode = fs::metadata(path)?.permissions().mode() & 0o777;
            listed.push(serde_json::json!({"path": name, "size": data.len(), "mode": mode}));
            bytes += data.len() as u64;
            contents.push((name.clone(), data, mode));
        }
        let manifest = serde_json::to_vec_pretty(&serde_json::json!({
            "format": FORMAT,
            "version": version,
            "network": chain.name(),
            "files": listed,
        }))?;
        append(&mut tar, MANIFEST, &manifest, 0o600)?;
        for (name, data, mode) in &contents {
            append(&mut tar, name, data, *mode)?;
        }
        let file = tar.into_inner()?.finish()?;
        file.sync_all()?;
        fs::rename(&tmp, output)?;
        Ok(Exported {
            files: files.keys().cloned().collect(),
            bytes,
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn append<W: Write>(
    tar: &mut tar::Builder<W>,
    name: &str,
    data: &[u8],
    mode: u32,
) -> Result<(), E> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(mode);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    tar.append_data(&mut header, name, data)?;
    Ok(())
}

/// The `config.toml` an archive carries, if any. On a host where no
/// configuration exists yet, it says where the data directory is.
pub fn archived_config(input: &Path) -> Result<Option<String>, E> {
    let file = fs::File::open(input).map_err(|e| format!("{}: {e}", input.display()))?;
    let mut archive = tar::Archive::new(GzDecoder::new(file));
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()?.to_string_lossy() == "config/config.toml" {
            if entry.size() > MAX_FILE_BYTES {
                return Err("config/config.toml: too large".into());
            }
            let mut text = String::new();
            entry.read_to_string(&mut text)?;
            return Ok(Some(text));
        }
    }
    Ok(None)
}

pub struct Imported {
    pub written: Vec<PathBuf>,
    pub unchanged: Vec<PathBuf>,
}

/// Restore an identity archive. Nothing is written unless every entry is an
/// identity file of this network and, without `overwrite`, no existing file
/// would change.
pub fn import(layout: &Layout, chain: Chain, input: &Path, overwrite: bool) -> Result<Imported, E> {
    refuse_while_running(layout, "importing an identity")?;
    let file = fs::File::open(input).map_err(|e| format!("{}: {e}", input.display()))?;
    let mut archive = tar::Archive::new(GzDecoder::new(file));
    let mut manifest: Option<serde_json::Value> = None;
    let mut staged: Vec<(PathBuf, Vec<u8>, u32)> = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.to_string_lossy().into_owned();
        if entry.header().entry_type() != tar::EntryType::Regular {
            return Err(format!("{name}: not a regular file; refusing the archive").into());
        }
        if entry.size() > MAX_FILE_BYTES || staged.len() >= MAX_FILES {
            return Err(format!("{name}: too large for an identity archive").into());
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        if name == MANIFEST {
            manifest = Some(serde_json::from_slice(&data)?);
            continue;
        }
        let target = layout
            .target(&name)
            .ok_or_else(|| format!("{name}: not an identity file; refusing the archive"))?;
        staged.push((target, data, entry.header().mode()? & 0o777));
    }
    let manifest = manifest.ok_or("not an e-lite identity archive (no manifest)")?;
    if manifest["format"].as_u64() != Some(u64::from(FORMAT)) {
        return Err(format!("unsupported identity format {}", manifest["format"]).into());
    }
    let network = manifest["network"].as_str().unwrap_or("");
    if network != chain.name() {
        return Err(format!(
            "this identity belongs to {network}, not {}; use --network {network}",
            chain.name()
        )
        .into());
    }
    if staged.is_empty() {
        return Err("the identity archive is empty".into());
    }

    let differs = |target: &Path, data: &[u8]| fs::read(target).is_ok_and(|old| old != data);
    let conflicts: Vec<String> = staged
        .iter()
        .filter(|(target, data, _)| differs(target, data))
        .map(|(target, _, _)| target.display().to_string())
        .collect();
    if !conflicts.is_empty() && !overwrite {
        return Err(format!(
            "these files exist with different contents (nothing was written; --yes replaces \
             them):\n  {}",
            conflicts.join("\n  ")
        )
        .into());
    }

    let mut written = Vec::new();
    let mut unchanged = Vec::new();
    for (target, data, mode) in staged {
        if fs::read(&target).is_ok_and(|old| old == data) {
            let _ = fs::set_permissions(&target, fs::Permissions::from_mode(mode));
            unchanged.push(target);
            continue;
        }
        write_file(&target, &data, mode)?;
        written.push(target);
    }
    Ok(Imported { written, unchanged })
}

/// Private directories, then a temporary file renamed into place.
fn write_file(target: &Path, data: &[u8], mode: u32) -> Result<(), E> {
    if let Some(parent) = target.parent() {
        if !parent.exists() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .map_err(|e| format!("{}: {e}", parent.display()))?;
        }
    }
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned());
    let tmp = target.with_file_name(format!(".{}.identity-tmp", name.unwrap_or_default()));
    let result = (|| -> Result<(), E> {
        let _ = fs::remove_file(&tmp);
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        file.write_all(data)?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.sync_all()?;
        fs::rename(&tmp, target).map_err(|e| format!("{}: {e}", target.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(root: &Path) -> Layout {
        Layout {
            data_dir: root.join("data"),
            onion_dir: root.join("data/tor/onion"),
            config: root.join("etc/config.toml"),
            public_servers: root.join("etc/public-servers.toml"),
            privacy: root.join("etc/privacy.toml"),
        }
    }

    fn put(path: &Path, data: &[u8], mode: u32) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn tmp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("e-lite-identity-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample(root: &Path) -> Layout {
        let l = layout(root);
        put(&l.data_dir.join("tls/ca.crt"), b"CA CERT", 0o644);
        put(&l.data_dir.join("tls/ca.key"), b"CA KEY", 0o600);
        put(&l.data_dir.join("tls/self-signed.key"), b"LEAF KEY", 0o600);
        put(&l.onion_dir.join("hostname"), b"abc.onion\n", 0o600);
        put(
            &l.onion_dir.join("hs_ed25519_secret_key"),
            &[7u8; 96],
            0o600,
        );
        put(&l.data_dir.join("decoy.key"), &[9u8; 32], 0o600);
        put(&l.config, b"[network]\nname = \"regtest\"\n", 0o644);
        put(&l.privacy, b"preset = \"balanced\"\n", 0o644);
        // Not identity: must never be packed.
        put(&l.data_dir.join("CURRENT"), b"MANIFEST-1", 0o644);
        put(&l.data_dir.join("tls/half.tmp"), b"partial", 0o600);
        fs::create_dir_all(l.onion_dir.join("authorized_clients")).unwrap();
        l
    }

    /// Files and modes of an identity, by path relative to `root`.
    fn inventory(root: &Path, l: &Layout) -> BTreeMap<String, (Vec<u8>, u32)> {
        l.present()
            .unwrap()
            .into_iter()
            .map(|(name, path)| {
                let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
                let rel = path.strip_prefix(root).unwrap().display().to_string();
                (format!("{name}={rel}"), (fs::read(&path).unwrap(), mode))
            })
            .collect()
    }

    #[test]
    fn a_round_trip_restores_every_file_and_mode() {
        let a = tmp("src");
        let b = tmp("dst");
        let src = sample(&a);
        let archive = a.join("id.tar.gz");
        let exported = export(&src, Chain::Regtest, "test", &archive).unwrap();
        assert_eq!(exported.files.len(), 8, "{:?}", exported.files);
        assert!(!exported
            .files
            .iter()
            .any(|f| f.contains("CURRENT") || f.contains(".tmp")));
        assert_eq!(
            fs::metadata(&archive).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let dst = layout(&b);
        let imported = import(&dst, Chain::Regtest, &archive, false).unwrap();
        assert_eq!(imported.written.len(), 8);
        assert_eq!(inventory(&a, &src), inventory(&b, &dst));
        let onion = fs::metadata(&dst.onion_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            onion, 0o700,
            "tor refuses a hidden service directory others can read"
        );

        // Importing the same archive again changes nothing.
        let again = import(&dst, Chain::Regtest, &archive, false).unwrap();
        assert!(again.written.is_empty());
        assert_eq!(again.unchanged.len(), 8);
        let _ = fs::remove_dir_all(&a);
        let _ = fs::remove_dir_all(&b);
    }

    #[test]
    fn an_existing_different_file_is_kept_unless_asked() {
        let a = tmp("conflict-src");
        let b = tmp("conflict-dst");
        let src = sample(&a);
        let archive = a.join("id.tar.gz");
        export(&src, Chain::Regtest, "test", &archive).unwrap();
        let dst = layout(&b);
        put(&dst.data_dir.join("decoy.key"), &[1u8; 32], 0o600);

        let err = import(&dst, Chain::Regtest, &archive, false)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("decoy.key") && err.contains("--yes"), "{err}");
        assert!(
            !dst.data_dir.join("tls/ca.key").exists(),
            "nothing may be written"
        );
        assert_eq!(
            fs::read(dst.data_dir.join("decoy.key")).unwrap(),
            vec![1u8; 32]
        );

        import(&dst, Chain::Regtest, &archive, true).unwrap();
        assert_eq!(
            fs::read(dst.data_dir.join("decoy.key")).unwrap(),
            vec![9u8; 32]
        );
        let _ = fs::remove_dir_all(&a);
        let _ = fs::remove_dir_all(&b);
    }

    #[test]
    fn another_network_and_an_existing_output_are_refused() {
        let a = tmp("net");
        let src = sample(&a);
        let archive = a.join("id.tar.gz");
        export(&src, Chain::Regtest, "test", &archive).unwrap();
        let err = export(&src, Chain::Regtest, "test", &archive)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("already exists"), "{err}");
        let err = import(&layout(&tmp("net-dst")), Chain::Signet, &archive, false)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("belongs to regtest"), "{err}");
        let _ = fs::remove_dir_all(&a);
    }

    /// An archive is only ever unpacked into the identity locations.
    #[test]
    fn foreign_entries_are_refused_before_anything_is_written() {
        let root = tmp("evil");
        let dst = layout(&root);
        let manifest = serde_json::to_vec(&serde_json::json!({
            "format": FORMAT, "version": "x", "network": "regtest", "files": []
        }))
        .unwrap();
        for evil in [
            "data/tls/../../escape",
            "data/tls/sub/key",
            "/etc/passwd",
            "data/CURRENT",
            "data/tor/data/state",
            "config/other.toml",
            "bitcoin/wallet.dat",
        ] {
            let archive = root.join("evil.tar.gz");
            let _ = fs::remove_file(&archive);
            let file = fs::File::create(&archive).unwrap();
            let mut tar = tar::Builder::new(GzEncoder::new(file, flate2::Compression::default()));
            append(&mut tar, MANIFEST, &manifest, 0o600).unwrap();
            append(&mut tar, "data/decoy.key", &[3u8; 32], 0o600).unwrap();
            // `append_data` validates paths; write the raw name instead.
            let mut header = tar::Header::new_gnu();
            header.set_size(4);
            header.set_mode(0o600);
            header.set_entry_type(tar::EntryType::Regular);
            let name = header.as_old_mut().name.as_mut();
            name[..evil.len()].copy_from_slice(evil.as_bytes());
            header.set_cksum();
            tar.append(&header, &b"evil"[..]).unwrap();
            tar.into_inner().unwrap().finish().unwrap();

            let err = import(&dst, Chain::Regtest, &archive, true)
                .err()
                .unwrap()
                .to_string();
            assert!(err.contains("not an identity file"), "{evil}: {err}");
            assert!(
                !dst.data_dir.join("decoy.key").exists(),
                "{evil}: a file was written"
            );
        }
        assert!(!root.join("escape").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_running_node_is_detected_by_its_database_lock() {
        let dir = tmp("lock");
        assert!(!node_is_running(&dir), "no database, no node");
        let store = el_store::Store::open(&dir).unwrap();
        // The lock belongs to this process, and `fcntl` locks never conflict
        // inside one process: ask from another one.
        let status = std::process::Command::new("python3")
            .arg("-c")
            .arg(
                "import fcntl,struct,sys\n\
                 f=open(sys.argv[1],'r+b')\n\
                 l=fcntl.fcntl(f,fcntl.F_GETLK,struct.pack('hhqqi',fcntl.F_WRLCK,0,0,0,0))\n\
                 sys.exit(0 if struct.unpack('hhqqi',l)[0]!=fcntl.F_UNLCK else 1)",
            )
            .arg(dir.join("LOCK"))
            .status();
        if let Ok(status) = status {
            assert!(
                status.success(),
                "RocksDB holds a write lock on LOCK while open"
            );
        }
        drop(store);
        assert!(!node_is_running(&dir), "a closed database leaves no lock");
        let _ = fs::remove_dir_all(&dir);
    }
}
