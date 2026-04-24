//! `audit knowledge` — manage the knowledge base.
//!
//! Commands shipped in CP7.10:
//!
//!  - `stats` / `ingest <source>` / `ingest --all`
//!  - `add-protocol <engagement-id> --url|--pdf|--file|--github`
//!  - `list-findings [--session|--severity]` / `show-finding <id>`
//!  - `correct <id> --reason <text>` / `dismiss <id>` / `confirm <id>`
//!  - `search <query>` / `search-code <path>`
//!  - `clear <collection> [--yes]` / `clear --all [--yes]`
//!
//! Persistence uses [`basilisk_vector::FileVectorStore`] at
//! `~/.basilisk/knowledge/store.json` — interim until the
//! LanceDB-backed store lands. Good for dogfooding and small-
//! operator corpora; the trait surface is identical so the swap
//! is transparent.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use basilisk_core::Config;
use basilisk_embeddings::{
    build_provider, EmbeddingProvider, ProviderKind as EmbedProviderKind, ProviderSelection,
};
use basilisk_git::RepoCache;
use basilisk_github::GithubClient;
use basilisk_ingest::{
    IngestOptions, IngestProgress, Ingester, OzAdvisoriesIngester, ProtocolIngester,
    ProtocolSource, SoloditIngester, SwcIngester,
};
use basilisk_knowledge::{Correction, FindingId, KnowledgeBase, SearchFilters, UserVerdict};
use basilisk_vector::{FileVectorStore, VectorStore};
use clap::{Args, Subcommand};

#[derive(Debug, Subcommand)]
pub enum KnowledgeCmd {
    /// Show collection sizes, last-ingest timestamps, schema versions.
    Stats(StatsArgs),
    /// Ingest an external corpus (or all of them).
    Ingest(IngestArgs),
    /// Attach documentation for a specific engagement.
    AddProtocol(AddProtocolArgs),
    /// List findings stored in `user_findings`.
    ListFindings(ListFindingsArgs),
    /// Show one finding in full.
    ShowFinding(ShowFindingArgs),
    /// Record a human correction against a finding.
    Correct(CorrectArgs),
    /// Mark a finding as a false positive.
    Dismiss(DismissArgs),
    /// Mark a finding as confirmed by human review.
    Confirm(ConfirmArgs),
    /// Natural-language search across collections.
    Search(SearchArgs),
    /// Drop a collection. Destructive; confirms unless `--yes`.
    Clear(ClearArgs),
}

#[derive(Debug, Args)]
pub struct StatsArgs {
    #[arg(long)]
    pub knowledge_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct IngestArgs {
    /// Corpus name: `solodit`, `swc`, `openzeppelin`.
    pub source: Option<String>,
    #[arg(long, conflicts_with = "source")]
    pub all: bool,
    #[arg(long)]
    pub max_records: Option<usize>,
    #[arg(long, default_value_t = true)]
    pub incremental: bool,
}

#[derive(Debug, Args)]
pub struct AddProtocolArgs {
    /// Engagement id — any string; used to scope retrieval later.
    pub engagement_id: String,
    #[arg(long, conflicts_with_all = ["pdf", "file", "github"])]
    pub url: Option<String>,
    #[arg(long, conflicts_with_all = ["url", "file", "github"])]
    pub pdf: Option<PathBuf>,
    #[arg(long, conflicts_with_all = ["url", "pdf", "github"])]
    pub file: Option<PathBuf>,
    /// `owner/repo[:subdir]` form. Example: `OpenZeppelin/openzeppelin-contracts:docs`.
    #[arg(long, conflicts_with_all = ["url", "pdf", "file"])]
    pub github: Option<String>,
}

#[derive(Debug, Args)]
pub struct ListFindingsArgs {
    #[arg(long)]
    pub session: Option<String>,
    #[arg(long)]
    pub severity: Option<String>,
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
}

#[derive(Debug, Args)]
pub struct ShowFindingArgs {
    pub id: String,
}

#[derive(Debug, Args)]
pub struct CorrectArgs {
    pub id: String,
    #[arg(long)]
    pub reason: String,
    #[arg(long)]
    pub severity: Option<String>,
    #[arg(long)]
    pub category: Option<String>,
}

#[derive(Debug, Args)]
pub struct DismissArgs {
    pub id: String,
    #[arg(long)]
    pub reason: String,
}

#[derive(Debug, Args)]
pub struct ConfirmArgs {
    pub id: String,
}

#[derive(Debug, Args)]
pub struct SearchArgs {
    pub query: String,
    #[arg(long)]
    pub collection: Option<String>,
    #[arg(long)]
    pub kind: Option<String>,
    #[arg(long)]
    pub severity: Option<String>,
    #[arg(long, default_value_t = 10)]
    pub limit: usize,
}

#[derive(Debug, Args)]
pub struct ClearArgs {
    /// Collection name. Mutually exclusive with `--all`.
    pub collection: Option<String>,
    #[arg(long, conflicts_with = "collection")]
    pub all: bool,
    #[arg(long)]
    pub yes: bool,
}

pub async fn run(cmd: &KnowledgeCmd, config: &Config) -> Result<()> {
    match cmd {
        KnowledgeCmd::Stats(args) => run_stats(args, config).await,
        KnowledgeCmd::Ingest(args) => run_ingest(args, config).await,
        KnowledgeCmd::AddProtocol(args) => run_add_protocol(args, config).await,
        KnowledgeCmd::ListFindings(args) => run_list_findings(args, config).await,
        KnowledgeCmd::ShowFinding(args) => run_show_finding(args, config).await,
        KnowledgeCmd::Correct(args) => run_correct(args, config).await,
        KnowledgeCmd::Dismiss(args) => run_dismiss(args, config).await,
        KnowledgeCmd::Confirm(args) => run_confirm(args, config).await,
        KnowledgeCmd::Search(args) => run_search(args, config).await,
        KnowledgeCmd::Clear(args) => run_clear(args, config).await,
    }
}

/// Default knowledge base path: `~/.basilisk/knowledge/store.json`.
/// Every command resolves this unless an override is supplied.
fn default_store_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".basilisk")
        .join("knowledge")
        .join("store.json")
}

/// Build an embedding provider from Config + env. Centralised
/// here so every command picks the same provider.
fn build_embeddings(config: &Config) -> Result<Arc<dyn EmbeddingProvider>> {
    let selection = ProviderSelection {
        provider: config
            .embeddings_provider
            .as_deref()
            .and_then(EmbedProviderKind::parse),
        voyage_api_key: config.voyage_api_key.clone(),
        openai_api_key: config.openai_api_key.clone(),
        ollama_host: config.ollama_host.clone(),
        model: None,
        voyage_token_rate_per_minute: None,
    };
    build_provider(&selection).context("building embedding provider")
}

async fn open_store() -> Result<Arc<FileVectorStore>> {
    FileVectorStore::open(default_store_path())
        .await
        .context("opening knowledge store")
}

async fn open_kb(config: &Config) -> Result<KnowledgeBase> {
    let store = open_store().await?;
    let embeddings = build_embeddings(config)?;
    Ok(KnowledgeBase::new(
        store as Arc<dyn VectorStore>,
        embeddings,
    ))
}

// --- stats -----------------------------------------------------------

async fn run_stats(_args: &StatsArgs, config: &Config) -> Result<()> {
    let store = open_store().await?;
    // Embedding provider is informational for stats; if none is
    // available we still want to list collections.
    let embed_result = build_embeddings(config);

    let collections = store.list_collections().await?;
    if collections.is_empty() {
        println!(
            "knowledge base is empty at {}",
            default_store_path().display()
        );
        println!();
        println!("shipped collections (created on first ingest):");
        for n in basilisk_vector::schema::ALL_COLLECTIONS {
            println!("  - {n}");
        }
    } else {
        println!("store: {}", default_store_path().display());
        println!();
        println!(
            "{:<20}  {:>8}  {:>6}  provider",
            "collection", "records", "dim"
        );
        for c in collections {
            println!(
                "{:<20}  {:>8}  {:>6}  {}",
                c.name, c.record_count, c.embedding_dim, c.embedding_provider,
            );
        }
    }

    println!();
    match embed_result {
        Ok(p) => println!(
            "current embedding provider: {} (dim={})",
            p.identifier(),
            p.dimensions(),
        ),
        Err(e) => println!("embedding provider not configured: {e}"),
    }

    // Incremental-ingest state.
    let state_path = basilisk_ingest::default_state_path();
    let state = basilisk_ingest::IngestState::load(&state_path).unwrap_or_default();
    println!();
    if state.sources.is_empty() {
        println!("no incremental ingest state recorded yet");
    } else {
        println!("ingest state ({}):", state_path.display());
        for (name, s) in &state.sources {
            let last = s
                .last_run_unix
                .map_or_else(|| "-".into(), |t| t.to_string());
            let cursor = s.cursor.as_deref().unwrap_or("-");
            println!(
                "  {name:<20} records={:>8} cursor={cursor:<48} last={last}",
                s.records_ingested,
            );
        }
    }
    Ok(())
}

// --- ingest ----------------------------------------------------------

async fn run_ingest(args: &IngestArgs, config: &Config) -> Result<()> {
    let store = open_store().await?;
    let embeddings = build_embeddings(config)?;

    let ingesters = if args.all {
        available_ingesters(config)
    } else {
        let name = args
            .source
            .as_deref()
            .context("specify a source or --all (sources: solodit, swc, openzeppelin, protocol)")?;
        if let Some(i) = ingester_by_name(name, config)? {
            vec![i]
        } else {
            eprintln!("unknown source: {name}");
            return Ok(());
        }
    };

    for ingester in ingesters {
        let name = ingester.source_name().to_string();
        println!("→ ingesting {name}");
        let options = IngestOptions {
            incremental: args.incremental,
            max_records: args.max_records,
            progress: Some(progress_printer()),
            ..Default::default()
        };
        match ingester
            .ingest(
                store.clone() as Arc<dyn VectorStore>,
                embeddings.clone(),
                options,
            )
            .await
        {
            Ok(report) => {
                // Clear the in-place progress line before printing
                // the final summary — otherwise it leaves leftover
                // chars when the summary line is shorter.
                eprintln!();
                println!(
                    "  {name}: scanned={}, new={}, updated={}, skipped={}, tokens={}, errors={}, {:.1}s",
                    report.records_scanned,
                    report.records_new,
                    report.records_updated,
                    report.records_skipped,
                    report.embedding_tokens_used,
                    report.errors.len(),
                    report.duration.as_secs_f32(),
                );
                for (id, err) in report.errors.iter().take(3) {
                    println!("    ! {id}: {err}");
                }
            }
            Err(e) => {
                // One ingester failing doesn't stop the others.
                eprintln!();
                eprintln!("  {name}: FAILED: {e}");
            }
        }
    }
    Ok(())
}

/// Build a progress callback that prints a single in-place
/// line to stderr so operators see "scanned=… upserted=… tokens=…"
/// climbing without the terminal flooding. Uses `\r` for in-place
/// updates; the caller prints a trailing newline before the summary
/// line so the progress doesn't get overwritten.
fn progress_printer() -> Arc<dyn Fn(IngestProgress) + Send + Sync> {
    Arc::new(|p: IngestProgress| {
        use std::io::Write;
        let mut err = std::io::stderr().lock();
        // `\r` carriage-returns to column 0; pad with trailing
        // spaces so shorter lines don't leave stale characters.
        let _ = write!(
            err,
            "\r  scanned={:>6} upserted={:>6} skipped={:>6} tokens={:>8}   ",
            p.records_scanned, p.records_upserted, p.records_skipped, p.embedding_tokens_used,
        );
        let _ = err.flush();
    })
}

/// Build a `GithubClient` from `config.github_token`. Works without
/// a token (60/hour unauthenticated is fine for default-branch
/// lookups); the `None` branch here only fires if client
/// construction itself fails (network stack, etc.).
fn build_github_client(config: &Config) -> Option<GithubClient> {
    GithubClient::new(config.github_token.as_deref()).ok()
}

/// Return every ingester the CLI knows how to build, in a stable
/// order so `--all` produces a reproducible sequence.
fn available_ingesters(config: &Config) -> Vec<Box<dyn Ingester>> {
    let repo_cache = Arc::new(
        RepoCache::open()
            .unwrap_or_else(|_| panic!("can't open repo cache — check filesystem permissions")),
    );
    let mut out: Vec<Box<dyn Ingester>> = Vec::new();
    out.push(Box::new(SoloditIngester::new()));
    // SWC pins its own ref — no GithubClient needed.
    out.push(Box::new(SwcIngester::new(Arc::clone(&repo_cache))));
    let mut oz = OzAdvisoriesIngester::new();
    if let Some(tok) = &config.github_token {
        oz = oz.with_token(tok.clone());
    }
    out.push(Box::new(oz));
    out
}

fn ingester_by_name(name: &str, config: &Config) -> Result<Option<Box<dyn Ingester>>> {
    match name {
        "solodit" => Ok(Some(Box::new(SoloditIngester::new()))),
        "swc" => {
            let repo_cache = Arc::new(RepoCache::open().context("opening repo cache")?);
            Ok(Some(Box::new(SwcIngester::new(repo_cache))))
        }
        "openzeppelin" => {
            let mut oz = OzAdvisoriesIngester::new();
            if let Some(tok) = &config.github_token {
                oz = oz.with_token(tok.clone());
            }
            Ok(Some(Box::new(oz)))
        }
        _ => Ok(None),
    }
}

// --- add-protocol ----------------------------------------------------

async fn run_add_protocol(args: &AddProtocolArgs, config: &Config) -> Result<()> {
    let source = pick_protocol_source(args)?;
    let repo_cache = if matches!(source, ProtocolSource::GithubDir { .. }) {
        Some(Arc::new(RepoCache::open().context("opening repo cache")?))
    } else {
        None
    };
    let mut ingester = ProtocolIngester::new(args.engagement_id.clone(), source, repo_cache);
    if let Some(gh) = build_github_client(config) {
        ingester = ingester.with_github(gh);
    }
    let store = open_store().await?;
    let embeddings = build_embeddings(config)?;
    println!(
        "→ ingesting protocol docs for engagement '{}'",
        args.engagement_id
    );
    let report = ingester
        .ingest(
            store as Arc<dyn VectorStore>,
            embeddings,
            IngestOptions::default(),
        )
        .await?;
    println!(
        "  scanned={}, new={}, updated={}, errors={}, {:.1}s",
        report.records_scanned,
        report.records_new,
        report.records_updated,
        report.errors.len(),
        report.duration.as_secs_f32(),
    );
    Ok(())
}

fn pick_protocol_source(args: &AddProtocolArgs) -> Result<ProtocolSource> {
    if let Some(url) = &args.url {
        return Ok(ProtocolSource::Url(url.clone()));
    }
    if let Some(pdf) = &args.pdf {
        return Ok(ProtocolSource::Pdf(pdf.clone()));
    }
    if let Some(file) = &args.file {
        return Ok(ProtocolSource::File(file.clone()));
    }
    if let Some(spec) = &args.github {
        let (repo_part, subdir) = match spec.split_once(':') {
            Some((r, s)) => (r, Some(PathBuf::from(s))),
            None => (spec.as_str(), None),
        };
        let (owner, repo) = repo_part
            .split_once('/')
            .context("--github expects 'owner/repo[:subdir]' form")?;
        return Ok(ProtocolSource::GithubDir {
            owner: owner.to_string(),
            repo: repo.to_string(),
            subdir,
        });
    }
    anyhow::bail!("specify one of --url / --pdf / --file / --github")
}

// --- list-findings + show-finding ------------------------------------

async fn run_list_findings(args: &ListFindingsArgs, config: &Config) -> Result<()> {
    let kb = open_kb(config).await?;
    let filters = SearchFilters {
        collections: vec![basilisk_vector::schema::USER_FINDINGS.into()],
        severity: args.severity.clone(),
        include_corrections: false,
        ..Default::default()
    };
    // Use a zero vector by searching with a generic query that
    // triggers the embedding path; result ordering is irrelevant
    // for listing. The limit governs output.
    let hits = kb.search("findings listing", filters, args.limit).await?;
    if hits.is_empty() {
        println!("no findings recorded");
        return Ok(());
    }
    println!(
        "{:<16}  {:<10}  {:<14}  {:<24}  title",
        "id", "severity", "category", "target",
    );
    for h in &hits {
        let sev = h
            .metadata
            .extra
            .get("severity")
            .and_then(|v| v.as_str())
            .unwrap_or("-");
        let cat = h
            .metadata
            .extra
            .get("category")
            .and_then(|v| v.as_str())
            .unwrap_or("-");
        let target = h
            .metadata
            .extra
            .get("target")
            .and_then(|v| v.as_str())
            .unwrap_or("-");
        // Filter by session if set — we don't push this into
        // VectorStore::search because session_id lives in
        // metadata.extra and Filter::Equals on nested JSON paths
        // is post-filtered by the memory store anyway.
        if let Some(session) = &args.session {
            let hit_session = h.metadata.extra.get("session_id").and_then(|v| v.as_str());
            if hit_session != Some(session.as_str()) {
                continue;
            }
        }
        let id_short = h.id.get(..14).unwrap_or(&h.id);
        let title = h.text.lines().next().unwrap_or("");
        println!("{id_short}  {sev:<10}  {cat:<14}  {target:<24}  {title}",);
    }
    Ok(())
}

async fn run_show_finding(args: &ShowFindingArgs, config: &Config) -> Result<()> {
    let kb = open_kb(config).await?;
    let id = FindingId::new(&args.id);
    if let Some(r) = kb.get_finding(&id).await? {
        println!("id:          {}", r.id);
        println!("source:      {}", r.metadata.source);
        println!("kind:        {}", r.metadata.kind);
        if let Some(sev) = r.metadata.extra.get("severity").and_then(|v| v.as_str()) {
            println!("severity:    {sev}");
        }
        if let Some(cat) = r.metadata.extra.get("category").and_then(|v| v.as_str()) {
            println!("category:    {cat}");
        }
        if let Some(target) = r.metadata.extra.get("target").and_then(|v| v.as_str()) {
            println!("target:      {target}");
        }
        if let Some(session) = r.metadata.extra.get("session_id").and_then(|v| v.as_str()) {
            println!("session:     {session}");
        }
        println!();
        println!("{}", r.text);
    } else {
        eprintln!("no finding with id {}", args.id);
        std::process::exit(1);
    }
    Ok(())
}

// --- correct / dismiss / confirm -------------------------------------

async fn run_correct(args: &CorrectArgs, config: &Config) -> Result<()> {
    let kb = open_kb(config).await?;
    kb.record_correction(
        &FindingId::new(&args.id),
        Correction {
            reason: args.reason.clone(),
            corrected_severity: args.severity.clone(),
            corrected_category: args.category.clone(),
        },
    )
    .await
    .with_context(|| format!("correcting finding {}", args.id))?;
    println!("corrected {}", args.id);
    Ok(())
}

async fn run_dismiss(args: &DismissArgs, config: &Config) -> Result<()> {
    let kb = open_kb(config).await?;
    kb.record_correction(
        &FindingId::new(&args.id),
        Correction {
            reason: args.reason.clone(),
            corrected_severity: None,
            corrected_category: None,
        },
    )
    .await
    .with_context(|| format!("dismissing finding {}", args.id))?;
    kb.record_verdict(&FindingId::new(&args.id), UserVerdict::Dismissed)
        .await?;
    println!("dismissed {}", args.id);
    Ok(())
}

async fn run_confirm(args: &ConfirmArgs, config: &Config) -> Result<()> {
    let kb = open_kb(config).await?;
    kb.record_verdict(&FindingId::new(&args.id), UserVerdict::Confirmed)
        .await
        .with_context(|| format!("confirming finding {}", args.id))?;
    println!("confirmed {}", args.id);
    Ok(())
}

// --- search ----------------------------------------------------------

async fn run_search(args: &SearchArgs, config: &Config) -> Result<()> {
    let kb = open_kb(config).await?;
    let filters = SearchFilters {
        collections: args.collection.clone().map(|c| vec![c]).unwrap_or_default(),
        kind: args.kind.clone(),
        severity: args.severity.clone(),
        include_corrections: true,
        ..Default::default()
    };
    let hits = kb.search(&args.query, filters, args.limit).await?;
    if hits.is_empty() {
        println!("no matches");
        return Ok(());
    }
    for (i, h) in hits.iter().enumerate() {
        let title = h.text.lines().next().unwrap_or("");
        println!(
            "{i:>2}. [{:.3}] {source:<12} {kind:<10} {id}",
            h.score,
            source = h.source,
            kind = h.kind,
            id = h.id.get(..14).unwrap_or(&h.id),
        );
        println!("    {title}");
        if !h.corrections.is_empty() {
            for c in &h.corrections {
                println!("    ! correction: {}", c.reason);
            }
        }
    }
    Ok(())
}

// --- clear -----------------------------------------------------------

async fn run_clear(args: &ClearArgs, _config: &Config) -> Result<()> {
    let store = open_store().await?;
    let targets: Vec<String> = if args.all {
        store
            .list_collections()
            .await?
            .into_iter()
            .map(|c| c.name)
            .collect()
    } else {
        vec![args
            .collection
            .clone()
            .context("specify a collection or --all")?]
    };

    if !args.yes {
        let label = if args.all {
            "ALL collections".to_string()
        } else {
            format!(
                "collection '{}'",
                targets.first().map_or("?", String::as_str)
            )
        };
        eprintln!(
            "About to delete {label} from {}.",
            default_store_path().display()
        );
        eprint!("Type 'y' to confirm: ");
        let _ = std::io::stderr().flush();
        let mut buf = [0u8; 2];
        let n = std::io::Read::read(&mut std::io::stdin(), &mut buf).unwrap_or(0);
        let answer = std::str::from_utf8(&buf[..n]).unwrap_or("").trim();
        if answer != "y" && answer != "Y" {
            eprintln!("aborted");
            return Ok(());
        }
    }

    for name in &targets {
        match store.delete_collection(name).await {
            Ok(()) => println!("cleared {name}"),
            Err(e) => eprintln!("  {name}: {e}"),
        }
    }
    Ok(())
}
