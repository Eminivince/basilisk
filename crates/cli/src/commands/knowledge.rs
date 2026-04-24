//! `audit knowledge` — manage the knowledge base.
//!
//! CP7.4 ships only the subcommand skeleton — the enum + a
//! functional `stats` + an `ingest <source>` stub that dispatches
//! to the right `basilisk-ingest::Ingester` once those land in
//! CP7.5–CP7.7. The remaining commands (`list-findings`,
//! `show-finding`, `correct`, `dismiss`, `confirm`, `search`,
//! `show`, `export`, `import`, `clear`, `reembed`, `add-protocol`,
//! `list-protocols`, `remove-protocol`) land in CP7.10 once
//! `basilisk-knowledge` (CP7.8) provides the API they call.
//!
//! The skeleton lands here (earlier than the original spec's
//! position at CP7.11) so CP7.5–CP7.7 ingester work can be
//! smoke-tested via `audit knowledge ingest <source>
//! --max-records 5` as each lands.

use std::sync::Arc;

use anyhow::{Context, Result};
use basilisk_core::Config;
use basilisk_ingest::{default_state_path, IngestState};
use basilisk_vector::{schema, MemoryVectorStore, VectorStore};
use clap::{Args, Subcommand};

#[derive(Debug, Subcommand)]
pub enum KnowledgeCmd {
    /// Show collection sizes, last-ingest timestamps, schema
    /// versions. The "what's in my knowledge base" command.
    Stats(StatsArgs),
    /// Ingest an external corpus. Concrete ingesters land in
    /// CP7.5–CP7.7.
    Ingest(IngestArgs),
}

#[derive(Debug, Args)]
pub struct StatsArgs {
    /// Path to the knowledge-base directory. Defaults to
    /// `~/.basilisk/knowledge`.
    #[arg(long)]
    pub knowledge_dir: Option<std::path::PathBuf>,
}

#[derive(Debug, Args)]
pub struct IngestArgs {
    /// Corpus to ingest: `solodit`, `swc`, `openzeppelin`, or
    /// `--all`. The `--all` shortcut runs every registered
    /// ingester in sequence; a failure in one does not stop the
    /// others.
    pub source: Option<String>,

    /// Run every ingester sequentially. Mutually exclusive with
    /// an explicit source argument.
    #[arg(long, conflicts_with = "source")]
    pub all: bool,

    /// Cap the number of records processed. Useful for smoke-
    /// testing as the ingesters land. `None` = unlimited.
    #[arg(long)]
    pub max_records: Option<usize>,

    /// When `false`, re-ingest every record instead of picking up
    /// from the last cursor. Defaults to `true`.
    #[arg(long, default_value_t = true)]
    pub incremental: bool,
}

pub async fn run(cmd: &KnowledgeCmd, _config: &Config) -> Result<()> {
    match cmd {
        KnowledgeCmd::Stats(args) => run_stats(args).await,
        KnowledgeCmd::Ingest(args) => run_ingest(args).await,
    }
}

async fn run_stats(_args: &StatsArgs) -> Result<()> {
    // CP7.4: in-memory store so `stats` runs cleanly on a fresh
    // machine without requiring LanceDB. CP7.3b replaces this
    // with a LanceDbStore wired to ~/.basilisk/knowledge/.
    let store: Arc<dyn VectorStore> = Arc::new(MemoryVectorStore::new());

    let collections = store
        .list_collections()
        .await
        .context("listing collections")?;

    if collections.is_empty() {
        // Fresh knowledge base — nothing created yet. Show the
        // shipped collection names so operators know what to expect
        // once ingestion runs.
        println!("knowledge base is empty");
        println!();
        println!("shipped collections (created on first ingest):");
        for name in schema::ALL_COLLECTIONS {
            println!("  - {name}");
        }
    } else {
        println!(
            "{:<20}  {:>8}  {:>6}  provider",
            "collection", "records", "dim",
        );
        for c in collections {
            println!(
                "{:<20}  {:>8}  {:>6}  {}",
                c.name, c.record_count, c.embedding_dim, c.embedding_provider,
            );
        }
    }

    // Incremental-ingest state summary.
    let state_path = default_state_path();
    let state = IngestState::load(&state_path).context("reading ingest state")?;
    println!();
    if state.sources.is_empty() {
        println!("no incremental ingest state recorded yet");
    } else {
        println!("ingest state ({}):", state_path.display());
        for (name, s) in &state.sources {
            let cursor = s.cursor.as_deref().unwrap_or("-");
            let last = s
                .last_run_unix
                .map_or_else(|| "-".into(), |t| t.to_string());
            println!(
                "  {name:<14} records={:>8}  cursor={cursor:<24}  last_run={last}",
                s.records_ingested,
            );
        }
    }

    Ok(())
}

// `async fn` kept despite no await — the real dispatch (CP7.5+)
// will call ingesters that are themselves async. Changing the
// shape now and again next commit is churn.
#[allow(clippy::unused_async)]
async fn run_ingest(args: &IngestArgs) -> Result<()> {
    // CP7.4 stub. The concrete dispatch lands commit-by-commit as
    // each ingester arrives (Solodit in CP7.5, SWC+OZ in CP7.6,
    // protocol-context in CP7.7). This stub simply reports which
    // ingester would run so the CLI + state-file machinery is
    // exercisable while the ETL is under development.
    if args.all {
        println!("would run all registered ingesters sequentially");
        println!("(none registered yet — CP7.5–CP7.7 will add Solodit / SWC / OpenZeppelin / protocol-context)");
        return Ok(());
    }
    let source = args
        .source
        .as_deref()
        .context("specify a source (solodit / swc / openzeppelin / protocol) or --all")?;
    println!(
        "would ingest source={source} incremental={} max_records={:?}",
        args.incremental, args.max_records,
    );
    println!("(no ingesters registered yet — CP7.5 lands Solodit first)");
    Ok(())
}
