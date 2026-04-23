//! `audit recon <target>` — classify an input and, for on-chain targets,
//! fetch bytecode + verified source + proxy info.

use std::time::Duration;

use alloy_primitives::Address;
use anyhow::{Context, Result};
use basilisk_core::{detect, Chain, Config, Target};
use basilisk_onchain::OnchainIngester;
use clap::{Args, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable multi-line summary (default).
    Pretty,
    /// Pretty-printed JSON.
    Json,
}

#[derive(Debug, Args)]
pub struct ReconArgs {
    /// Target to reconnaissance: a URL, address, path, or free-form string.
    pub target: String,

    /// Chain hint applied when the target resolves to an on-chain address.
    /// Accepts canonical names, aliases (eth, arb, op, ...), or chain IDs.
    #[arg(long, value_parser = parse_chain_arg)]
    pub chain: Option<Chain>,

    /// Output format for the classified / resolved target.
    #[arg(long, value_enum, default_value_t = OutputFormat::Pretty)]
    pub output: OutputFormat,

    /// Bypass all on-disk caches for this run. Successful lookups still
    /// write so subsequent runs can reuse the result.
    #[arg(long)]
    pub no_cache: bool,

    /// Override the overall on-chain resolution timeout (seconds).
    /// Default: `config.onchain_timeout_secs` (60 unless configured).
    #[arg(long)]
    pub timeout: Option<u64>,
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
    let resolved = ingester
        .resolve(address)
        .await
        .context("resolving on-chain contract")?;
    match args.output {
        OutputFormat::Pretty => print!("{resolved}"),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&resolved)?),
    }
    tracing::info!(
        address = %address,
        chain = chain.canonical_name(),
        verified = resolved.source.is_some(),
        is_proxy = resolved.proxy.is_some(),
        "recon complete",
    );
    Ok(())
}
