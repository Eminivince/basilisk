//! `audit` — the Basilisk command-line entry point.

mod commands;

use anyhow::{Context, Result};
use basilisk_core::Config;
use basilisk_logging::LogFormat;
use clap::{Parser, Subcommand};

use crate::commands::{agent::AgentArgs, cache::CacheArgs, recon::ReconArgs};

#[derive(Debug, Parser)]
#[command(
    name = "audit",
    version,
    about = "Basilisk — AI-driven smart-contract auditor",
    long_about = None,
)]
struct Cli {
    /// Emit logs as JSON. When unset, defaults to pretty on a TTY and JSON otherwise.
    #[arg(long, global = true)]
    json_logs: bool,

    /// Force pretty (human-readable) logs, overriding the TTY default.
    #[arg(long, global = true, conflicts_with = "json_logs")]
    pretty_logs: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Classify a target (GitHub repo, on-chain address, local path) and, for
    /// on-chain targets, fetch bytecode + verified source + proxy info.
    Recon(ReconArgs),
    /// Run the LLM-driven auditor agent against a target.
    Agent(AgentArgs),
    /// Inspect and manage Basilisk's on-disk cache.
    Cache(CacheArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let config = Config::load().context("loading configuration")?;

    let format = if cli.json_logs {
        Some(LogFormat::Json)
    } else if cli.pretty_logs {
        Some(LogFormat::Pretty)
    } else {
        None
    };
    basilisk_logging::init(format, &config.log_level)
        .map_err(|e| anyhow::anyhow!("failed to initialize logging: {e}"))?;

    match &cli.command {
        Command::Recon(args) => commands::recon::run(args, &config).await,
        Command::Agent(args) => commands::agent::run(args, &config).await,
        Command::Cache(args) => commands::cache::run(args).await,
    }
}
