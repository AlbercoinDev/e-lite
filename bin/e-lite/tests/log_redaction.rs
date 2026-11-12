//! AUD-132: log lines must never carry wallet-identifying data. This test
//! reads every log macro invocation in the workspace sources and rejects the
//! arguments that would print a script hash, a txid or the block hash of a
//! wallet request. Index-level block hashes (repair, reorg, startup tip) are
//! not wallet data and stay allowed.

use std::path::{Path, PathBuf};

const MACROS: [&str; 5] = [
    "eprintln!(",
    "println!(",
    "debug_log!(",
    "trace_log!(",
    "eprint!(",
];

/// Substrings that identify wallet data inside a log macro invocation.
const FORBIDDEN: [&str; 9] = [
    "to_electrum_hex",
    "{script",
    "script_hash)",
    "{txid",
    "txid.to_hex()",
    ", txid)",
    "{spk",
    "rec.hash",
    "{scripthash",
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The text of a macro call starting at `start` (just after the `(`), up to
/// the matching `)`. String literals are skipped when counting parentheses.
fn call_body(src: &str, start: usize) -> &str {
    let bytes = src.as_bytes();
    let (mut depth, mut i, mut in_str) = (1usize, start, false);
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if c == b'\\' {
                i += 1;
            } else if c == b'"' {
                in_str = false;
            }
        } else if c == b'"' {
            in_str = true;
        } else if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
            if depth == 0 {
                return &src[start..i];
            }
        }
        i += 1;
    }
    &src[start..]
}

/// Skip the `#[cfg(test)] mod tests` tail: tests may print anything.
fn production_part(src: &str) -> &str {
    match src.find("#[cfg(test)]\nmod tests") {
        Some(i) => &src[..i],
        None => src,
    }
}

/// Offending `(line, pattern)` pairs in one source text.
fn scan(src: &str) -> Vec<(usize, &'static str)> {
    let src = production_part(src);
    let mut found = Vec::new();
    for mac in MACROS {
        let mut from = 0;
        while let Some(pos) = src[from..].find(mac) {
            let start = from + pos + mac.len();
            // `eprintln!(` also matches inside `debug_eprintln!(` etc.; all of
            // them are log output, so no word-boundary check.
            let body = call_body(src, start);
            for bad in FORBIDDEN {
                if body.contains(bad) {
                    found.push((src[..start].lines().count(), bad));
                }
            }
            from = start;
        }
    }
    // `println!(` also matches inside `eprintln!(`: report each hit once.
    found.sort();
    found.dedup();
    found
}

#[test]
fn the_scanner_catches_the_leaks_it_is_meant_for() {
    // The exact line AUD-132 removed from el-serve, and a rawtx one.
    let old = r#"
        eprintln!(
            "[e-lite] get_history {} failed: {}",
            script.to_electrum_hex(),
            e.message
        );
        eprintln!("[e-lite] rawtx {txid} at height {height} failed: {e}");
        debug_log!("ok: height {} ({})", height, format!("{}", 1));
    "#;
    let hits: Vec<&str> = scan(old).into_iter().map(|(_, bad)| bad).collect();
    assert_eq!(hits, vec!["to_electrum_hex", "{txid"]);
}

#[test]
fn log_macros_do_not_print_wallet_data() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    rust_files(&root.join("bin"), &mut files);
    files.retain(|p| !p.components().any(|c| c.as_os_str() == "tests"));
    assert!(
        files.len() > 20,
        "sources not found under {}",
        root.display()
    );

    let mut offenders = Vec::new();
    for file in &files {
        let src = std::fs::read_to_string(file).unwrap();
        for (line, bad) in scan(&src) {
            offenders.push(format!("{}:{line}: `{bad}`", file.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "log output must not identify wallet data:\n{}",
        offenders.join("\n")
    );
}
