//! `audit bench` — Set 9's evaluation surface.
//!
//! Lists, inspects, and runs the five benchmark targets shipped with
//! `basilisk-bench`. Each `run` spawns a `--vuln`-shaped agent
//! session against the target's address at the pinned fork block;
//! the resulting findings are scored against the target's
//! `expected_findings` and persisted to the `bench_runs` table
//! alongside the agent's session in `sessions.db`.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use basilisk_bench::{
    all_targets, score, AgentFindingSummary, BenchStore, BenchmarkRun, BenchmarkScore,
    BenchmarkTarget,
};
use basilisk_core::Config;
use clap::{Args, Subcommand};

use basilisk_agent::default_db_path;

use super::agent_runner::AgentFlags;

#[derive(Debug, Subcommand)]
pub enum BenchCmd {
    /// List every benchmark target.
    List,
    /// Show a target's full definition.
    Show(ShowArgs),
    /// Show run history, newest first.
    History(HistoryArgs),
    /// Run the agent against a target (or all targets), score the
    /// findings, and record the run. Uses `--vuln` mode; pass the
    /// same agent flags as `audit recon --agent` to override provider
    /// / model / budget.
    Run(RunArgs),
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Benchmark target id (e.g. `visor-2021`). Run
    /// `audit bench list` to see the full list.
    pub id: String,
}

#[derive(Debug, Args)]
pub struct HistoryArgs {
    /// Path to the session database. Defaults to
    /// `~/.basilisk/sessions.db`.
    #[arg(long, value_name = "PATH", env = "BASILISK_SESSION_DB")]
    pub db: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Target id to run against. Omit to run every target
    /// sequentially.
    pub id: Option<String>,

    #[command(flatten)]
    pub agent: AgentFlags,
}

pub async fn run(cmd: &BenchCmd, config: &Config) -> Result<()> {
    match cmd {
        BenchCmd::List => run_list(),
        BenchCmd::Show(args) => run_show(args),
        BenchCmd::History(args) => run_history(args),
        BenchCmd::Run(args) => run_run(args, config).await,
    }
}

fn run_list() -> Result<()> {
    println!("Basilisk benchmark targets:\n");
    for t in all_targets() {
        println!("  {:20} — {}", t.id, t.name);
        println!(
            "    chain={} block={} severity={:?}",
            t.chain, t.fork_block, t.severity
        );
        println!("    classes=[{}]", t.vulnerability_classes.join(", "),);
    }
    println!("\n{} targets shipped.", all_targets().len());
    Ok(())
}

fn run_show(args: &ShowArgs) -> Result<()> {
    let t = basilisk_bench::targets::by_id(&args.id).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown target id: {:?}. Run `audit bench list` for options.",
            args.id
        )
    })?;
    println!("{}", render_target(t));
    Ok(())
}

fn render_target(t: &BenchmarkTarget) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# {} ({})\n", t.name, t.id);
    let _ = writeln!(
        s,
        "- chain: {}\n- target: {:?}\n- fork block (pre-exploit): {}\n- exploit block: {}\n- severity: {:?}",
        t.chain, t.target_address, t.fork_block, t.exploit_block, t.severity,
    );
    let _ = writeln!(
        s,
        "- vulnerability classes: {}",
        t.vulnerability_classes.join(", "),
    );
    s.push_str("\n## Expected findings\n\n");
    for (i, e) in t.expected_findings.iter().enumerate() {
        let _ = writeln!(
            s,
            "  {}. class={} severity_min={} must_mention={:?}",
            i + 1,
            e.class,
            e.severity_min.as_str(),
            e.must_mention,
        );
        if !e.must_not_mention_only.is_empty() {
            let _ = writeln!(s, "     disqualifiers: {:?}", e.must_not_mention_only);
        }
    }
    s.push_str("\n## References\n\n");
    for r in t.references {
        let _ = writeln!(s, "  - {r}");
    }
    if !t.notes.is_empty() {
        s.push_str("\n## Notes\n\n");
        s.push_str(t.notes);
        s.push('\n');
    }
    s
}

fn run_history(args: &HistoryArgs) -> Result<()> {
    let db = args.db.clone().unwrap_or_else(default_db_path);
    let store = BenchStore::open(&db)
        .with_context(|| format!("opening bench history at {}", db.display()))?;
    let rows = store.history().context("reading bench history")?;
    if rows.is_empty() {
        println!("No bench runs recorded yet. Run `audit bench run <id>` to start.");
        return Ok(());
    }
    println!(
        "{:>4}  {:<20}  {:>8}  matches   misses   false_pos  session",
        "id", "target", "coverage",
    );
    for r in rows {
        let sess = r.session_id.as_deref().unwrap_or("-");
        println!(
            "{:>4}  {:<20}  {:>7.1}%    {:>3}     {:>3}       {:>3}     {}",
            r.id, r.target_id, r.coverage_percent, r.matches, r.misses, r.false_positives, sess,
        );
    }
    Ok(())
}

async fn run_run(args: &RunArgs, config: &Config) -> Result<()> {
    let targets: Vec<&'static BenchmarkTarget> = match args.id.as_deref() {
        Some(id) => {
            let t = basilisk_bench::targets::by_id(id).ok_or_else(|| {
                anyhow::anyhow!("unknown target id: {id:?} (run `audit bench list`)")
            })?;
            vec![t]
        }
        None => all_targets().to_vec(),
    };

    let db = args.agent.db.clone().unwrap_or_else(default_db_path);
    let store = BenchStore::open(&db)
        .with_context(|| format!("opening bench store at {}", db.display()))?;

    // Force --vuln on for bench runs regardless of what the operator
    // passed — bench is the whole point of vuln-mode calibration.
    let mut flags = args.agent.clone();
    flags.vuln = true;

    for t in targets {
        eprintln!(
            "\n=== bench {}: {} (block {}) ===",
            t.id, t.name, t.fork_block
        );
        let target_input = format!("{}/0x{}", t.chain, hex::encode(t.target_address.as_slice()));
        let started = std::time::Instant::now();
        let outcome = match super::agent_runner::run_agent_with_outcome(
            &target_input,
            &flags,
            config,
        )
        .await
        {
            Ok(o) => o,
            Err(e) => {
                eprintln!("  run failed: {e}");
                continue;
            }
        };
        let duration = started.elapsed();

        // Pull the agent's findings out of the session log. `audit
        // session show` can do this; here we read session_feedback
        // filtered by the session id — findings were written to
        // user_findings directly, but the tool_calls log also carries
        // each record_finding invocation's input, which is the shape
        // the scorer wants.
        let agent_findings = extract_agent_findings(&outcome.session_id, &db).unwrap_or_default();
        let (limitations_count, suspicions_count) =
            count_feedback(&outcome.session_id, &db).unwrap_or((0, 0));

        let run = BenchmarkRun {
            target_id: t.id.into(),
            session_id: outcome.session_id.clone(),
            agent_findings,
            duration,
            cost_cents: Some(outcome.stats.cost_cents),
            turns: outcome.stats.turns,
            limitations_count,
            suspicions_count,
        };
        let s = score(t, &run);
        print_score(&s);
        if let Err(e) = record_run(&store, t, &run, &s) {
            eprintln!("  failed to record run: {e}");
        }
    }
    Ok(())
}

fn print_score(s: &BenchmarkScore) {
    println!(
        "  coverage: {:.1}%  matches: {}  misses: {}  false-positives: {}",
        s.coverage_percent,
        s.matches.len(),
        s.misses.len(),
        s.false_positives.len(),
    );
    for m in &s.matches {
        println!(
            "    ✓ {} → \"{}\" ({})",
            m.expected_class, m.agent_finding_title, m.agent_finding_severity
        );
    }
    for m in &s.misses {
        println!(
            "    ✗ {} (wanted keywords: {:?}, min severity: {})",
            m.class, m.must_mention, m.severity_min
        );
    }
}

fn record_run(
    store: &BenchStore,
    target: &BenchmarkTarget,
    run: &BenchmarkRun,
    s: &BenchmarkScore,
) -> Result<()> {
    let run_json = serde_json::to_string(run)?;
    let created = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_millis(),
    )
    .unwrap_or(i64::MAX);
    let id = store.record(
        target.id,
        Some(run.session_id.as_str()),
        &run_json,
        s,
        created,
    )?;
    println!("  recorded as bench run #{id}");
    Ok(())
}

/// Pull every `record_finding` `tool_call` out of the session and
/// render it as a [`AgentFindingSummary`]. Uses the `SessionStore`
/// directly via the default db path.
fn extract_agent_findings(
    session_id: &basilisk_agent::SessionId,
    db_path: &std::path::Path,
) -> Result<Vec<AgentFindingSummary>> {
    let store = basilisk_agent::SessionStore::open(db_path)?;
    let loaded = store.load_session(session_id)?;
    let mut out = Vec::new();
    for tc in &loaded.tool_calls {
        if tc.tool_name != "record_finding" {
            continue;
        }
        let input = &tc.input;
        let title = input
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let severity = input
            .get("severity")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let category = input
            .get("category")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let summary = input
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        out.push(AgentFindingSummary {
            title,
            severity,
            category,
            summary,
        });
    }
    Ok(out)
}

fn count_feedback(
    session_id: &basilisk_agent::SessionId,
    db_path: &std::path::Path,
) -> Result<(u32, u32)> {
    let store = basilisk_agent::SessionStore::open(db_path)?;
    let lim = u32::try_from(store.count_feedback(session_id, "limitation")?).unwrap_or(u32::MAX);
    let sus = u32::try_from(store.count_feedback(session_id, "suspicion")?).unwrap_or(u32::MAX);
    Ok((lim, sus))
}
