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

    let total = tuning::memory_info().total_mib.unwrap_or(0).max(512);
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4) as u64;

    let dbcache = (total / 4).clamp(450, 2048).min((total / 3).max(300));
    let par = cpus.saturating_sub(1).clamp(1, 8);
    let rpcthreads = cpus.clamp(4, 16);

    if operator_sets(cfg, "-dbcache") {
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

    // `prune_mib = 0` used to omit the flag entirely, so a `prune=` line in an
    // existing bitcoin.conf could turn a full node into a pruned one (or the
    // other way around). Always state it.
    plan.args.push(format!("-prune={}", cfg.core.prune_mib));

    plan.notes.push(format!(
        "{} MiB total RAM, {} CPUs -> dbcache {} MiB, par {}, rpcthreads {} ({} network)",
        total,
        cpus,
        plan.dbcache_mib,
        plan.par,
        plan.rpcthreads,
        chain.name()
    ));
    Ok(plan)
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
        assert_eq!(plan.dbcache_mib, 0);
        assert!(plan.args.iter().any(|a| a.starts_with("-par=")));
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
}
