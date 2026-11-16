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
    Core,
    Onion,
    Cache,
    Privacy,
    ArcSetup,
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
    pub(crate) blocks_dir: Option<String>,
    pub(crate) cookie_file: Option<String>,
    pub(crate) listen: Option<String>,
    pub(crate) tls: Option<bool>,
    pub(crate) plain_lan: bool,
    pub(crate) tls_listen: Option<String>,
    pub(crate) tls_cert: Option<String>,
    pub(crate) tls_key: Option<String>,
    pub(crate) metrics: Option<String>,
    pub(crate) allow_mainnet: bool,
    pub(crate) json: bool,
    pub(crate) no_follow: bool,
    pub(crate) batch: Option<u32>,
    pub(crate) reorg_window: Option<u32>,
    pub(crate) block_source: Option<String>,
    pub(crate) readers: Option<usize>,
    pub(crate) manage: Option<String>,
    pub(crate) prune_mib: Option<u64>,
    pub(crate) min_free_gib: Option<u64>,
    pub(crate) allow_clearnet: bool,
    pub(crate) use_tor: Option<bool>,
    pub(crate) allow_auto_prune: bool,
    pub(crate) no_rawtx: bool,
    pub(crate) output: Option<String>,
    pub(crate) input: Option<String>,
    pub(crate) with_core: bool,
    pub(crate) no_hash_files: bool,
    pub(crate) compress_level: Option<u32>,
    pub(crate) verify: Option<String>,
    pub(crate) expect_digest: Option<String>,
    pub(crate) set: Vec<String>,
    pub(crate) profile: Option<String>,
    pub(crate) adaptive: Option<bool>,
    pub(crate) cache_mib: Option<u64>,
    pub(crate) commit_interval_ms: Option<u64>,
    pub(crate) commit_sync: Option<String>,
    pub(crate) prefetch_blocks: Option<usize>,
    pub(crate) write_buffer_mib: Option<u64>,
    pub(crate) adaptive_memtables: Option<bool>,
    pub(crate) prefetch_deadline_ms: Option<u64>,
    pub(crate) max_open_files: Option<u32>,
    pub(crate) memory_reserve_mib: Option<u64>,
    pub(crate) memory_fraction: Option<f64>,
    pub(crate) allow_low_memory: bool,
    /// `arc-setup --apply`: run the setup as root (writes the systemd unit).
    pub(crate) apply: bool,
    pub(crate) background_jobs: Option<usize>,
    pub(crate) milestone_blocks: Option<u64>,
    pub(crate) milestone_seconds: Option<u64>,
    pub(crate) max_core_lag_blocks: Option<u32>,
    pub(crate) max_bulk_lag_blocks: Option<u32>,
    pub(crate) max_bulk_lag_gib: Option<u64>,
    pub(crate) pause_core_during_bulk: Option<bool>,
    pub(crate) core_resume_ahead_blocks: Option<u32>,
    pub(crate) core_resume_ahead_gib: Option<u64>,
    pub(crate) scheduler: Option<String>,
    pub(crate) prune_defer_free_gib: Option<u64>,
    pub(crate) min_gain_blocks: Option<u32>,
    pub(crate) prune_interval_seconds: Option<u64>,
    pub(crate) poll_seconds: Option<u64>,
    pub(crate) repair_max_gap_blocks: Option<u32>,
    pub(crate) critical_free_gib: Option<u64>,
    pub(crate) max_clients: Option<usize>,
    pub(crate) banner_file: Option<String>,
    pub(crate) core_args: Vec<String>,
    pub(crate) core_io_priority: Option<u8>,
    pub(crate) core_cgroup_io_weight: Option<u64>,
    pub(crate) p2p_peers: Option<String>,
    pub(crate) no_p2p: bool,
    pub(crate) no_esplora: bool,
    pub(crate) decoys: Option<u32>,
    pub(crate) rawtx_cache_mib: Option<u64>,
    pub(crate) tor_socks: Option<String>,
    pub(crate) tor_mode: Option<String>,
    pub(crate) tor_plain: Option<bool>,
    pub(crate) tor_tls: Option<bool>,
    pub(crate) tor_auto_cert: Option<bool>,
    pub(crate) tor_onion_port: Option<u16>,
    pub(crate) tor_onion_tls_port: Option<u16>,
    pub(crate) tor_socks_port: Option<u16>,
    pub(crate) tor_binary: Option<String>,
    pub(crate) tor_target_plain: Option<String>,
    pub(crate) tor_target_tls: Option<String>,
    pub(crate) no_tor: bool,
    pub(crate) purge: bool,
    pub(crate) yes: bool,
    pub(crate) dry_run: bool,
    pub(crate) stop_on_exit: Option<bool>,
    pub(crate) restart: Option<bool>,
    pub(crate) log_file: Option<String>,
    pub(crate) log_max_mib: Option<u64>,
    pub(crate) log_level: Option<String>,
    pub(crate) no_log_file: bool,
    pub(crate) no_supervise: bool,
}

pub(crate) fn parse_args() -> R<Args> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
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
            "--blocks-dir" => args.blocks_dir = Some(value("--blocks-dir")?),
            "--block-source" => args.block_source = Some(value("--block-source")?),
            "--bulk-readers" | "--readers" => args.readers = Some(value("--readers")?.parse()?),
            "--cookie-file" => args.cookie_file = Some(value("--cookie-file")?),
            "--rpc-user" => args.rpc_user = Some(value("--rpc-user")?),
            "--rpc-password" => args.rpc_password = Some(value("--rpc-password")?),
            "--listen" => args.listen = Some(value("--listen")?),
            "--tls-listen" => args.tls_listen = Some(value("--tls-listen")?),
            "--tls" => args.tls = Some(true),
            "--no-tls" => args.tls = Some(false),
            "--plain-lan" => args.plain_lan = true,
            "--tls-cert" => args.tls_cert = Some(value("--tls-cert")?),
            "--tls-key" => args.tls_key = Some(value("--tls-key")?),
            "--metrics" => args.metrics = Some(value("--metrics")?),
            "--batch" => args.batch = Some(value("--batch")?.parse()?),
            "--reorg-window" => args.reorg_window = Some(value("--reorg-window")?.parse()?),
            "--manage" => args.manage = Some(value("--manage")?),
            "--prune-mib" => args.prune_mib = Some(value("--prune-mib")?.parse()?),
            "--min-free-gib" => args.min_free_gib = Some(value("--min-free-gib")?.parse()?),
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
            "--profile" => args.profile = Some(value("--profile")?),
            "--cache-mib" => args.cache_mib = Some(value("--cache-mib")?.parse()?),
            "--commit-interval-ms" => {
                args.commit_interval_ms = Some(value("--commit-interval-ms")?.parse()?)
            }
            "--commit-sync" => args.commit_sync = Some(value("--commit-sync")?),
            "--prefetch-blocks" => {
                args.prefetch_blocks = Some(value("--prefetch-blocks")?.parse()?)
            }
            "--write-buffer-mib" => {
                args.write_buffer_mib = Some(value("--write-buffer-mib")?.parse()?)
            }
            "--adaptive-memtables" => args.adaptive_memtables = Some(true),
            "--no-adaptive-memtables" => args.adaptive_memtables = Some(false),
            "--prefetch-deadline-ms" => {
                args.prefetch_deadline_ms = Some(value("--prefetch-deadline-ms")?.parse()?)
            }
            "--max-open-files" => args.max_open_files = Some(value("--max-open-files")?.parse()?),
            "--memory-reserve-mib" => {
                args.memory_reserve_mib = Some(value("--memory-reserve-mib")?.parse()?)
            }
            "--memory-fraction" => {
                args.memory_fraction = Some(value("--memory-fraction")?.parse()?)
            }
            "--allow-low-memory" => args.allow_low_memory = true,
            "--apply" => args.apply = true,
            "--background-jobs" => {
                args.background_jobs = Some(value("--background-jobs")?.parse()?)
            }
            "--milestone-blocks" => {
                args.milestone_blocks = Some(value("--milestone-blocks")?.parse()?)
            }
            "--milestone-seconds" => {
                args.milestone_seconds = Some(value("--milestone-seconds")?.parse()?)
            }
            "--max-core-lag-blocks" => {
                args.max_core_lag_blocks = Some(value("--max-core-lag-blocks")?.parse()?)
            }
            "--max-bulk-lag-blocks" => {
                args.max_bulk_lag_blocks = Some(value("--max-bulk-lag-blocks")?.parse()?)
            }
            "--max-bulk-lag-gib" => {
                args.max_bulk_lag_gib = Some(value("--max-bulk-lag-gib")?.parse()?)
            }
            "--pause-core-during-bulk" => args.pause_core_during_bulk = Some(true),
            "--no-pause-core-during-bulk" => args.pause_core_during_bulk = Some(false),
            "--core-resume-ahead-blocks" => {
                args.core_resume_ahead_blocks = Some(value("--core-resume-ahead-blocks")?.parse()?)
            }
            "--core-resume-ahead-gib" => {
                args.core_resume_ahead_gib = Some(value("--core-resume-ahead-gib")?.parse()?)
            }
            "--scheduler" => args.scheduler = Some(value("--scheduler")?),
            "--prune-defer-free-gib" => {
                args.prune_defer_free_gib = Some(value("--prune-defer-free-gib")?.parse()?)
            }
            "--min-gain-blocks" => {
                args.min_gain_blocks = Some(value("--min-gain-blocks")?.parse()?)
            }
            "--prune-interval-seconds" => {
                args.prune_interval_seconds = Some(value("--prune-interval-seconds")?.parse()?)
            }
            "--poll-seconds" => args.poll_seconds = Some(value("--poll-seconds")?.parse()?),
            "--repair-max-gap-blocks" => {
                args.repair_max_gap_blocks = Some(value("--repair-max-gap-blocks")?.parse()?)
            }
            "--critical-free-gib" => {
                args.critical_free_gib = Some(value("--critical-free-gib")?.parse()?)
            }
            "--max-clients" => args.max_clients = Some(value("--max-clients")?.parse()?),
            "--banner-file" => args.banner_file = Some(value("--banner-file")?),
            "--core-arg" => args.core_args.push(value("--core-arg")?),
            "--core-io-priority" => {
                args.core_io_priority = Some(value("--core-io-priority")?.parse()?)
            }
            "--core-cgroup-io-weight" => {
                args.core_cgroup_io_weight = Some(value("--core-cgroup-io-weight")?.parse()?)
            }
            "--p2p-peers" => args.p2p_peers = Some(value("--p2p-peers")?),
            "--decoys" => args.decoys = Some(value("--decoys")?.parse()?),
            "--rawtx-cache-mib" => {
                args.rawtx_cache_mib = Some(value("--rawtx-cache-mib")?.parse()?)
            }
            "--tor-socks" => args.tor_socks = Some(value("--tor-socks")?),
            "--tor-mode" => args.tor_mode = Some(value("--tor-mode")?),
            "--tor-onion-port" => args.tor_onion_port = Some(value("--tor-onion-port")?.parse()?),
            "--tor-onion-tls-port" => {
                args.tor_onion_tls_port = Some(value("--tor-onion-tls-port")?.parse()?)
            }
            "--tor-socks-port" => args.tor_socks_port = Some(value("--tor-socks-port")?.parse()?),
            "--tor-binary" => args.tor_binary = Some(value("--tor-binary")?),
            "--tor-target-plain" => args.tor_target_plain = Some(value("--tor-target-plain")?),
            "--tor-target-tls" => args.tor_target_tls = Some(value("--tor-target-tls")?),
            "--tor-plain" => args.tor_plain = Some(true),
            "--no-tor-plain" => args.tor_plain = Some(false),
            "--tor-tls" => args.tor_tls = Some(true),
            "--no-tor-tls" => args.tor_tls = Some(false),
            "--tor-auto-cert" => args.tor_auto_cert = Some(true),
            "--no-tor-auto-cert" => args.tor_auto_cert = Some(false),
            "--no-tor" => args.no_tor = true,
            "--purge" => args.purge = true,
            "--yes" | "-y" => args.yes = true,
            "--dry-run" => args.dry_run = true,
            "--adaptive" => args.adaptive = Some(true),
            "--no-adaptive" => args.adaptive = Some(false),
            "--no-p2p" => args.no_p2p = true,
            "--no-esplora" => args.no_esplora = true,
            "--log-file" => args.log_file = Some(value("--log-file")?),
            "--log-max-mib" => args.log_max_mib = Some(value("--log-max-mib")?.parse()?),
            "--no-log-file" => args.no_log_file = true,
            "--no-supervise" => args.no_supervise = true,
            "--log-level" => args.log_level = Some(value("--log-level")?),
            "--stop-on-exit" => args.stop_on_exit = Some(true),
            "--no-stop-on-exit" => args.stop_on_exit = Some(false),
            "--restart" => args.restart = Some(true),
            "--no-restart" => args.restart = Some(false),
            "--allow-mainnet" => args.allow_mainnet = true,
            "--allow-clearnet" => args.allow_clearnet = true,
            "--rawtx-tor" => args.use_tor = Some(true),
            "--no-rawtx-tor" => args.use_tor = Some(false),
            "--allow-auto-prune" => args.allow_auto_prune = true,
            "--no-rawtx" => args.no_rawtx = true,
            "--json" => args.json = true,
            "--no-follow" => args.no_follow = true,
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
                    "arc-setup" => Command::ArcSetup,
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
    arc-setup        let e-lite manage the ZFS ARC cap without root
                     (prints the chgrp/chmod + systemd unit; --apply as root)
    version          print version

OPTIONS:
    --config <path>         TOML config file (default ~/.config/e-lite/config.toml)
    --public-servers <path> public-servers.toml (default: next to the config file)
    --network <name>        auto | regtest | signet | testnet4 | mainnet
    --data-dir <path>       index directory
    --core-rpc <url>        http://host:port (use an SSH/VPN tunnel for WAN)
    --rpc-user <user>       explicit Core RPC user (remote Core)
    --rpc-password <pass>   explicit Core RPC password (remote Core)
    --core-datadir <path>   Core datadir (cookie is read from there)
    --blocks-dir <path>     Core blocks directory (default <datadir>/blocks)
    --block-source <src>    auto | rpc | files (default auto)
    --readers <n>           parallel fetch workers (0 = auto)
    --cookie-file <path>    explicit Core cookie
    --manage <mode>         external | auto | managed (default auto)
    --prune-mib <n>         Core prune mode: 1 manual (default), 0 full, >=550 auto (needs opt-in)
    --min-free-gib <n>      pause Core downloads below this free space
    --listen <addr>         Electrum plain listen (default 127.0.0.1:<network port>;
                            an explicit address also moves TLS to that host)
    --plain-lan             offer the plain listener on 0.0.0.0 (TLS already is)
    --tls / --no-tls        enable (default) or disable Electrum TLS
    --tls-listen <addr>     Electrum TLS listen (default <listen host>:<network TLS port>)
    --tls-cert <path>       PEM certificate chain (default: generated self-signed)
    --tls-key <path>        PEM private key (default: generated with the certificate)
    --metrics <addr>        status/metrics listen (default 127.0.0.1:<network port>)
    --batch <n>             blocks per atomic commit (default 256)
    --reorg-window <n>      undo window in blocks (default 288)
    --allow-mainnet         required for mainnet
    --allow-clearnet        allow external rawtx over clearnet (privacy risk)
    --no-rawtx-tor          fetch wallet data directly, no Tor (exposes your IP)
    --rawtx-tor             fetch wallet data through Tor (default)
    --no-rawtx              disable external rawtx sources
    --no-follow             stop when caught up (only for `index`)
    --json                  JSON output for status
    --output <path>         snapshot file for export-snapshot
    --input <path>          snapshot file for import-snapshot
    --with-core             export-snapshot: bundle the network's Core datadir
    --no-hash-files         export-snapshot: skip the SHA-256 of every Core file
    --compress-level <n>    export-snapshot gzip level for both sections (0..9;
                            default: index 0 = stored, Core 3)
    --verify [basic|full]   import-snapshot: hash check, or Core verifychain
    --expect-digest <hex>   import-snapshot: refuse unless the recomputed index
                            digest equals this value (from your export output)
    --set <key=value>       override any TOML field, e.g. index.profile=ssd
                            (values: true, 42, ["a","b"], or a bare string)

TUNING (all networks):
    --profile <name>        auto | hdd | ssd | nvme | manual
    --adaptive              enable the controller (default)
    --no-adaptive           freeze the configured/derived values
    --cache-mib <n>         RocksDB block cache (0 = profile/RAM)
    --write-buffer-mib <n>  total RocksDB memtable budget (0 = effective RAM/16, 512..2048)
    --memory-reserve-mib <n> RAM left for OS/Core/page cache (0 = effective RAM/8, 2048..4096)
    --memory-fraction <f>   e-lite ceiling as a share of total RAM (0 = 0.75)
    --allow-low-memory      skip the minimum-RAM check (unsupported)
    --background-jobs <n>   RocksDB jobs (0 = profile, capped by CPUs)
    --commit-interval-ms <n> commit/fsync interval (0 = profile; 500..30000)
    --commit-sync <mode>    auto | always | txids (bulk durability tradeoff)
    --prefetch-blocks <n>   prepared-block window cap (0 = auto; >0 caps)
    --prefetch-deadline-ms <n> stop prefetching a block after n ms (0 = off)
    --max-open-files <n>        RocksDB fds for table readers (0 = 8192)
    --adaptive-memtables    let the controller resize memtables at runtime
    --no-adaptive-memtables keep the startup memtable budget (default)
    --milestone-blocks <n>  progress report every N blocks (0 = off)
    --milestone-seconds <n> progress report every N seconds (0 = off)

PRUNING AND CORE:
    --max-core-lag-blocks <n>   auto-prune download limit (0 = off)
    --max-bulk-lag-blocks <n>   legacy block growth limit (used if GiB is 0)
    --max-bulk-lag-gib <n>      pause Core above this unindexed backlog (0 = off)
    --pause-core-during-bulk    pause Core during a long catch-up (default)
    --no-pause-core-during-bulk disable the full bulk pause
    --core-resume-ahead-blocks <n> legacy resume margin in blocks
    --core-resume-ahead-gib <n> resume Core below this backlog (default 7)
    --scheduler <mode>          overlap (default) | alternate (opt-in test)
    --prune-defer-free-gib <n>  skip normal prune above this free space (default 20)
    --min-gain-blocks <n>       minimum safe-height gain between prunes (default 5000)
    --prune-interval-seconds <n> minimum time between prunes
    --poll-seconds <n>          supervisor poll interval
    --repair-max-gap-blocks <n> largest external prune-gap repair
    --critical-free-gib <n>     pause indexing below this free space
    --allow-auto-prune          opt in to prune_mib >= 550 (automatic pruning)
    --core-arg <arg>            extra bitcoind argument (repeatable)
    --core-io-priority <0-7>    best-effort ionice for managed Core (opt-in)
    --core-cgroup-io-weight <n> best-effort cgroup v2 io.weight (opt-in)
    --stop-on-exit / --no-stop-on-exit
    --restart / --no-restart

RAWTX, SERVER AND TLS:
    --max-clients <n>       Electrum client limit
    --banner-file <path>    wallet banner template (created if missing)
    --p2p-peers <a,b>       comma-separated P2P peers for rawtx/repair
    --no-p2p                disable the P2P rawtx/repair source
    --no-esplora            disable the Esplora rawtx/repair source
    --decoys <n>            decoy wallets per wallet load (0 disables)
    --rawtx-cache-mib <n>   validated rawtx cache size (0 = disabled)
    --tor-socks <addr>      SOCKS5h proxy for rawtx (see --no-rawtx-tor for direct)

TOR / ONION:
    --tor-mode <mode>       managed | off (default managed)
    --no-tor                same as --tor-mode off
    --tor-plain / --no-tor-plain    publish the plain Electrum port on the onion
    --tor-tls / --no-tor-tls        publish the TLS Electrum port on the onion
    --tor-auto-cert / --no-tor-auto-cert
                            self-signed TLS certificate when none is configured
    --tor-onion-port <n>    onion virtual port for plain (0 = same as listen)
    --tor-onion-tls-port <n> onion virtual port for TLS (0 = same as tls-listen)
    --tor-socks-port <n>    managed Tor SOCKS port (0 = auto)
    --tor-binary <path>     Tor binary (empty = installer bundle, then PATH)
    --tor-target-plain <addr> loopback target for the plain onion port
    --tor-target-tls <addr> loopback target for the TLS onion port
    --purge                 onion reset: delete keys instead of keeping onion.prev
    --yes, -y               onion reset and cache clear: do not ask for confirmation
    --dry-run               cache clear: report what would be removed and stop

LOGGING:
    --log-file <path>       application log (default <data-dir>/e-lite.log)
    --log-max-mib <n>       rotate to e-lite.log.1 above N MiB (0 = never)
    --log-level <level>     info | debug | trace (alpha default: debug;
                            extra diagnostic lines are gated by this level)
    --no-log-file           disable the application log
    --no-supervise          run/serve/index as one process, without the
                            automatic restart of e-lite after a fatal error
"#
    )
}

pub(crate) fn print_help() {
    print!("{}", help_text());
}
