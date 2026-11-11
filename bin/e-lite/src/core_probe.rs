//! Probe Bitcoin Core state with a short-lived local Core process.
//!
//! Used by two snapshot paths:
//! - `export-snapshot --with-core`: record tip, prune height and version
//!   before the datadir is copied. The datadir is only consistent while Core
//!   is stopped, so e-lite refuses to export if a Core answers, then starts a
//!   probe instance, collects the facts via RPC and stops it again.
//! - `import-snapshot --verify=full`: open the restored datadir the same way
//!   and run a bounded `verifychain` plus tip checks.
//!
//! The probe never talks to the network (`-connect=0 -listen=0`) and always
//! stops the process it started, waiting for a clean exit (a large UTXO cache
//! needs time to flush, exactly like a managed Core).

use crate::config::Config;
use el_model::Chain;
use el_rpc::{CoreClient, CoreConfig};
use std::fs::{self, OpenOptions};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long the probe waits for Core RPC to answer. A pruned mainnet
/// chainstate opens in seconds; this also covers slow first loads.
const PROBE_RPC_WAIT: Duration = Duration::from_secs(180);
/// Grace for the probe's own Core shutdown, same rationale as the supervisor.
const PROBE_SHUTDOWN_GRACE: Duration = Duration::from_secs(120);
/// `verifychain` re-checks blocks from disk; it needs its own, long timeout.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(600);
/// Depth of the bounded `verifychain` at the tip.
const VERIFY_DEPTH: u32 = 288;

#[derive(Debug, Clone)]
pub struct CoreFacts {
    /// Core's `getnetworkinfo` subversion.
    pub version: String,
    pub tip_height: u64,
    pub tip_hash: String,
    pub pruned: bool,
    /// Lowest still-served block; 0 when Core is not pruned.
    pub prune_height: u64,
}

fn chain_arg(chain: Chain) -> &'static str {
    match chain {
        Chain::Mainnet => "main",
        Chain::Testnet4 => "testnet4",
        Chain::Signet => "signet",
        Chain::Regtest => "regtest",
    }
}

fn probe_client(cfg: &Config, chain: Chain, datadir: &Path) -> Result<CoreClient, String> {
    let cc = CoreConfig {
        rpc_url: cfg.rpc_url(chain),
        cookie_file: None,
        core_datadir: Some(datadir.to_path_buf()),
        rpc_user: cfg.core.rpc_user.clone(),
        rpc_password: cfg.core.rpc_password.clone(),
        chain,
        timeout: Duration::from_secs(2),
    };
    CoreClient::connect(&cc).map_err(|e| format!("Core RPC client: {e}"))
}

/// True when a Core already answers on the configured RPC URL.
pub fn core_running(cfg: &Config, chain: Chain) -> bool {
    probe_client(cfg, chain, &cfg.core_datadir(chain))
        .ok()
        .map(|c| c.blockchain_info().is_ok())
        .unwrap_or(false)
}

fn facts_from(client: &CoreClient) -> Result<CoreFacts, String> {
    let info = client.blockchain_info().map_err(|e| e.to_string())?;
    let net = client.network_info().map_err(|e| e.to_string())?;
    let tip_hash = client.block_hash(info.blocks).map_err(|e| e.to_string())?;
    Ok(CoreFacts {
        version: net.subversion,
        tip_height: info.blocks,
        tip_hash,
        pruned: info.pruned,
        prune_height: if info.pruned { info.pruneheight } else { 0 },
    })
}

fn spawn_probe(
    cfg: &Config,
    chain: Chain,
    datadir: &Path,
    log_file: &Path,
) -> Result<Child, String> {
    fs::create_dir_all(datadir).map_err(|e| format!("Core datadir {}: {e}", datadir.display()))?;
    if let Some(parent) = log_file.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file)
        .map_err(|e| format!("cannot open probe log {}: {e}", log_file.display()))?;
    let mut cmd = Command::new(&cfg.core.bitcoind);
    cmd.arg(format!("-datadir={}", datadir.display()));
    cmd.arg(format!("-chain={}", chain_arg(chain)));
    cmd.arg("-daemon=0");
    cmd.arg("-server=1");
    cmd.arg("-rpcbind=127.0.0.1");
    cmd.arg(format!("-rpcport={}", rpc_port(&cfg.rpc_url(chain), chain)));
    cmd.arg("-rpcallowip=127.0.0.1");
    // Isolated: no peers, no inbound, minimal cache. The probe only reads.
    cmd.arg("-connect=0");
    cmd.arg("-listen=0");
    cmd.arg("-dbcache=64");
    // Keep the operator's prune setting so `pruneheight` is meaningful for a
    // pruned datadir and a full datadir is not silently treated as prunable.
    cmd.arg(format!("-prune={}", cfg.core.prune_mib));
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(log);
    cmd.spawn()
        .map_err(|e| format!("cannot start {}: {e}", cfg.core.bitcoind))
}

fn rpc_port(url: &str, chain: Chain) -> u16 {
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let rest = rest.trim_end_matches('/');
    if let Some(port) = rest.rsplit_once(':').and_then(|(_, p)| p.parse().ok()) {
        return port;
    }
    chain.default_rpc_port()
}

fn wait_for_exit(child: Child, what: &str) -> Result<(), String> {
    let deadline = Instant::now() + PROBE_SHUTDOWN_GRACE;
    let mut child = child;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(200));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{what} did not exit within {}s; it was killed",
                    PROBE_SHUTDOWN_GRACE.as_secs()
                ));
            }
        }
    }
}

/// Stop the probe's Core (best effort) and wait for a clean exit. `child` is
/// wrapped so error paths can take it while the happy path keeps using it.
fn shutdown_probe(client: Option<&CoreClient>, child: &mut Option<Child>) -> Result<(), String> {
    if let Some(client) = client {
        if let Err(e) = client.stop() {
            eprintln!("[e-lite] probe: Core stop RPC failed ({e}); waiting for exit anyway");
        }
    }
    match child.take() {
        Some(child) => wait_for_exit(child, "probe Core"),
        None => Ok(()),
    }
}

fn wait_for_rpc(
    client: &CoreClient,
    child: &mut Option<Child>,
    log_file: &Path,
) -> Result<(), String> {
    let started = Instant::now();
    loop {
        match client.blockchain_info() {
            Ok(_) => return Ok(()),
            Err(_) if Instant::now() < started + PROBE_RPC_WAIT => {
                if let Some(child) = child.as_mut() {
                    if let Ok(Some(status)) = child.try_wait() {
                        return Err(format!(
                            "probe bitcoind exited early with {status}; see {}",
                            log_file.display()
                        ));
                    }
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(e) => {
                return Err(format!(
                    "Core did not answer RPC within {}s: {e}",
                    PROBE_RPC_WAIT.as_secs()
                ));
            }
        }
    }
}

/// Start a probe Core on `datadir`, collect the chain facts via RPC and stop
/// it again. Errors leave no Core running.
pub fn probe(cfg: &Config, chain: Chain, datadir: &Path) -> Result<CoreFacts, String> {
    let log_file = cfg.log_file(chain);
    let mut child = Some(spawn_probe(cfg, chain, datadir, &log_file)?);
    let client = match probe_client(cfg, chain, datadir) {
        Ok(c) => c,
        Err(e) => {
            let _ = shutdown_probe(None, &mut child);
            return Err(e);
        }
    };
    if let Err(e) = wait_for_rpc(&client, &mut child, &log_file) {
        let _ = shutdown_probe(Some(&client), &mut child);
        return Err(e);
    }
    let facts = match facts_from(&client) {
        Ok(f) => f,
        Err(e) => {
            let _ = shutdown_probe(Some(&client), &mut child);
            return Err(e);
        }
    };
    // Clean shutdown before the datadir is copied or handed back.
    shutdown_probe(Some(&client), &mut child)?;
    Ok(facts)
}

/// Facts for `export-snapshot --with-core`: refuse when Core is running (the
/// datadir would be copied inconsistently) and require a local RPC URL.
pub fn export_facts(
    cfg: &Config,
    chain: Chain,
    index_tip_height: u64,
) -> Result<CoreFacts, String> {
    if !el_rpc::rpc_url_is_loopback(&cfg.rpc_url(chain)) {
        return Err(
            "export-snapshot --with-core needs a local Core RPC (loopback); a remote \
             datadir cannot be copied from here"
                .to_string(),
        );
    }
    if core_running(cfg, chain) {
        return Err(
            "Bitcoin Core is running; stop it first so the datadir is copied in a \
             consistent state"
                .to_string(),
        );
    }
    let datadir = cfg.core_datadir(chain);
    if !datadir.is_dir() {
        return Err(format!(
            "Core datadir {} not found; --with-core needs the chain data to bundle",
            datadir.display()
        ));
    }
    let facts = probe(cfg, chain, &datadir)?;
    if facts.tip_height < index_tip_height {
        return Err(format!(
            "Core tip {} is behind the index tip {}; the pair cannot be exported",
            facts.tip_height, index_tip_height
        ));
    }
    Ok(facts)
}

#[derive(Debug)]
pub struct VerifyReport {
    pub core_tip_height: u64,
    pub core_tip_hash: String,
    pub index_tip_block_ok: bool,
    pub verifychain_ok: bool,
    pub prune_height: u64,
}

/// Full verification of a restored pair: start Core on the imported datadir,
/// check the tip and the index-tip block, run a bounded `verifychain`, stop.
pub fn verify_restored(
    cfg: &Config,
    chain: Chain,
    datadir: &Path,
    index_tip_height: u64,
    index_tip_hash: &str,
    expected_core_tip_hash: Option<&str>,
) -> Result<VerifyReport, String> {
    let log_file = cfg.log_file(chain);
    let mut child = Some(spawn_probe(cfg, chain, datadir, &log_file)?);
    let client = match probe_client(cfg, chain, datadir) {
        Ok(c) => c,
        Err(e) => {
            let _ = shutdown_probe(None, &mut child);
            return Err(e);
        }
    };
    if let Err(e) = wait_for_rpc(&client, &mut child, &log_file) {
        let _ = shutdown_probe(Some(&client), &mut child);
        return Err(e);
    }
    let facts = match facts_from(&client) {
        Ok(f) => f,
        Err(e) => {
            let _ = shutdown_probe(Some(&client), &mut child);
            return Err(e);
        }
    };
    let mut report = VerifyReport {
        core_tip_height: facts.tip_height,
        core_tip_hash: facts.tip_hash.clone(),
        index_tip_block_ok: false,
        verifychain_ok: false,
        prune_height: facts.prune_height,
    };
    // The Core section of the manifest must describe this datadir.
    if let Some(expected) = expected_core_tip_hash {
        if facts.tip_hash != expected {
            let _ = shutdown_probe(Some(&client), &mut child);
            return Err(format!(
                "restored Core tip {} does not match the manifest tip {expected}",
                facts.tip_hash
            ));
        }
    }
    // The index tip must exist in the restored chain.
    report.index_tip_block_ok = client
        .block_hash(index_tip_height)
        .map(|h| h == index_tip_hash)
        .unwrap_or(false);
    if !report.index_tip_block_ok {
        let _ = shutdown_probe(Some(&client), &mut child);
        return Err(format!(
            "the restored chain does not have the index tip {index_tip_hash} at \
             height {index_tip_height}"
        ));
    }
    // Long-timeout client for verifychain; the probe client has a 2 s timeout.
    let verifier = match verify_client(cfg, chain, datadir) {
        Ok(c) => c,
        Err(e) => {
            let _ = shutdown_probe(Some(&client), &mut child);
            return Err(e);
        }
    };
    report.verifychain_ok = match verifier.verify_chain(2, VERIFY_DEPTH) {
        Ok(ok) => ok,
        Err(e) => {
            let _ = shutdown_probe(Some(&client), &mut child);
            return Err(format!("verifychain failed: {e}"));
        }
    };
    shutdown_probe(Some(&client), &mut child)?;
    if !report.verifychain_ok {
        return Err("verifychain reported a problem with the restored blocks".to_string());
    }
    Ok(report)
}

fn verify_client(cfg: &Config, chain: Chain, datadir: &Path) -> Result<CoreClient, String> {
    let cc = CoreConfig {
        rpc_url: cfg.rpc_url(chain),
        cookie_file: None,
        core_datadir: Some(datadir.to_path_buf()),
        rpc_user: cfg.core.rpc_user.clone(),
        rpc_password: cfg.core.rpc_password.clone(),
        chain,
        timeout: VERIFY_TIMEOUT,
    };
    CoreClient::connect(&cc).map_err(|e| format!("Core RPC client: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_args_match_core_naming() {
        assert_eq!(chain_arg(Chain::Mainnet), "main");
        assert_eq!(chain_arg(Chain::Testnet4), "testnet4");
        assert_eq!(chain_arg(Chain::Signet), "signet");
        assert_eq!(chain_arg(Chain::Regtest), "regtest");
    }

    #[test]
    fn rpc_port_is_taken_from_the_url() {
        assert_eq!(rpc_port("http://127.0.0.1:55110", Chain::Testnet4), 55110);
        assert_eq!(
            rpc_port("http://127.0.0.1", Chain::Testnet4),
            Chain::Testnet4.default_rpc_port()
        );
    }

    #[test]
    fn a_dead_core_counts_as_not_running() {
        let mut cfg = Config::default();
        cfg.core.rpc_url = Some("http://127.0.0.1:1".to_string());
        cfg.core.rpc_user = Some("u".to_string());
        cfg.core.rpc_password = Some("p".to_string());
        assert!(!core_running(&cfg, Chain::Regtest));
    }
}
