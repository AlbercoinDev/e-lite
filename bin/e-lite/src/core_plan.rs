//! Starting Bitcoin Core with parameters that fit the machine.
//!
//! e-lite and Core validate the same chain at the same time, so their caches
//! compete for one RAM budget. Choosing `-dbcache` from the host's total while
//! e-lite also claims a quarter of RAM is how a 4 GiB box ends up swapping.
//!
//! Rules:
//! * only a Core that e-lite starts is configured; an external node is never
//!   touched;
//! * operator arguments always win for the knobs we are allowed to suggest;
//! * arguments e-lite must own (datadir, chain, RPC, pruning) are rejected
//!   instead of silently producing two conflicting values.

use crate::config::{Config, CoreManage};
use crate::tuning;
use el_model::Chain;

/// Core arguments that e-lite manages. Passing them through `extra_args`
/// would create two sources of truth for the same setting.
const MANAGED_ARGS: [&str; 9] = [
    "-datadir",
    "-chain",
    "-server",
    "-daemon",
    "-daemonwait",
    "-rpcbind",
    "-rpcport",
    "-rpcallowip",
    "-prune",
];

#[derive(Debug, Clone, Default)]
pub struct CorePlan {
    /// Arguments to append after the managed ones.
    pub args: Vec<String>,
    /// Human-readable explanation of what was chosen and why.
    pub notes: Vec<String>,
    pub dbcache_mib: u64,
    pub par: u64,
    pub rpcthreads: u64,
}

/// `-dbcache=512` -> `-dbcache`.
fn arg_name(arg: &str) -> String {
    match arg.split_once('=') {
        Some((name, _)) => name.trim().to_string(),
        None => arg.trim().to_string(),
    }
}

/// Reject operator arguments that would fight with the managed ones.
pub fn validate_extra_args(cfg: &Config) -> Result<(), String> {
    for arg in &cfg.core.extra_args {
        let name = arg_name(arg);
        if MANAGED_ARGS.contains(&name.as_str()) {
            return Err(format!(
                "[core] extra_args contains {name}, which e-lite manages itself. Remove it and \
                 use the matching [core]/[pruning] setting instead (datadir, chain, RPC bind/port, \
                 daemon mode and pruning are owned by e-lite)."
            ));
        }
    }
    Ok(())
}

fn operator_sets(cfg: &Config, name: &str) -> bool {
    cfg.core.extra_args.iter().any(|a| arg_name(a) == name)
}

/// Operator-supplied `-dbcache` in MiB, used by the e-lite memory budget even
/// when e-lite does not add the flag itself.
pub fn operator_dbcache_mib(cfg: &Config) -> Option<u64> {
    cfg.core.extra_args.iter().find_map(|a| {
        a.strip_prefix("-dbcache=")
            .and_then(|v| v.trim().parse::<u64>().ok())
    })
}

/// Choose Core's memory/CPU knobs for the detected machine.
///
/// `dbcache` is a quarter of total RAM capped at 2 GiB: enough to keep the
/// UTXO cache useful during IBD without starving the index, the page cache and
/// the rest of the system. `par` leaves one core for e-lite and the kernel.
/// The values are deliberately conservative; recordings of a real IBD decide
/// whether they should grow.
pub fn plan_core(cfg: &Config, chain: Chain) -> Result<CorePlan, String> {
    let mut plan = CorePlan::default();
    if cfg.core.manage == CoreManage::External {
        // An external node is never launched by e-lite, so its `bitcoin.conf`
        // and `extra_args` are none of our business.
        plan.notes
            .push("external Core: e-lite does not change its parameters".to_string());
        return Ok(plan);
    }
    validate_extra_args(cfg)?;

    let mem = tuning::memory_info();
    let total = mem.total_mib.unwrap_or(0).max(512);
    let available = mem.available_mib;
    // Core's `-par` is a thread count; using logical SMT threads on a 4C/8T
    // machine oversubscribes the CPU and starves the index. Count physical
    // cores (after affinity/cgroup quota) instead.
    let cpu = tuning::CpuInfo::detect();
    let cpus = cpu.physical_effective.max(1) as u64;

    // Fit `-dbcache` to what the OS can give now, not to installed RAM. On an
    // 8 GiB box that is ~1 GiB; a busy 32 GiB host with 4 GiB free gets 2 GiB
    // instead of the old 4 GiB that pushed it into swap.
    let dbcache = tuning::fitted_dbcache_mib(total, available);
    let par = cpus.saturating_sub(1).clamp(1, 8);
    let rpcthreads = cpus.clamp(4, 16);

    if operator_sets(cfg, "-dbcache") {
        // Keep the operator value, and report it: leaving the field at its
        // default made the summary read "dbcache 0 MiB" while the real Core
        // reserve was the operator's number.
        plan.dbcache_mib = operator_dbcache_mib(cfg).unwrap_or(0);
        plan.notes
            .push("operator set -dbcache; keeping it".to_string());
    } else {
        plan.dbcache_mib = dbcache;
        plan.args.push(format!("-dbcache={dbcache}"));
    }
    if operator_sets(cfg, "-par") {
        plan.notes.push("operator set -par; keeping it".to_string());
    } else {
        plan.par = par;
        plan.args.push(format!("-par={par}"));
    }
    if operator_sets(cfg, "-rpcthreads") {
        plan.notes
            .push("operator set -rpcthreads; keeping it".to_string());
    } else {
        plan.rpcthreads = rpcthreads;
        plan.args.push(format!("-rpcthreads={rpcthreads}"));
    }

    // e-lite publishes its own Electrum onion. Core's own onion needs a Tor
    // control port that the managed torrc does not provide (only SocksPort and
    // HiddenService* are written), so Core would retry 127.0.0.1:9051 forever
    // and log an error each time. Disable it unless the operator explicitly
    // asked for Core's onion and supplies a control port.
    if operator_sets(cfg, "-listenonion") {
        plan.notes
            .push("operator set -listenonion; keeping it".to_string());
    } else {
        plan.args.push("-listenonion=0".to_string());
        plan.notes.push(
            "Core onion disabled (-listenonion=0); e-lite manages the Electrum onion".to_string(),
        );
    }

    let facts = NetFacts::detect(cfg, chain);
    let (net_args, net_notes) = network_args(cfg, &facts)?;
    plan.args.extend(net_args);
    plan.notes.extend(net_notes);

    // `prune_mib = 0` used to omit the flag entirely, so a `prune=` line in an
    // existing bitcoin.conf could turn a full node into a pruned one (or the
    // other way around). Always state it.
    plan.args.push(format!("-prune={}", cfg.core.prune_mib));

    let dbcache_text = if operator_sets(cfg, "-dbcache") {
        match operator_dbcache_mib(cfg) {
            Some(v) => format!("dbcache {v} MiB (operator)"),
            None => "dbcache from -dbcache (operator)".to_string(),
        }
    } else {
        format!("dbcache {} MiB", plan.dbcache_mib)
    };
    plan.notes.push(format!(
        "{} MiB total, {} MiB available, {} physical cores ({} logical) -> {}, \
         par {}, rpcthreads {} ({} network)",
        total,
        available,
        cpu.physical_effective,
        cpu.logical,
        dbcache_text,
        plan.par,
        plan.rpcthreads,
        chain.name()
    ));
    Ok(plan)
}

/// What the host offers for Core's P2P privacy (AUD-136).
#[derive(Debug, Clone, Default)]
pub struct NetFacts {
    /// SOCKS5 address of a Tor that Core can use.
    pub tor_socks: Option<String>,
    /// I2P SAM router address.
    pub i2p_sam: Option<String>,
    /// The configured `bitcoind` knows `-privatebroadcast`.
    pub private_broadcast_supported: bool,
}

fn tcp_answers(addr: &str) -> bool {
    use std::net::ToSocketAddrs;
    addr.to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .is_some_and(|a| {
            std::net::TcpStream::connect_timeout(&a, std::time::Duration::from_millis(300)).is_ok()
        })
}

impl NetFacts {
    pub fn detect(cfg: &Config, chain: Chain) -> Self {
        // The managed Tor (its port is fixed by `tor::prepare` in `run`; before
        // that, or in `index`, the network default is the best guess), else an
        // operator proxy that answers.
        let tor_socks = if cfg.tor.managed() && crate::tor::resolve_binary(cfg).is_some() {
            let port = if cfg.tor.socks_port > 0 {
                cfg.tor.socks_port
            } else {
                chain.default_socks_port()
            };
            Some(format!("127.0.0.1:{port}"))
        } else {
            cfg.rawtx
                .tor_socks
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty() && tcp_answers(s))
                .map(str::to_string)
        };
        let i2p_sam = match cfg.core.i2p_sam.trim() {
            "" => None,
            "auto" => Some("127.0.0.1:7656".to_string()).filter(|a| tcp_answers(a)),
            addr => Some(addr.to_string()),
        };
        // Only asked when needed: it runs `bitcoind -help`.
        let private_broadcast_supported = cfg.core.private_broadcast
            && std::process::Command::new(&cfg.core.bitcoind)
                .arg("-help")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).contains("-privatebroadcast"))
                .unwrap_or(false);
        NetFacts {
            tor_socks,
            i2p_sam,
            private_broadcast_supported,
        }
    }
}

/// Core arguments an operator may set to take over the network setup.
const NETWORK_ARGS: [&str; 6] = [
    "-proxy",
    "-onion",
    "-onlynet",
    "-i2psam",
    "-connect",
    "-privatebroadcast",
];

/// P2P network arguments for a Core started by e-lite (AUD-136).
///
/// * `auto`: clearnet **plus** every private network available. Core then
///   also reaches onion/I2P/CJDNS peers; block download speed is unchanged.
/// * `private`: **only** private networks. No Bitcoin peer ever sees the
///   node's IP: no clearnet peers, no listening, no DNS, no address
///   discovery. Slower IBD. Fails when no private network is available.
/// * `clearnet`: nothing is added.
///
/// `private_broadcast` adds `-privatebroadcast=1` when Core supports it and
/// Tor or I2P is available: wallet transactions leave through short-lived
/// Tor/I2P connections even though blocks still come over clearnet.
pub fn network_args(cfg: &Config, facts: &NetFacts) -> Result<(Vec<String>, Vec<String>), String> {
    let mut args = Vec::new();
    let mut notes = Vec::new();
    let mode = cfg.core.network.as_str();
    if let Some(set) = NETWORK_ARGS.iter().find(|a| operator_sets(cfg, a)) {
        if mode == "private" {
            return Err(format!(
                "[core] network = \"private\" conflicts with {set} in [core] extra_args; \
                 remove one of them"
            ));
        }
        notes.push(format!(
            "operator set {set}; e-lite leaves Core's network settings alone"
        ));
        return Ok((args, notes));
    }
    if mode == "clearnet" {
        notes.push("Core network: clearnet only ([core] network = \"clearnet\")".to_string());
        return Ok((args, notes));
    }
    let private = mode == "private";
    let mut nets: Vec<&str> = Vec::new();
    if let Some(socks) = &facts.tor_socks {
        if private {
            // Everything through Tor, including name lookups.
            args.push(format!("-proxy={socks}"));
            args.push("-onlynet=onion".to_string());
        } else {
            args.push(format!("-onion={socks}"));
        }
        nets.push("onion");
    }
    if let Some(sam) = &facts.i2p_sam {
        args.push(format!("-i2psam={sam}"));
        // Outbound only: no persistent I2P address is published.
        args.push("-i2pacceptincoming=0".to_string());
        if private {
            args.push("-onlynet=i2p".to_string());
        }
        nets.push("i2p");
    }
    if cfg.core.cjdns {
        args.push("-cjdnsreachable=1".to_string());
        if private {
            args.push("-onlynet=cjdns".to_string());
        }
        nets.push("cjdns");
    }
    if private {
        if nets.is_empty() {
            return Err(
                "[core] network = \"private\" needs a private network: the managed Tor \
                 ([tor] mode = \"managed\"), a reachable [rawtx] tor_socks, an I2P SAM router \
                 ([core] i2p_sam) or CJDNS ([core] cjdns = true). Refusing to start Core on \
                 clearnet."
                    .to_string(),
            );
        }
        args.push("-listen=0".to_string());
        args.push("-dnsseed=0".to_string());
        args.push("-dns=0".to_string());
        args.push("-discover=0".to_string());
        args.push("-natpmp=0".to_string());
        notes.push(format!(
            "Core network: private only ({}); no clearnet peers, no listening, no DNS",
            nets.join(" + ")
        ));
    } else if nets.is_empty() {
        notes.push("Core network: clearnet only (no Tor, I2P or CJDNS available)".to_string());
    } else {
        notes.push(format!("Core network: clearnet + {}", nets.join(" + ")));
    }
    if cfg.core.private_broadcast {
        let anonymous = facts.tor_socks.is_some() || facts.i2p_sam.is_some();
        if !facts.private_broadcast_supported {
            notes.push(
                "WARNING: private_broadcast requested but this bitcoind has no \
                 -privatebroadcast; wallet transactions are broadcast normally"
                    .to_string(),
            );
        } else if !anonymous {
            notes.push(
                "WARNING: private_broadcast requested but neither Tor nor I2P is \
                 available; wallet transactions are broadcast normally"
                    .to_string(),
            );
        } else {
            args.push("-privatebroadcast=1".to_string());
            notes.push(
                "wallet transactions are broadcast over short-lived Tor/I2P connections \
                 (-privatebroadcast)"
                    .to_string(),
            );
        }
    }
    Ok((args, notes))
}

/// One-line summary for `doctor` and the startup log.
pub fn describe(plan: &CorePlan) -> String {
    if plan.args.is_empty() {
        plan.notes.join("; ")
    } else {
        format!("{} ({})", plan.args.join(" "), plan.notes.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        let mut c = Config::default();
        c.network.name = "mainnet".to_string();
        c.core.manage = CoreManage::Managed;
        c
    }

    #[test]
    fn managed_args_cannot_be_overridden() {
        let mut c = cfg();
        c.core.extra_args = vec!["-prune=550".to_string()];
        assert!(validate_extra_args(&c).is_err());
        c.core.extra_args = vec!["-datadir=/tmp/x".to_string()];
        assert!(validate_extra_args(&c).is_err());
        c.core.extra_args = vec!["-dbcache=512".to_string()];
        assert!(validate_extra_args(&c).is_ok());
    }

    #[test]
    fn operator_values_are_preserved() {
        let mut c = cfg();
        c.core.extra_args = vec!["-dbcache=3000".to_string()];
        let plan = plan_core(&c, Chain::Mainnet).unwrap();
        assert!(plan.args.iter().any(|a| a == "-prune=1"));
        assert!(!plan.args.iter().any(|a| a.starts_with("-dbcache=")));
        // The plan reports the operator value it keeps: printing 0 made the
        // startup line read "dbcache 0 MiB" while Core really got 3000.
        assert_eq!(plan.dbcache_mib, 3000);
        assert!(
            plan.notes
                .iter()
                .any(|n| n.contains("dbcache 3000 MiB (operator)")),
            "{:?}",
            plan.notes
        );
        assert!(plan.args.iter().any(|a| a.starts_with("-par=")));
        assert_eq!(operator_dbcache_mib(&c), Some(3000));
    }

    #[test]
    fn core_onion_is_disabled_by_default() {
        let c = cfg();
        let plan = plan_core(&c, Chain::Mainnet).unwrap();
        assert!(plan.args.iter().any(|a| a == "-listenonion=0"));
        assert!(plan.notes.iter().any(|n| n.contains("Core onion disabled")));
    }

    #[test]
    fn operator_can_keep_core_onion() {
        let mut c = cfg();
        c.core.extra_args = vec!["-listenonion=1".to_string()];
        let plan = plan_core(&c, Chain::Mainnet).unwrap();
        assert!(!plan.args.iter().any(|a| a == "-listenonion=0"));
        assert!(plan
            .notes
            .iter()
            .any(|n| n.contains("operator set -listenonion")));
    }

    #[test]
    fn prune_zero_is_explicit() {
        let mut c = cfg();
        c.core.prune_mib = 0;
        let plan = plan_core(&c, Chain::Mainnet).unwrap();
        assert!(plan.args.iter().any(|a| a == "-prune=0"));
    }

    #[test]
    fn external_core_is_never_configured() {
        let mut c = cfg();
        c.core.manage = CoreManage::External;
        let plan = plan_core(&c, Chain::Mainnet).unwrap();
        assert!(plan.args.is_empty());
    }

    fn facts(tor: bool, i2p: bool, pb: bool) -> NetFacts {
        NetFacts {
            tor_socks: tor.then(|| "127.0.0.1:9050".to_string()),
            i2p_sam: i2p.then(|| "127.0.0.1:7656".to_string()),
            private_broadcast_supported: pb,
        }
    }

    #[test]
    fn auto_network_adds_private_networks_to_clearnet() {
        let c = cfg();
        let (args, notes) = network_args(&c, &facts(true, true, false)).unwrap();
        assert_eq!(
            args,
            vec![
                "-onion=127.0.0.1:9050",
                "-i2psam=127.0.0.1:7656",
                "-i2pacceptincoming=0"
            ]
        );
        // Clearnet stays: nothing restricts or proxies it.
        assert!(!args
            .iter()
            .any(|a| a.starts_with("-onlynet") || a.starts_with("-proxy")));
        assert!(notes.iter().any(|n| n.contains("clearnet + onion + i2p")));
        // Nothing available: Core starts exactly as before.
        let (args, notes) = network_args(&c, &facts(false, false, false)).unwrap();
        assert!(args.is_empty());
        assert!(notes.iter().any(|n| n.contains("clearnet only")));
    }

    #[test]
    fn private_network_never_touches_clearnet() {
        let mut c = cfg();
        c.core.network = "private".to_string();
        c.core.cjdns = true;
        let (args, _) = network_args(&c, &facts(true, true, false)).unwrap();
        for wanted in [
            "-proxy=127.0.0.1:9050",
            "-onlynet=onion",
            "-onlynet=i2p",
            "-onlynet=cjdns",
            "-listen=0",
            "-dnsseed=0",
            "-dns=0",
            "-discover=0",
        ] {
            assert!(
                args.iter().any(|a| a == wanted),
                "{wanted} missing in {args:?}"
            );
        }
        assert!(!args
            .iter()
            .any(|a| a == "-onlynet=ipv4" || a == "-onlynet=ipv6"));
        // Fail closed: never fall back to clearnet.
        c.core.cjdns = false;
        let err = network_args(&c, &facts(false, false, false)).unwrap_err();
        assert!(err.contains("Refusing to start Core on clearnet"), "{err}");
        // An operator proxy would fight with the private plan.
        c.core.extra_args = vec!["-proxy=10.0.0.1:9050".to_string()];
        assert!(network_args(&c, &facts(true, false, false)).is_err());
    }

    #[test]
    fn clearnet_mode_and_operator_args_add_nothing() {
        let mut c = cfg();
        c.core.network = "clearnet".to_string();
        assert!(network_args(&c, &facts(true, true, true))
            .unwrap()
            .0
            .is_empty());
        let mut c = cfg();
        c.core.extra_args = vec!["-onlynet=onion".to_string()];
        let (args, notes) = network_args(&c, &facts(true, true, true)).unwrap();
        assert!(args.is_empty());
        assert!(notes.iter().any(|n| n.contains("operator set -onlynet")));
    }

    #[test]
    fn private_broadcast_needs_support_and_an_anonymous_network() {
        let mut c = cfg();
        c.core.private_broadcast = true;
        let (args, _) = network_args(&c, &facts(true, false, true)).unwrap();
        assert!(args.iter().any(|a| a == "-privatebroadcast=1"));
        // Old Core: the flag would stop it from starting.
        let (args, notes) = network_args(&c, &facts(true, false, false)).unwrap();
        assert!(!args.iter().any(|a| a.starts_with("-privatebroadcast")));
        assert!(notes.iter().any(|n| n.contains("WARNING")));
        // No Tor/I2P: nothing to broadcast through.
        let (args, notes) = network_args(&c, &facts(false, false, true)).unwrap();
        assert!(!args.iter().any(|a| a.starts_with("-privatebroadcast")));
        assert!(notes.iter().any(|n| n.contains("neither Tor nor I2P")));
        // Off by default.
        let (args, _) = network_args(&cfg(), &facts(true, true, true)).unwrap();
        assert!(!args.iter().any(|a| a.starts_with("-privatebroadcast")));
    }
}
