//! `audit recon <target>` — classify an input and, where possible, expand
//! into a richer structure:
//!   * on-chain address → [`basilisk_onchain::ResolvedSystem`]
//!   * local path → [`basilisk_project::ResolvedProject`]
//!   * GitHub URL → clone via [`basilisk_git::RepoCache`], then render the
//!     working tree as a `ResolvedProject`. `--no-fetch` opts out of the
//!     clone and just prints the classifier output.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use alloy_primitives::Address;
use anyhow::{Context, Result};
use basilisk_core::{detect, Chain, Config, GitRef, Target};
use basilisk_git::{CloneStrategy, FetchOptions, RepoCache};
use basilisk_github::GithubClient;
use basilisk_onchain::{ExpansionLimits, OnchainIngester};
use basilisk_project::{resolve_project, ResolvedProject};
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

    /// For GitHub targets, skip the clone and just print the classifier.
    /// Useful for "what did this URL resolve to?" without any I/O.
    #[arg(long)]
    pub no_fetch: bool,

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

    match &target {
        Target::OnChain { address, chain } => {
            return resolve_onchain(*address, chain, config, args).await;
        }
        Target::LocalPath { root, .. } => {
            return resolve_local_path(root, args);
        }
        Target::Github {
            owner,
            repo,
            reference,
            subpath,
        } if !args.no_fetch => {
            return fetch_and_resolve_github(
                owner,
                repo,
                reference.clone(),
                subpath.as_deref(),
                args,
            )
            .await;
        }
        _ => {}
    }

    match args.output {
        OutputFormat::Pretty => print!("{target}"),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&target)?),
    }
    tracing::info!(target = ?target, "recon complete");
    Ok(())
}

async fn fetch_and_resolve_github(
    owner: &str,
    repo: &str,
    reference: Option<GitRef>,
    subpath: Option<&Path>,
    args: &ReconArgs,
) -> Result<()> {
    let cache = RepoCache::open().context("opening repo cache")?;
    let github = GithubClient::new(std::env::var("GITHUB_TOKEN").ok().as_deref())
        .context("initializing GitHub client")?;
    let options = FetchOptions {
        strategy: CloneStrategy::Shallow,
        force_refresh: args.no_cache,
        github: Some(github),
    };
    let fetched = cache
        .fetch(owner, repo, reference, options)
        .await
        .with_context(|| format!("fetching {owner}/{repo}"))?;
    tracing::info!(
        owner = owner,
        repo = repo,
        sha = %fetched.commit_sha,
        cached = fetched.cached,
        working_tree = %fetched.working_tree.display(),
        "fetched github target",
    );

    let project_root = match subpath {
        Some(sp) if !sp.as_os_str().is_empty() => fetched.working_tree.join(sp),
        _ => fetched.working_tree.clone(),
    };
    resolve_local_path(&project_root, args)
}

fn resolve_local_path(root: &std::path::Path, args: &ReconArgs) -> Result<()> {
    let project: ResolvedProject = resolve_project(root)
        .with_context(|| format!("resolving local project at {}", root.display()))?;

    match args.output {
        OutputFormat::Pretty => print!("{project}"),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&project)?),
    }
    let stats = project.stats();
    tracing::info!(
        root = %project.root.display(),
        kind = %project.config.layout.kind,
        sources = project.enumeration.sources().count(),
        tests = project.enumeration.tests().count(),
        resolved = stats.resolved_imports,
        unresolved = stats.unresolved_imports,
        "recon complete",
    );
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
