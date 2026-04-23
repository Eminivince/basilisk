//! `audit recon <target>` — classify an input into a structured [`Target`].
//!
//! Pure local detection; no network or RPC calls. Unclassifiable inputs
//! still produce a successful exit (0) — the classification just resolves
//! to `Target::Unknown` with a reason.

use anyhow::Result;
use basilisk_core::{detect, Chain, Config};
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

    /// Output format for the classified target.
    #[arg(long, value_enum, default_value_t = OutputFormat::Pretty)]
    pub output: OutputFormat,
}

fn parse_chain_arg(s: &str) -> std::result::Result<Chain, String> {
    s.parse::<Chain>().map_err(|e| e.to_string())
}

pub fn run(args: &ReconArgs, _config: &Config) -> Result<()> {
    let target = detect(&args.target, args.chain.clone());
    match args.output {
        OutputFormat::Pretty => print!("{target}"),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&target)?),
    }
    tracing::info!(target = ?target, "recon complete");
    Ok(())
}
