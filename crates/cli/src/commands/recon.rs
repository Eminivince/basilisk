//! `audit recon <target>` — classify an input and, for on-chain targets,
//! expand into a full [`basilisk_onchain::ResolvedSystem`].

use std::{path::PathBuf, time::Duration};

use alloy_primitives::Address;
use anyhow::{Context, Result};
use basilisk_core::{detect, Chain, Config, Target};
use basilisk_onchain::{ExpansionLimits, OnchainIngester};
use clap::{Args, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable multi-line summary (default).
    Pretty,
    /// Pretty-printed JSON.
    Json,
}

#[allow(clippy::struct_excessive_bools)] // Every bool is an independent expansion toggle.
#[derive(Debug, Args)]
pub struct ReconArgs {
    /// Target to reconnaissance: a URL, address, path, or free-form string.
    pub target: String,

    /// Chain hint applied when the target resolves to an on-chain address.
    #[arg(long, value_parser = parse_chain_arg)]
    pub chain: Option<Chain>,

    /// Output format for the classified / resolved target.
    #[arg(long, value_enum, default_value_t = OutputFormat::Pretty)]
    pub output: OutputFormat,

    /// Bypass all on-disk caches for this run. Writes still land.
    #[arg(long)]
    pub no_cache: bool,

    /// Override the per-contract timeout (seconds).
    #[arg(long)]
    pub timeout: Option<u64>,

    // ---- system-expansion flags ----
    /// Maximum BFS depth from root.
    #[arg(long)]
    pub max_depth: Option<usize>,
    /// Maximum total contracts to resolve.
    #[arg(long)]
    pub max_contracts: Option<usize>,
    /// Maximum total wall-clock (seconds) for system expansion.
    #[arg(long)]
    pub max_duration: Option<u64>,
    /// Disable the storage-slot scanner.
    #[arg(long)]
    pub no_expand_storage: bool,
    /// Disable the bytecode PUSH20 scanner.
    #[arg(long)]
    pub no_expand_bytecode: bool,
    /// Disable the verified-source immutable/constant extractor.
    #[arg(long)]
    pub no_expand_immutables: bool,
    /// Skip upgrade-history log walks.
    #[arg(long)]
    pub no_history: bool,
    /// Skip constructor-args recovery.
    #[arg(long)]
    pub no_constructor_args: bool,
    /// Skip storage-layout recovery (currently stubbed anyway).
    #[arg(long)]
    pub skip_storage_layout: bool,
    /// How many storage slots to scan per contract.
    #[arg(long)]
    pub storage_scan_depth: Option<usize>,
    /// Start history scans at this block.
    #[arg(long)]
    pub history_from_block: Option<u64>,
    /// Parallelism for system expansion (currently advisory — see deviation).
    #[arg(long)]
    pub parallelism: Option<usize>,
    /// Write the contract graph as `GraphViz` DOT to this path.
    #[arg(long)]
    pub dot: Option<PathBuf>,
}

fn parse_chain_arg(s: &str) -> std::result::Result<Chain, String> {
    s.parse::<Chain>().map_err(|e| e.to_string())
}

pub async fn run(args: &ReconArgs, config: &Config) -> Result<()> {
    let target = detect(&args.target, args.chain.clone());

    if let Target::OnChain { address, chain } = &target {
        return resolve_onchain(*address, chain, config, args).await;
    }

    match args.output {
        OutputFormat::Pretty => print!("{target}"),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&target)?),
    }
    tracing::info!(target = ?target, "recon complete");
    Ok(())
}

fn build_limits(args: &ReconArgs) -> ExpansionLimits {
    let mut l = ExpansionLimits::default();
    if let Some(v) = args.max_depth {
        l.max_depth = v;
    }
    if let Some(v) = args.max_contracts {
        l.max_contracts = v;
    }
    if let Some(v) = args.max_duration {
        l.max_duration = Duration::from_secs(v);
    }
    if args.no_expand_storage {
        l.expand_storage = false;
    }
    if args.no_expand_bytecode {
        l.expand_bytecode = false;
    }
    if args.no_expand_immutables {
        l.expand_immutables = false;
    }
    if args.no_history {
        l.fetch_history = false;
    }
    if args.no_constructor_args {
        l.fetch_constructor_args = false;
    }
    if args.skip_storage_layout {
        l.fetch_storage_layout = false;
    }
    if let Some(v) = args.storage_scan_depth {
        l.storage_scan_depth = v;
    }
    if let Some(v) = args.history_from_block {
        l.history_from_block = v;
    }
    if let Some(v) = args.parallelism {
        l.parallelism = v;
    }
    l
}

async fn resolve_onchain(
    address: Address,
    chain: &Chain,
    config: &Config,
    args: &ReconArgs,
) -> Result<()> {
    let mut ingester = if args.no_cache {
        OnchainIngester::new_uncached(chain, config).context("initializing on-chain ingester")?
    } else {
        OnchainIngester::new(chain, config).context("initializing on-chain ingester")?
    };
    if let Some(t) = args.timeout {
        ingester = ingester.with_timeout(Duration::from_secs(t));
    }
    let limits = build_limits(args);
    let system = ingester
        .resolve_system(address, limits)
        .await
        .context("resolving on-chain system")?;

    if let Some(path) = &args.dot {
        let dot = system.graph.to_dot();
        std::fs::write(path, dot).with_context(|| format!("writing {}", path.display()))?;
        eprintln!("wrote graph → {}", path.display());
    }

    match args.output {
        OutputFormat::Pretty => print!("{system}"),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&system)?),
    }
    tracing::info!(
        address = %address,
        chain = chain.canonical_name(),
        contracts = system.stats.contracts_resolved,
        edges = system.graph.edge_count(),
        "recon complete",
    );
    Ok(())
}
