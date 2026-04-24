//! `audit agent <target>` — run the LLM-driven auditor on `target`.
//!
//! `CP6a` wires the pieces together end-to-end:
//!
//!  - `AnthropicBackend` (real LLM, non-streaming — `CP6b` swaps in
//!    streaming output for the terminal),
//!  - the `standard_registry` of 11 tools,
//!  - a `SessionStore` rooted at `--db` (defaulting to
//!    `<data-local>/basilisk/sessions.db`),
//!  - a `Budget` assembled from the `--max-*` flags.
//!
//! The system prompt in `CP6a` is a short placeholder; `CP8` replaces
//! it with the production recon brief.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use basilisk_agent::{
    default_db_path, standard_registry, AgentObserver, AgentOutcome, AgentRunner, AgentStats,
    AgentStopReason, Budget, NoopObserver, SessionId, SessionStore,
};
use basilisk_core::Config;
use basilisk_git::RepoCache;
use basilisk_github::GithubClient;
use basilisk_llm::{AnthropicBackend, LlmBackend, DEFAULT_MODEL};
use clap::{Args, ValueEnum};

/// Placeholder system prompt. `CP8` ships the production brief.
const PLACEHOLDER_SYSTEM_PROMPT: &str = "You are Basilisk, an LLM-driven smart-contract auditor \
running reconnaissance on a target. Use the available tools to classify the target, fetch \
sources, resolve dependencies, and inspect notable patterns. When you have enough information \
to write a useful recon brief, call `finalize_report`. Do not keep exploring once you have \
enough to brief a human reviewer. Prefer concise tool-use turns over long prose between calls.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable summary plus the final report markdown (default).
    Pretty,
    /// Pretty-printed JSON of the [`AgentOutcome`].
    Json,
}

#[derive(Debug, Args)]
pub struct AgentArgs {
    /// Target for the agent. Free-form: URL, address, local path, etc.
    /// Handed to the agent verbatim as part of the initial user message.
    pub target: String,

    /// Optional free-form note attached to the session row.
    #[arg(long)]
    pub note: Option<String>,

    /// Path to the session database. Defaults to
    /// `<data-local>/basilisk/sessions.db`.
    #[arg(long)]
    pub db: Option<PathBuf>,

    /// Path to a file containing the system prompt. When omitted, a
    /// placeholder prompt is used (CP8 ships the production one).
    #[arg(long)]
    pub system_prompt: Option<PathBuf>,

    /// Anthropic model id. Defaults to the crate's `DEFAULT_MODEL`.
    #[arg(long)]
    pub model: Option<String>,

    /// Max LLM turns.
    #[arg(long)]
    pub max_turns: Option<u32>,

    /// Max total tokens (input + output + cache).
    #[arg(long)]
    pub max_tokens: Option<u64>,

    /// Max estimated spend in cents.
    #[arg(long)]
    pub max_cost_cents: Option<u32>,

    /// Max wall-clock duration in seconds.
    #[arg(long)]
    pub max_duration_secs: Option<u64>,

    /// Output format for the final summary.
    #[arg(long, value_enum, default_value_t = OutputFormat::Pretty)]
    pub output: OutputFormat,

    /// Suppress the live progress stream on stderr. Has no effect on
    /// the final summary (`--output`). Useful when redirecting output
    /// for scripting or when a non-TTY consumer doesn't want noise.
    #[arg(long)]
    pub quiet: bool,
}

pub async fn run(args: &AgentArgs, config: &Config) -> Result<()> {
    let api_key = config
        .anthropic_api_key
        .as_deref()
        .context("ANTHROPIC_API_KEY is not set — export it or put it in a .env file")?;

    let model = args.model.as_deref().unwrap_or(DEFAULT_MODEL);
    let backend: Arc<dyn LlmBackend> = Arc::new(
        AnthropicBackend::with_model(api_key, model).context("initialising Anthropic backend")?,
    );

    let db_path = args.db.clone().unwrap_or_else(default_db_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("creating session DB parent directory {}", parent.display())
        })?;
    }
    let store = Arc::new(
        SessionStore::open(&db_path)
            .with_context(|| format!("opening session DB at {}", db_path.display()))?,
    );

    // Fold any lingering `running` rows from a crashed previous run.
    let swept = store
        .mark_running_as_interrupted("agent process restart")
        .context("marking stale sessions interrupted")?;
    if swept > 0 {
        tracing::info!(
            count = swept,
            "marked stale running sessions as interrupted",
        );
    }

    let system_prompt = load_system_prompt(args.system_prompt.as_deref())?;
    let github = Arc::new(
        GithubClient::new(config.github_token.as_deref()).context("initialising GitHub client")?,
    );
    let repo_cache = Arc::new(RepoCache::open().context("opening repo cache")?);

    let budget = build_budget(args);
    let runner = AgentRunner::new(
        backend,
        standard_registry(),
        Arc::clone(&store),
        Arc::new(config.clone()),
        github,
        repo_cache,
        system_prompt,
        budget,
    );

    eprintln!(
        "starting agent session  target={:?}  model={}  budget={:?}",
        args.target,
        runner.model_identifier(),
        runner.budget(),
    );
    eprintln!("  session db: {}", db_path.display());

    let pretty = PrettyObserver::new();
    let noop = NoopObserver;
    let observer: &dyn AgentObserver = if args.quiet {
        &noop
    } else {
        &pretty
    };

    let outcome = runner
        .run_with_observer(
            args.target.clone(),
            build_initial_message(&args.target, args.note.as_deref()),
            args.note.clone(),
            observer,
        )
        .await
        .context("agent run failed")?;

    render_outcome(&outcome, args.output);
    Ok(())
}

/// Stderr-writing observer that prints the live progress of an agent
/// run: turn headers, assistant text as it streams, and one line per
/// tool call (`>` when it starts, `<` with duration when it returns).
///
/// Output goes to stderr so `--output json` piping stays clean. The
/// only shared state is a flag tracking whether the current turn has
/// emitted text, so we can insert a newline before the first tool line
/// without leaving a stray blank line when the turn is pure tool use.
struct PrettyObserver {
    state: Mutex<PrettyState>,
}

#[derive(Default)]
struct PrettyState {
    text_this_turn: bool,
}

impl PrettyObserver {
    fn new() -> Self {
        Self {
            state: Mutex::new(PrettyState::default()),
        }
    }

    fn write_line(msg: &str) {
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "{msg}");
    }
}

impl AgentObserver for PrettyObserver {
    fn on_session_start(&self, session_id: &SessionId) {
        Self::write_line(&format!("── session {session_id} ──"));
    }

    fn on_turn_start(&self, turn: u32) {
        let mut s = self.state.lock().expect("pretty-observer state poisoned");
        s.text_this_turn = false;
        drop(s);
        Self::write_line(&format!("━━ turn {turn} ━━"));
    }

    fn on_text_delta(&self, _turn: u32, text: &str) {
        {
            let mut s = self.state.lock().expect("pretty-observer state poisoned");
            s.text_this_turn = true;
        }
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(text.as_bytes());
        let _ = err.flush();
    }

    fn on_tool_use_start(&self, _turn: u32, name: &str, _tool_use_id: &str) {
        let had_text = {
            let s = self.state.lock().expect("pretty-observer state poisoned");
            s.text_this_turn
        };
        if had_text {
            Self::write_line("");
        }
        Self::write_line(&format!("> {name}"));
    }

    fn on_tool_result(&self, _turn: u32, name: &str, ok: bool, duration_ms: u64) {
        let status = if ok { "ok" } else { "ERROR" };
        Self::write_line(&format!("< {name}  {status}  ({duration_ms}ms)"));
    }

    fn on_turn_end(&self, _turn: u32, stats: &AgentStats) {
        Self::write_line(&format!(
            "── turn end: {} tokens cumulative, ~{}¢ ──",
            stats.total_tokens(),
            stats.cost_cents,
        ));
    }

    fn on_session_complete(&self, _outcome: &AgentOutcome) {
        // Final summary is rendered by `render_outcome` on stdout; we
        // don't echo it here to avoid double-printing.
    }
}

fn build_budget(args: &AgentArgs) -> Budget {
    let mut b = Budget::default();
    if let Some(v) = args.max_turns {
        b.max_turns = v;
    }
    if let Some(v) = args.max_tokens {
        b.max_tokens_total = v;
    }
    if let Some(v) = args.max_cost_cents {
        b.max_cost_cents = v;
    }
    if let Some(v) = args.max_duration_secs {
        b.max_duration = Duration::from_secs(v);
    }
    b
}

fn load_system_prompt(path: Option<&std::path::Path>) -> Result<String> {
    let Some(p) = path else {
        tracing::warn!(
            "no --system-prompt supplied; using placeholder (see CP8 for production prompt)"
        );
        return Ok(PLACEHOLDER_SYSTEM_PROMPT.to_string());
    };
    std::fs::read_to_string(p)
        .with_context(|| format!("reading system prompt from {}", p.display()))
}

fn build_initial_message(target: &str, note: Option<&str>) -> String {
    let mut msg = format!(
        "Target: {target}\n\n\
         Please perform reconnaissance. Classify the target, pull any sources that exist, \
         and investigate notable patterns. Call `finalize_report` when you have enough \
         to write a useful recon brief for a human reviewer."
    );
    if let Some(n) = note {
        msg.push_str("\n\nOperator note: ");
        msg.push_str(n);
    }
    msg
}

fn render_outcome(outcome: &AgentOutcome, format: OutputFormat) {
    match format {
        OutputFormat::Json => match serde_json::to_string_pretty(outcome) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("failed to serialise outcome: {e}"),
        },
        OutputFormat::Pretty => render_pretty(outcome),
    }
}

fn render_pretty(outcome: &AgentOutcome) {
    let status = if outcome.stop_reason.is_success() {
        "COMPLETED"
    } else {
        "FAILED"
    };
    println!();
    println!("── agent session: {status} ──");
    println!("session_id: {}", outcome.session_id);
    println!(
        "stop_reason: {}",
        describe_stop_reason(&outcome.stop_reason)
    );
    println!(
        "stats: {} turns, {} tool calls, {} tokens, ~{}¢, {}ms",
        outcome.stats.turns,
        outcome.stats.tool_calls,
        outcome.stats.total_tokens(),
        outcome.stats.cost_cents,
        outcome.stats.duration_ms,
    );
    println!();

    if let Some(report) = &outcome.final_report {
        println!("── final report ({:?}) ──", report.confidence);
        println!("{}", report.markdown);
        if let Some(notes) = &report.notes {
            println!();
            println!("── reviewer notes ──");
            println!("{notes}");
        }
    } else {
        println!("(no final report — agent did not call finalize_report)");
    }
}

fn describe_stop_reason(r: &AgentStopReason) -> String {
    match r {
        AgentStopReason::ReportFinalized => "report_finalized".into(),
        AgentStopReason::TurnLimitReached => "turn_limit_reached".into(),
        AgentStopReason::TokenBudgetExhausted => "token_budget_exhausted".into(),
        AgentStopReason::CostBudgetExhausted => "cost_budget_exhausted".into(),
        AgentStopReason::DurationLimitReached => "duration_limit_reached".into(),
        AgentStopReason::LlmError { message } => format!("llm_error: {message}"),
        AgentStopReason::ToolError { tool, message } => format!("tool_error ({tool}): {message}"),
        AgentStopReason::UserInterrupt => "user_interrupt".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_overrides_apply_on_top_of_defaults() {
        let args = AgentArgs {
            target: "x".into(),
            note: None,
            db: None,
            system_prompt: None,
            model: None,
            max_turns: Some(5),
            max_tokens: None,
            max_cost_cents: Some(100),
            max_duration_secs: None,
            output: OutputFormat::Pretty,
            quiet: false,
        };
        let b = build_budget(&args);
        assert_eq!(b.max_turns, 5);
        assert_eq!(b.max_cost_cents, 100);
        // Unspecified fields keep their default.
        assert_eq!(b.max_tokens_total, Budget::default().max_tokens_total);
        assert_eq!(b.max_duration, Budget::default().max_duration);
    }

    #[test]
    fn initial_message_embeds_target_and_note() {
        let msg = build_initial_message("eth/0xdead", Some("trusted author"));
        assert!(msg.contains("eth/0xdead"));
        assert!(msg.contains("finalize_report"));
        assert!(msg.contains("trusted author"));
    }

    #[test]
    fn initial_message_omits_note_section_when_absent() {
        let msg = build_initial_message("x", None);
        assert!(!msg.contains("Operator note"));
    }

    #[test]
    fn describe_stop_reason_renders_payloads() {
        assert_eq!(
            describe_stop_reason(&AgentStopReason::ReportFinalized),
            "report_finalized"
        );
        let s = describe_stop_reason(&AgentStopReason::LlmError {
            message: "oops".into(),
        });
        assert!(s.contains("oops"));
    }
}
