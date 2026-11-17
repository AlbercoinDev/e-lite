//! cli module (split from main.rs in alpha.29.17.0).

use super::*;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Command {
    Help,
    Version,
    Init,
    Doctor,
    Index,
    Serve,
    Run,
    Status,
    ExportSnapshot,
    ImportSnapshot,
    ExportIdentity,
    ImportIdentity,
    Core,
    Onion,
    Cache,
    Privacy,
}

#[derive(Default)]
pub(crate) struct Args {
    pub(crate) command: Option<Command>,
    pub(crate) subcommand: Option<String>,
    pub(crate) config: Option<String>,
    pub(crate) public_servers: Option<String>,
    pub(crate) network: Option<String>,
    pub(crate) data_dir: Option<String>,
    pub(crate) rpc_url: Option<String>,
    pub(crate) rpc_user: Option<String>,
    pub(crate) rpc_password: Option<String>,
    pub(crate) core_datadir: Option<String>,
    pub(crate) cookie_file: Option<String>,
    pub(crate) listen: Option<String>,
    pub(crate) tls: Option<bool>,
    pub(crate) plain_lan: bool,
    pub(crate) tls_listen: Option<String>,
    pub(crate) metrics: Option<String>,
    pub(crate) allow_mainnet: bool,
    pub(crate) json: bool,
    pub(crate) no_follow: bool,
    pub(crate) manage: Option<String>,
    pub(crate) prune_mib: Option<u64>,
    pub(crate) allow_clearnet: bool,
    pub(crate) use_tor: Option<bool>,
    pub(crate) allow_auto_prune: bool,
    pub(crate) output: Option<String>,
    pub(crate) input: Option<String>,
    pub(crate) with_core: bool,
    pub(crate) no_hash_files: bool,
    pub(crate) compress_level: Option<u32>,
    pub(crate) verify: Option<String>,
    pub(crate) expect_digest: Option<String>,
    pub(crate) set: Vec<String>,
    pub(crate) allow_low_memory: bool,
    pub(crate) core_args: Vec<String>,
    pub(crate) no_tor: bool,
    pub(crate) purge: bool,
    pub(crate) yes: bool,
    pub(crate) dry_run: bool,
    pub(crate) log_level: Option<String>,
    pub(crate) no_log_file: bool,
    pub(crate) no_supervise: bool,
    /// One line per retired flag that was used, printed by the node.
    pub(crate) deprecations: Vec<String>,
}

/// How a retired flag becomes a `--set section.key=value`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Retired {
    /// A string value.
    Str,
    /// A non-negative integer.
    Int,
    /// An integer, at least 1 (the flag clamped it).
    IntMin1,
    Float,
    /// A comma-separated list of strings.
    List,
    /// A switch with this TOML value.
    Const(&'static str),
}

/// Flags replaced by `--set` (2026-11-17). Each one still works, as a `--set`
/// in the same position (a later explicit `--set` of the same key wins), and
/// prints one deprecation line. To be removed two releases after
/// 0.1.0-alpha.29.19.0 (ROADMAP).
pub(crate) const DEPRECATED_FLAGS: &[(&str, &str, Retired)] = &[
    ("--blocks-dir", "core.blocks_dir", Retired::Str),
    ("--block-source", "index.block_source", Retired::Str),
    ("--tls-cert", "server.tls_cert", Retired::Str),
    ("--tls-key", "server.tls_key", Retired::Str),
    ("--profile", "index.profile", Retired::Str),
    ("--commit-sync", "index.commit_sync", Retired::Str),
    ("--scheduler", "pruning.scheduler", Retired::Str),
    ("--banner-file", "server.banner_file", Retired::Str),
    ("--tor-socks", "rawtx.tor_socks", Retired::Str),
    ("--tor-mode", "tor.mode", Retired::Str),
    ("--tor-binary", "tor.binary", Retired::Str),
    ("--tor-target-plain", "tor.target_plain", Retired::Str),
    ("--tor-target-tls", "tor.target_tls", Retired::Str),
    ("--log-file", "logging.file", Retired::Str),
    ("--readers", "index.readers", Retired::Int),
    ("--bulk-readers", "index.readers", Retired::Int),
    ("--batch", "index.batch_blocks", Retired::IntMin1),
    ("--reorg-window", "index.reorg_window", Retired::IntMin1),
    ("--min-free-gib", "pruning.min_free_gib", Retired::IntMin1),
    ("--cache-mib", "index.cache_mib", Retired::Int),
    (
        "--commit-interval-ms",
        "index.commit_interval_ms",
        Retired::Int,
    ),
    ("--prefetch-blocks", "index.prefetch_blocks", Retired::Int),
    ("--write-buffer-mib", "index.write_buffer_mib", Retired::Int),
    (
        "--prefetch-deadline-ms",
        "index.prefetch_deadline_ms",
        Retired::Int,
    ),
    ("--max-open-files", "index.max_open_files", Retired::Int),
    (
        "--memory-reserve-mib",
        "index.memory_reserve_mib",
        Retired::Int,
    ),
    ("--memory-fraction", "index.memory_fraction", Retired::Float),
    ("--background-jobs", "index.background_jobs", Retired::Int),
    ("--milestone-blocks", "index.milestone_blocks", Retired::Int),
    (
        "--milestone-seconds",
        "index.milestone_seconds",
        Retired::Int,
    ),
    (
        "--max-core-lag-blocks",
        "pruning.max_core_lag_blocks",
        Retired::Int,
    ),
    (
        "--max-bulk-lag-blocks",
        "pruning.max_bulk_lag_blocks",
        Retired::Int,
    ),
    (
        "--max-bulk-lag-gib",
        "pruning.max_bulk_lag_gib",
        Retired::Int,
    ),
    (
        "--core-resume-ahead-blocks",
        "pruning.core_resume_ahead_blocks",
        Retired::Int,
    ),
    (
        "--core-resume-ahead-gib",
        "pruning.core_resume_ahead_gib",
        Retired::Int,
    ),
    (
        "--prune-defer-free-gib",
        "pruning.prune_defer_free_gib",
        Retired::Int,
    ),
    ("--min-gain-blocks", "pruning.min_gain_blocks", Retired::Int),
    (
        "--prune-interval-seconds",
        "pruning.prune_interval_seconds",
        Retired::Int,
    ),
    ("--poll-seconds", "pruning.poll_seconds", Retired::Int),
    (
        "--repair-max-gap-blocks",
        "pruning.repair_max_gap_blocks",
        Retired::Int,
    ),
    (
        "--critical-free-gib",
        "pruning.critical_free_gib",
        Retired::Int,
    ),
    ("--max-clients", "server.max_clients", Retired::Int),
    ("--core-io-priority", "core.io_priority", Retired::Int),
    (
        "--core-cgroup-io-weight",
        "core.cgroup_io_weight",
        Retired::Int,
    ),
    ("--decoys", "rawtx.decoys", Retired::Int),
    ("--rawtx-cache-mib", "rawtx.cache_mib", Retired::Int),
    ("--tor-onion-port", "tor.onion_port", Retired::Int),
    ("--tor-onion-tls-port", "tor.onion_tls_port", Retired::Int),
    ("--tor-socks-port", "tor.socks_port", Retired::Int),
    ("--log-max-mib", "logging.max_mib", Retired::Int),
    ("--p2p-peers", "rawtx.p2p_peers", Retired::List),
    ("--tls", "server.tls", Retired::Const("true")),
    (
        "--adaptive-memtables",
        "index.adaptive_memtables",
        Retired::Const("true"),
    ),
    (
        "--no-adaptive-memtables",
        "index.adaptive_memtables",
        Retired::Const("false"),
    ),
    (
        "--pause-core-during-bulk",
        "pruning.pause_core_during_bulk",
        Retired::Const("true"),
    ),
    (
        "--no-pause-core-during-bulk",
        "pruning.pause_core_during_bulk",
        Retired::Const("false"),
    ),
    ("--tor-plain", "tor.plain", Retired::Const("true")),
    ("--no-tor-plain", "tor.plain", Retired::Const("false")),
    ("--tor-tls", "tor.tls", Retired::Const("true")),
    ("--no-tor-tls", "tor.tls", Retired::Const("false")),
    (
        "--tor-auto-cert",
        "tor.tls_auto_cert",
        Retired::Const("true"),
    ),
    (
        "--no-tor-auto-cert",
        "tor.tls_auto_cert",
        Retired::Const("false"),
    ),
    ("--adaptive", "index.adaptive", Retired::Const("true")),
    ("--no-adaptive", "index.adaptive", Retired::Const("false")),
    ("--no-p2p", "rawtx.use_p2p", Retired::Const("false")),
    ("--no-esplora", "rawtx.use_esplora", Retired::Const("false")),
    (
        "--stop-on-exit",
        "core.stop_on_exit",
        Retired::Const("true"),
    ),
    (
        "--no-stop-on-exit",
        "core.stop_on_exit",
        Retired::Const("false"),
    ),
    ("--restart", "core.restart", Retired::Const("true")),
    ("--no-restart", "core.restart", Retired::Const("false")),
    ("--rawtx-tor", "rawtx.use_tor", Retired::Const("true")),
    ("--no-rawtx", "rawtx.enabled", Retired::Const("false")),
];

pub(crate) fn retired_flag(flag: &str) -> Option<(&'static str, Retired)> {
    DEPRECATED_FLAGS
        .iter()
        .find(|(name, _, _)| *name == flag)
        .map(|(_, key, kind)| (*key, *kind))
}

/// `section.key=value` for a retired flag, reading its value if it takes one.
pub(crate) fn retired_to_set(flag: &str, value: &mut impl FnMut(&str) -> R<String>) -> R<String> {
    let (key, kind) = retired_flag(flag).ok_or_else(|| format!("unknown flag: {flag}"))?;
    let rendered = match kind {
        Retired::Const(v) => v.to_string(),
        Retired::Str => toml::Value::String(value(flag)?).to_string(),
        Retired::Int => value(flag)?
            .trim()
            .parse::<u64>()
            .map_err(|e| format!("{flag}: {e}"))?
            .to_string(),
        Retired::IntMin1 => value(flag)?
            .trim()
            .parse::<u64>()
            .map_err(|e| format!("{flag}: {e}"))?
            .max(1)
            .to_string(),
        Retired::Float => value(flag)?
            .trim()
            .parse::<f64>()
            .map_err(|e| format!("{flag}: {e}"))?
            .to_string(),
        Retired::List => toml::Value::Array(
            value(flag)?
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| toml::Value::String(s.to_string()))
                .collect(),
        )
        .to_string(),
    };
    Ok(format!("{key}={rendered}"))
}

pub(crate) fn parse_args() -> R<Args> {
    parse_args_from(std::env::args().skip(1))
}

pub(crate) fn parse_args_from(argv: impl IntoIterator<Item = String>) -> R<Args> {
    let mut args = Args::default();
    let mut it = argv.into_iter();
    while let Some(raw) = it.next() {
        let (flag, inline) = match raw.split_once('=') {
            Some((f, v)) => (f.to_string(), Some(v.to_string())),
            None => (raw.clone(), None),
        };
        let mut value = |name: &str| -> R<String> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            it.next()
                .ok_or_else(|| format!("{name} needs a value").into())
        };
        match flag.as_str() {
            "-h" | "--help" => args.command = Some(Command::Help),
            "--version" | "-V" => args.command = Some(Command::Version),
            "--config" => args.config = Some(value("--config")?),
            "--public-servers" => args.public_servers = Some(value("--public-servers")?),
            "--network" => args.network = Some(value("--network")?),
            "--data-dir" => args.data_dir = Some(value("--data-dir")?),
            "--core-rpc" => args.rpc_url = Some(value("--core-rpc")?),
            "--core-datadir" => args.core_datadir = Some(value("--core-datadir")?),
            "--cookie-file" => args.cookie_file = Some(value("--cookie-file")?),
            "--rpc-user" => args.rpc_user = Some(value("--rpc-user")?),
            "--rpc-password" => args.rpc_password = Some(value("--rpc-password")?),
            "--listen" => args.listen = Some(value("--listen")?),
            "--tls-listen" => args.tls_listen = Some(value("--tls-listen")?),
            "--no-tls" => args.tls = Some(false),
            "--plain-lan" => args.plain_lan = true,
            "--metrics" => args.metrics = Some(value("--metrics")?),
            "--manage" => args.manage = Some(value("--manage")?),
            "--prune-mib" => args.prune_mib = Some(value("--prune-mib")?.parse()?),
            "--output" => args.output = Some(value("--output")?),
            "--input" => args.input = Some(value("--input")?),
            "--with-core" => args.with_core = true,
            // Hashing is the default (AUD-143); the old flag is accepted.
            "--hash-files" => args.no_hash_files = false,
            "--no-hash-files" => args.no_hash_files = true,
            "--compress-level" => args.compress_level = Some(value("--compress-level")?.parse()?),
            "--verify" => {
                let v = inline.clone().unwrap_or_else(|| "basic".to_string());
                if v != "basic" && v != "full" {
                    return fail(
                        "--verify takes `basic` (per-file hashes) or `full` \
                                 (hashes + a bounded verifychain on the restored Core)"
                            .to_string(),
                    );
                }
                args.verify = Some(v);
            }
            "--expect-digest" => args.expect_digest = Some(value("--expect-digest")?),
            "--set" => args.set.push(value("--set")?),
            "--allow-low-memory" => args.allow_low_memory = true,
            "--core-arg" => args.core_args.push(value("--core-arg")?),
            "--no-tor" => args.no_tor = true,
            "--purge" => args.purge = true,
            "--yes" | "-y" => args.yes = true,
            "--dry-run" => args.dry_run = true,
            "--no-log-file" => args.no_log_file = true,
            "--no-supervise" => args.no_supervise = true,
            "--log-level" => args.log_level = Some(value("--log-level")?),
            "--allow-mainnet" => args.allow_mainnet = true,
            "--allow-clearnet" => args.allow_clearnet = true,
            "--no-rawtx-tor" => args.use_tor = Some(false),
            "--allow-auto-prune" => args.allow_auto_prune = true,
            "--json" => args.json = true,
            "--no-follow" => args.no_follow = true,
            other if retired_flag(other).is_some() => {
                let item = retired_to_set(other, &mut value)?;
                args.deprecations.push(format!(
                    "[e-lite] WARNING: {other} is deprecated and will be removed: use --set {item}"
                ));
                args.set.push(item);
            }
            other if other.starts_with('-') => return fail(format!("unknown flag: {other}")),
            other => {
                if matches!(
                    args.command,
                    Some(Command::Core)
                        | Some(Command::Onion)
                        | Some(Command::Cache)
                        | Some(Command::Privacy)
                ) && args.subcommand.is_none()
                {
                    args.subcommand = Some(other.to_string());
                    continue;
                }
                if args.command.is_some() {
                    return fail(format!("unexpected argument: {other}"));
                }
                args.command = Some(match other {
                    "init" => Command::Init,
                    "doctor" => Command::Doctor,
                    "index" => Command::Index,
                    "serve" => Command::Serve,
                    "run" => Command::Run,
                    "status" => Command::Status,
                    "core" => Command::Core,
                    "onion" => Command::Onion,
                    "cache" => Command::Cache,
                    "privacy" => Command::Privacy,
                    "export-snapshot" => Command::ExportSnapshot,
                    "import-snapshot" => Command::ImportSnapshot,
                    "export-identity" => Command::ExportIdentity,
                    "import-identity" => Command::ImportIdentity,
                    "version" => Command::Version,
                    "help" => Command::Help,
                    _ => return fail(format!("unknown command: {other}")),
                });
            }
        }
    }
    Ok(args)
}

pub(crate) fn help_text() -> String {
    format!(
        r#"e-lite {VERSION} — Electrum 1.4-1.7 server for Bitcoin Core

USAGE:
    e-lite <command> [options]

COMMANDS:
    init             validate network/genesis against Core and create the database
    doctor           check config, Core, disk, ports and database integrity
    index            index the active chain (add --no-follow to stop at tip)
    serve            serve Electrum queries from an already indexed database
    run              index and serve in one process (recommended; waits for Core)
    status           print health (via local HTTP, or the database if stopped)
    core status      show the Core supervisor state and RPC reachability
    core logs        print the last 50 lines of a Core launched by e-lite
    onion show       print the Tor onion address and managed-Tor state
    onion reset      generate a new onion address (stop e-lite first)
    cache status     size and limits of the rawtx cache (the only on-disk cache)
    cache clear      delete the rawtx cache (needs --yes; never touches the index,
                     Core, TLS or Tor); restart e-lite to release RAM as well
    export-snapshot  print the canonical digest, or write a backup with --output
                     (--with-core bundles the network's Core chain data with a
                     SHA-256 per file; gzip: index stored, Core level 3,
                     --compress-level 0..9 sets both)
    import-snapshot  restore a backup with --input (target must be empty)
                     (--verify checks per-file hashes; --verify=full opens the
                     restored Core and runs a bounded verifychain;
                     --expect-digest <hex> requires the digest you recorded)
    export-identity  write the TLS identity, onion keys, decoy secret and the
                     configuration files to --output (private keys, not
                     encrypted; stop e-lite first)
    import-identity  restore them from --input (--yes replaces files that differ)
    version          print version

OPTIONS:
    --config <path>         TOML config file (default ~/.config/e-lite/config.toml)
    --network <name>        auto | regtest | signet | testnet4 | mainnet
    --data-dir <path>       index directory
    --set <key=value>       set any config field for this run, e.g.
                            --set index.readers=8 --set tor.mode=off
                            (values: true, 42, ["a","b"], or a bare string;
                            every key is in config.example.toml)
    --public-servers <path> public-servers.toml (default: next to the config file)
    --log-level <level>     info | debug | trace (alpha default: debug)
    --json                  JSON output for status, privacy check, cache
    --help, --version

CORE:
    --core-rpc <url>        http://host:port (use an SSH/VPN tunnel for WAN)
    --core-datadir <path>   Core datadir (cookie is read from there)
    --cookie-file <path>    explicit Core cookie
    --rpc-user <user>       explicit Core RPC user (remote Core)
    --rpc-password <pass>   explicit Core RPC password (remote Core)
    --manage <mode>         external | auto | managed (default auto)
    --prune-mib <n>         Core prune mode: 1 manual (default), 0 full, >=550 auto (needs opt-in)
    --core-arg <arg>        extra bitcoind argument (repeatable, adds to [core] extra_args)

LISTENERS:
    --listen <addr>         Electrum plain listen (default 127.0.0.1:<network port>;
                            an explicit address also moves TLS to that host)
    --tls-listen <addr>     Electrum TLS listen (default <listen host>:<network TLS port>)
    --no-tls                disable Electrum TLS
    --plain-lan             offer the plain listener on 0.0.0.0 (TLS already is)
    --metrics <addr>        status/metrics listen (default 127.0.0.1:<network port>)

PRIVACY:
    --no-tor                no managed Tor and no onion service
    --no-rawtx-tor          fetch wallet data directly, no Tor (exposes your IP)
    --allow-clearnet        allow external rawtx over clearnet (privacy risk)

PROCESS:
    --no-supervise          run/serve/index as one process, without the
                            automatic restart after a fatal error
    --no-follow             stop when caught up (only for `index`)
    --no-log-file           disable the application log

EXPLICIT PERMISSIONS:
    --allow-mainnet         required for mainnet
    --allow-auto-prune      opt in to prune_mib >= 550 (automatic pruning)
    --allow-low-memory      skip the minimum-RAM check (unsupported)
    --yes, -y               onion reset, cache clear, import-identity: do not ask
    --dry-run               cache clear: report what would be removed and stop
    --purge                 onion reset: delete keys instead of keeping onion.prev

SNAPSHOTS AND IDENTITY:
    --output <path>         file written by export-snapshot / export-identity
    --input <path>          file read by import-snapshot / import-identity
    --with-core             export-snapshot: bundle the network's Core datadir
    --no-hash-files         export-snapshot: skip the SHA-256 of every Core file
    --compress-level <n>    export-snapshot gzip level for both sections (0..9;
                            default: index 0 = stored, Core 3)
    --verify [basic|full]   import-snapshot: hash check, or Core verifychain
    --expect-digest <hex>   import-snapshot: refuse unless the recomputed index
                            digest equals this value (from your export output)

Everything else (tuning, pruning, rawtx, Tor ports, logging) is a config key:
put it in config.toml, or pass --set for one run. Older flags such as
--readers still work for now and print their replacement (docs/CLI.md).
"#
    )
}

pub(crate) fn print_help() {
    print!("{}", help_text());
}
