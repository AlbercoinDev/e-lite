//! `privacy.toml`: presets, ranges and overrides of the external path
//! (AUD-163).
//!
//! The file lives next to `config.toml` (like `public-servers.toml`) and is
//! created from the shipped template on the first run. Every key is
//! optional: a missing key takes the preset's value. An unknown key or a
//! value outside its range is a startup error naming the file, the key and
//! the range. `--set privacy.<section>.<key>=<value>` overrides the file.

use crate::config::Config;
use el_rawtx::{ExternalMode, PrivacyParams};
use std::path::{Path, PathBuf};

/// The template shipped with e-lite (also in the repository root). It is
/// documentation plus `preset = "balanced"`; a test keeps it equal to the
/// preset.
pub const TEMPLATE: &str = include_str!("../../../privacy.toml");

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PrivacyFile {
    preset: Option<String>,
    mode: Option<String>,
    #[serde(default)]
    decoys: DecoysSection,
    #[serde(default)]
    sessions: SessionsSection,
    #[serde(default)]
    blocks: BlocksSection,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DecoysSection {
    wallets: Option<i64>,
    size_min_ratio: Option<f64>,
    size_max_ratio: Option<f64>,
    era_margin_blocks: Option<i64>,
    seek_attempts: Option<i64>,
    growth: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionsSection {
    size: Option<i64>,
    min_servers: Option<i64>,
    max_share: Option<f64>,
    parallel: Option<i64>,
    batch_window_ms: Option<i64>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BlocksSection {
    decoys: Option<i64>,
}

/// One effective value and where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Origin {
    pub key: &'static str,
    pub value: String,
    /// `preset`, `file`, `--set` or `config.toml`.
    pub from: &'static str,
    pub range: &'static str,
}

#[derive(Debug, Clone)]
pub struct Loaded {
    pub path: PathBuf,
    /// `file`, `created` or `missing` (preset values).
    pub source: &'static str,
    pub params: PrivacyParams,
    pub origins: Vec<Origin>,
    pub warnings: Vec<String>,
}

/// Where `privacy.toml` lives: `[rawtx] privacy_file`, else next to the
/// active config file.
pub fn target_path(cfg: &Config, config_path: Option<&Path>) -> PathBuf {
    if let Some(path) = cfg
        .rawtx
        .privacy_file
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        return PathBuf::from(path);
    }
    let config = config_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| crate::config::home().join(".config/e-lite/config.toml"));
    config
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("privacy.toml")
}

/// Read (creating it from the template when missing) and validate.
pub fn load(cfg: &Config, config_path: Option<&Path>, create: bool) -> Result<Loaded, String> {
    let path = target_path(cfg, config_path);
    let mut source = "file";
    let text = if path.is_file() {
        std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read privacy file {}: {e}", path.display()))?
    } else if create {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(&path, TEMPLATE) {
            Ok(()) => {
                eprintln!(
                    "[e-lite] privacy: created {} from the template",
                    path.display()
                );
                source = "created";
            }
            Err(e) => {
                eprintln!(
                    "[e-lite] WARNING: cannot create privacy file {}: {e}; using the balanced preset",
                    path.display()
                );
                source = "missing";
            }
        }
        TEMPLATE.to_string()
    } else {
        source = "missing";
        String::new()
    };
    let mut loaded = parse(&text, &cfg.rawtx.privacy_overrides, cfg.rawtx.decoys)
        .map_err(|e| format!("invalid privacy file {}: {e}", path.display()))?;
    loaded.path = path;
    loaded.source = source;
    Ok(loaded)
}

/// Parse a file's text plus `--set privacy.*` overrides. `config_decoys` is
/// `[rawtx] decoys` from config.toml (the block-decoy default).
pub fn parse(text: &str, overrides: &[String], config_decoys: u32) -> Result<Loaded, String> {
    let mut value: toml::Value = if text.trim().is_empty() {
        toml::Value::Table(Default::default())
    } else {
        toml::from_str(text).map_err(|e| e.to_string())?
    };
    let mut set_keys: Vec<String> = Vec::new();
    for item in overrides {
        let (key, raw) = item
            .split_once('=')
            .ok_or_else(|| format!("--set {item}: expected privacy.<key>=<value>"))?;
        let key = key
            .trim()
            .strip_prefix("privacy.")
            .ok_or_else(|| format!("--set {item}: not a privacy key"))?;
        set_path(&mut value, key, parse_scalar(raw.trim()))?;
        set_keys.push(key.to_string());
    }
    let file_keys = present_keys(&value);
    let file: PrivacyFile = value
        .try_into()
        .map_err(|e: toml::de::Error| e.to_string())?;

    let preset_name = file.preset.clone().unwrap_or_else(|| "balanced".into());
    let mut p = PrivacyParams::preset(&preset_name).ok_or_else(|| {
        format!("preset = {preset_name:?}: expected \"balanced\", \"max\" or \"fast\"")
    })?;
    let mut origins = Vec::new();
    let from = |key: &str| -> &'static str {
        if set_keys.iter().any(|k| k == key) {
            "--set"
        } else if file_keys.iter().any(|k| k == key) {
            "file"
        } else {
            "preset"
        }
    };
    origins.push(Origin {
        key: "preset",
        value: preset_name.clone(),
        from: from("preset"),
        range: "balanced | max | fast",
    });

    if let Some(mode) = &file.mode {
        p.mode = match mode.as_str() {
            "tx" => ExternalMode::Tx,
            "block" => ExternalMode::Block,
            other => return Err(format!("mode = {other:?}: expected \"tx\" or \"block\"")),
        };
    }
    origins.push(Origin {
        key: "mode",
        value: p.mode.name().into(),
        from: from("mode"),
        range: "tx | block",
    });

    macro_rules! int {
        ($opt:expr, $field:expr, $key:literal, $lo:expr, $hi:expr, $range:literal) => {{
            if let Some(v) = $opt {
                if !($lo..=$hi).contains(&v) {
                    return Err(format!("{} = {v}: out of range {}", $key, $range));
                }
                $field = v as _;
            }
            origins.push(Origin {
                key: $key,
                value: $field.to_string(),
                from: from($key),
                range: $range,
            });
        }};
    }
    macro_rules! float {
        ($opt:expr, $field:expr, $key:literal, $lo:expr, $hi:expr, $range:literal) => {{
            if let Some(v) = $opt {
                if !($lo..=$hi).contains(&v) {
                    return Err(format!("{} = {v}: out of range {}", $key, $range));
                }
                $field = v;
            }
            origins.push(Origin {
                key: $key,
                value: format!("{}", $field),
                from: from($key),
                range: $range,
            });
        }};
    }

    let d = &file.decoys;
    int!(d.wallets, p.decoy_wallets, "decoys.wallets", 0, 8, "0..8");
    float!(
        d.size_min_ratio,
        p.decoy_size_min_ratio,
        "decoys.size_min_ratio",
        0.1,
        1.0,
        "0.1..1.0"
    );
    float!(
        d.size_max_ratio,
        p.decoy_size_max_ratio,
        "decoys.size_max_ratio",
        1.0,
        10.0,
        "1.0..10.0"
    );
    if p.decoy_size_max_ratio < p.decoy_size_min_ratio {
        return Err("decoys.size_max_ratio must not be below decoys.size_min_ratio".into());
    }
    int!(
        d.era_margin_blocks,
        p.decoy_era_margin_blocks,
        "decoys.era_margin_blocks",
        0,
        500_000,
        "0..500000"
    );
    int!(
        d.seek_attempts,
        p.decoy_seek_attempts,
        "decoys.seek_attempts",
        8,
        1024,
        "8..1024"
    );
    if let Some(g) = d.growth {
        p.decoy_growth = g;
    }
    origins.push(Origin {
        key: "decoys.growth",
        value: p.decoy_growth.to_string(),
        from: from("decoys.growth"),
        range: "true | false",
    });

    let s = &file.sessions;
    int!(s.size, p.session_size, "sessions.size", 1, 500, "1..500");
    int!(
        s.min_servers,
        p.min_servers,
        "sessions.min_servers",
        1,
        16,
        "1..16"
    );
    float!(
        s.max_share,
        p.max_share,
        "sessions.max_share",
        0.1,
        1.0,
        "0.1..1.0"
    );
    int!(
        s.parallel,
        p.parallel_sessions,
        "sessions.parallel",
        1,
        32,
        "1..32"
    );
    int!(
        s.batch_window_ms,
        p.batch_window_ms,
        "sessions.batch_window_ms",
        0,
        1000,
        "0..1000"
    );

    // Block decoys default to [rawtx] decoys from config.toml.
    p.block_decoys = config_decoys.min(8);
    let block_from_file = file.blocks.decoys.is_some();
    int!(
        file.blocks.decoys,
        p.block_decoys,
        "blocks.decoys",
        0,
        8,
        "0..8"
    );
    if !block_from_file {
        if let Some(last) = origins.last_mut() {
            last.from = "config.toml";
        }
    }

    let mut warnings = Vec::new();
    if p.mode == ExternalMode::Tx && p.decoy_wallets == 0 {
        warnings.push(
            "decoys.wallets = 0: servers see your wallet's transactions without decoy wallets"
                .to_string(),
        );
    }
    if p.mode == ExternalMode::Tx && p.min_servers == 1 {
        warnings.push(
            "sessions.min_servers = 1: one server may receive every transaction of a wallet"
                .to_string(),
        );
    }
    Ok(Loaded {
        path: PathBuf::new(),
        source: "file",
        params: p,
        origins,
        warnings,
    })
}

/// `true`, `false`, integers and floats become TOML scalars; anything else a
/// string.
fn parse_scalar(raw: &str) -> toml::Value {
    let unquoted = raw.trim_matches('"');
    if raw == "true" || raw == "false" {
        toml::Value::Boolean(raw == "true")
    } else if let Ok(i) = raw.parse::<i64>() {
        toml::Value::Integer(i)
    } else if let Ok(f) = raw.parse::<f64>() {
        toml::Value::Float(f)
    } else {
        toml::Value::String(unquoted.to_string())
    }
}

fn set_path(value: &mut toml::Value, key: &str, new: toml::Value) -> Result<(), String> {
    let parts: Vec<&str> = key.split('.').collect();
    let mut cur = value;
    for (i, part) in parts.iter().enumerate() {
        let table = cur
            .as_table_mut()
            .ok_or_else(|| format!("--set privacy.{key}: {part} is not a section"))?;
        if i + 1 == parts.len() {
            table.insert(part.to_string(), new);
            return Ok(());
        }
        cur = table
            .entry(part.to_string())
            .or_insert_with(|| toml::Value::Table(Default::default()));
    }
    Ok(())
}

/// Dotted keys present in a parsed file (`decoys.wallets`, `preset`, ...).
fn present_keys(value: &toml::Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(table) = value.as_table() {
        for (k, v) in table {
            match v.as_table() {
                Some(inner) => out.extend(inner.keys().map(|ik| format!("{k}.{ik}"))),
                None => out.push(k.clone()),
            }
        }
    }
    out
}

/// One line for the startup log.
pub fn summary(loaded: &Loaded) -> String {
    let p = &loaded.params;
    format!(
        "{} ({}): preset {}, mode {}, decoy wallets {}, sessions {} txids over >= {} servers \
         (max share {}), {} parallel, block decoys {}",
        loaded.path.display(),
        loaded.source,
        p.preset,
        p.mode.name(),
        p.decoy_wallets,
        p.session_size,
        p.min_servers,
        p.max_share,
        p.parallel_sessions,
        p.block_decoys
    )
}

/// `e-lite privacy check [--json]`.
pub fn cmd_check(cfg: &Config, config_path: Option<&Path>, json: bool) -> Result<(), String> {
    let loaded = load(cfg, config_path, false)?;
    if json {
        let values: Vec<serde_json::Value> = loaded
            .origins
            .iter()
            .map(|o| {
                serde_json::json!({"key": o.key, "value": o.value, "from": o.from, "range": o.range})
            })
            .collect();
        let out = serde_json::json!({
            "path": loaded.path.display().to_string(),
            "source": loaded.source,
            "valid": true,
            "values": values,
            "warnings": loaded.warnings,
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return Ok(());
    }
    println!(
        "privacy file: {} ({})",
        loaded.path.display(),
        if loaded.source == "missing" {
            "missing: preset values; created on the next run"
        } else {
            "valid"
        }
    );
    println!("{:<26} {:<10} {:<12} range", "key", "value", "from");
    for o in &loaded.origins {
        println!("{:<26} {:<10} {:<12} {}", o.key, o.value, o.from, o.range);
    }
    for w in &loaded.warnings {
        println!("warning: {w}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_is_the_balanced_preset() {
        let loaded = parse(TEMPLATE, &[], 1).unwrap();
        assert_eq!(loaded.params, PrivacyParams::balanced());
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn presets_and_explicit_values() {
        let max = parse("preset = \"max\"", &[], 1).unwrap().params;
        assert_eq!(max.mode, ExternalMode::Block);
        let fast = parse("preset = \"fast\"\n[decoys]\nwallets = 2\n", &[], 1).unwrap();
        assert_eq!(
            fast.params.decoy_wallets, 2,
            "the file wins over the preset"
        );
        assert_eq!(fast.params.session_size, 100);
        let o = fast
            .origins
            .iter()
            .find(|o| o.key == "decoys.wallets")
            .unwrap();
        assert_eq!(o.from, "file");
    }

    #[test]
    fn set_wins_over_the_file() {
        let loaded = parse(
            "[sessions]\nsize = 50\n",
            &["privacy.sessions.size=7".to_string()],
            1,
        )
        .unwrap();
        assert_eq!(loaded.params.session_size, 7);
        let o = loaded
            .origins
            .iter()
            .find(|o| o.key == "sessions.size")
            .unwrap();
        assert_eq!(o.from, "--set");
    }

    #[test]
    fn every_bound_is_accepted_and_beyond_it_rejected() {
        let cases: &[(&str, &str, &str, &str)] = &[
            ("decoys", "wallets", "0", "9"),
            ("decoys", "wallets", "8", "-1"),
            ("decoys", "size_min_ratio", "0.1", "0.05"),
            ("decoys", "size_max_ratio", "10.0", "10.5"),
            ("decoys", "era_margin_blocks", "500000", "500001"),
            ("decoys", "seek_attempts", "8", "7"),
            ("decoys", "seek_attempts", "1024", "1025"),
            ("sessions", "size", "1", "0"),
            ("sessions", "size", "500", "501"),
            ("sessions", "min_servers", "16", "17"),
            ("sessions", "max_share", "0.1", "0.09"),
            ("sessions", "parallel", "32", "33"),
            ("sessions", "batch_window_ms", "1000", "1001"),
            ("blocks", "decoys", "8", "9"),
        ];
        for (section, key, ok, bad) in cases {
            assert!(
                parse(&format!("[{section}]\n{key} = {ok}\n"), &[], 1).is_ok(),
                "{section}.{key} = {ok}"
            );
            let err = parse(&format!("[{section}]\n{key} = {bad}\n"), &[], 1).unwrap_err();
            assert!(err.contains(key) && err.contains("range"), "{err}");
        }
    }

    #[test]
    fn unknown_keys_and_presets_are_errors() {
        assert!(parse("[decoys]\nwalets = 3\n", &[], 1).is_err());
        assert!(parse("preset = \"paranoid\"", &[], 1).is_err());
        assert!(parse("mode = \"blocks\"", &[], 1).is_err());
        assert!(parse(
            "[decoys]\nsize_min_ratio = 0.9\nsize_max_ratio = 1.0\n",
            &[],
            1
        )
        .is_ok());
        assert!(parse(
            "[decoys]\nsize_min_ratio = 1.0\nsize_max_ratio = 1.0\n",
            &[],
            1
        )
        .is_ok());
    }

    #[test]
    fn risky_values_warn() {
        let loaded = parse(
            "[decoys]\nwallets = 0\n[sessions]\nmin_servers = 1\n",
            &[],
            1,
        )
        .unwrap();
        assert_eq!(loaded.warnings.len(), 2);
    }

    #[test]
    fn block_decoys_default_to_the_config_value() {
        let loaded = parse("", &[], 3).unwrap();
        assert_eq!(loaded.params.block_decoys, 3);
        let o = loaded
            .origins
            .iter()
            .find(|o| o.key == "blocks.decoys")
            .unwrap();
        assert_eq!(o.from, "config.toml");
        assert_eq!(
            parse("[blocks]\ndecoys = 0\n", &[], 3)
                .unwrap()
                .params
                .block_decoys,
            0
        );
    }
}
