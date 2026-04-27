//! `audit` — the Basilisk command-line entry point.

mod commands;

use anyhow::{Context, Result};
use basilisk_core::Config;
use basilisk_logging::LogFormat;
use clap::{Parser, Subcommand};

use crate::commands::{
    bench::BenchCmd, cache::CacheArgs, doc::DocArgs, knowledge::KnowledgeCmd, recon::ReconArgs,
    session::SessionCmd,
};

#[derive(Debug, Parser)]
#[command(
    name = "basilisk",
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
    /// Audit a target (GitHub repo, on-chain address, local path) via the
    /// LLM-driven auditor. Defaults to recon-mode; pass `--vuln` to switch
    /// to vulnerability-hunting mode (see `--help` on `recon` for the
    /// full flag list, including `--model`, `--provider`, budget caps,
    /// and the `--vuln` mode-selector).
    Recon(ReconArgs),
    /// Inspect, resume, and delete agent sessions persisted on disk.
    #[command(subcommand)]
    Session(SessionCmd),
    /// Manage the knowledge base — ingest corpora, search findings,
    /// add protocol docs, correct / dismiss / confirm entries.
    #[command(subcommand)]
    Knowledge(KnowledgeCmd),
    /// Inspect and manage Basilisk's on-disk cache.
    Cache(CacheArgs),
    /// Set 9 benchmark harness — list/show/run the five calibration
    /// targets and review scored runs.
    #[command(subcommand)]
    Bench(BenchCmd),
    /// Serve the Basilisk documentation on localhost (default port 3000).
    Doc(DocArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env before clap parses, so `#[arg(env = "...")]` directives
    // on agent flags (--provider, --model, …) pick up values from the
    // repo's .env file. Config::load() below is idempotent w.r.t.
    // dotenv — it's safe to invoke a second time.
    let _ = dotenvy::dotenv();

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

    // Set 9.5 / CP9.5.5 — install signal handlers that shut down
    // outstanding forks before re-raising the signal. Spawned in
    // the background so the main task continues normally; on signal
    // we iterate the global registry and call shutdown() on each
    // live fork. A leaked anvil process is an incident we can avoid
    // with this hook.
    install_signal_handlers();

    match &cli.command {
        Command::Recon(args) => commands::recon::run(args, &config).await,
        Command::Session(cmd) => commands::session::run(cmd, &config).await,
        Command::Knowledge(cmd) => commands::knowledge::run(cmd, &config).await,
        Command::Cache(args) => commands::cache::run(args).await,
        Command::Bench(cmd) => commands::bench::run(cmd, &config).await,
        Command::Doc(args) => commands::doc::run(args).await,
    }
}

/// Spawn signal handlers (Ctrl-C + SIGTERM on Unix). On signal,
/// enumerate the global fork registry and shut each live fork down
/// before terminating the process with the conventional 128+signal
/// exit code. Best-effort: errors during shutdown are logged but
/// don't block termination.
fn install_signal_handlers() {
    tokio::spawn(async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %e, "ctrl_c handler errored — fork cleanup skipped");
            return;
        }
        eprintln!("\nreceived SIGINT — shutting down outstanding forks");
        let n = basilisk_exec::GLOBAL_FORK_REGISTRY.shutdown_all().await;
        eprintln!("  cleaned up {n} fork(s)");
        std::process::exit(130);
    });

    #[cfg(unix)]
    tokio::spawn(async {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "SIGTERM handler errored — fork cleanup skipped");
                return;
            }
        };
        if term.recv().await.is_some() {
            eprintln!("\nreceived SIGTERM — shutting down outstanding forks");
            let n = basilisk_exec::GLOBAL_FORK_REGISTRY.shutdown_all().await;
            eprintln!("  cleaned up {n} fork(s)");
            std::process::exit(143);
        }
    });
}
