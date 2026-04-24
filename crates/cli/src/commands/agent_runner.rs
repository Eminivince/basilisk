//! Shared helpers that drive [`AgentRunner`] from the CLI.
//!
//! Entry points:
//!
//!  - [`AgentFlags`] — the `--agent`-gated flags on `audit recon`.
//!  - [`OutputFormat`] — how to render the final outcome (pretty / JSON).
//!  - [`run_agent`] — builds the backend + registry + session store,
//!    calls [`AgentRunner::run_with_observer`], and renders the result.
//!  - [`PrettyObserver`] — stderr-writing observer for the live UX.
//!  - [`resume_agent`] — re-attaches to an interrupted session and
//!    continues its loop (`audit session resume`).
//!
//! This module used to back a top-level `audit agent <target>`
//! subcommand (`CP6a`); that entry point was withdrawn in `CP6c` so
//! the spec's `audit recon <target> --agent` surface is the single
//! way to invoke the agent. The helpers below are reused verbatim by
//! both `recon` and `session resume`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use basilisk_agent::{
    default_db_path, standard_registry, AgentObserver, AgentOutcome, AgentRunner, AgentStats,
    AgentStopReason, Budget, LoadedSession, NoopObserver, SessionId, SessionStore,
    RECON_V1_PROMPT,
};
use basilisk_core::Config;
use basilisk_git::RepoCache;
use basilisk_github::GithubClient;
use basilisk_llm::{AnthropicBackend, LlmBackend, Message, MessageRole, DEFAULT_MODEL};
use clap::{Args, ValueEnum};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable summary plus the final report markdown (default).
    #[default]
    Pretty,
    /// Pretty-printed JSON of the [`AgentOutcome`].
    Json,
}

/// Flags that attach to `audit recon <target>` when `--agent` is set.
///
/// Every flag carries a unique clap `id` so this struct can be
/// `#[command(flatten)]`-ed into another command (like `recon`)
/// without colliding on arg names (both have `--output`, both have
/// `--max-duration`, etc).
///
/// The user-facing CLI names stay clean (`--model`, `--max-turns`,
/// `--agent-output`, …). Rust field names are free to be whatever
/// reads best.
#[derive(Debug, Args, Default)]
pub struct AgentFlags {
    /// Free-form note attached to the session row.
    #[arg(long = "session-note", id = "agent_session_note")]
    pub note: Option<String>,

    /// Path to the session database. Defaults to `~/.basilisk/sessions.db`.
    #[arg(long = "db", id = "agent_db")]
    pub db: Option<PathBuf>,

    /// Path to a file containing the system prompt. Overrides the
    /// embedded `recon_v1` prompt. Useful for prompt iteration.
    #[arg(long = "system-prompt", id = "agent_system_prompt")]
    pub system_prompt: Option<PathBuf>,

    /// Anthropic model id. Defaults to the crate's `DEFAULT_MODEL`.
    #[arg(long = "model", id = "agent_model")]
    pub model: Option<String>,

    /// Max LLM turns.
    #[arg(long = "max-turns", id = "agent_max_turns")]
    pub max_turns: Option<u32>,

    /// Max total tokens (input + output + cache).
    #[arg(long = "max-tokens", id = "agent_max_tokens")]
    pub max_tokens: Option<u64>,

    /// Max estimated spend in cents.
    #[arg(long = "max-cost", id = "agent_max_cost")]
    pub max_cost_cents: Option<u32>,

    /// Max wall-clock duration in seconds (agent only).
    #[arg(long = "agent-max-duration", id = "agent_max_duration")]
    pub max_duration_secs: Option<u64>,

    /// Output format for the agent's final summary.
    #[arg(
        long = "agent-output",
        id = "agent_output",
        value_enum,
        default_value_t = OutputFormat::Pretty,
    )]
    pub output: OutputFormat,

    /// Suppress the live progress stream on stderr.
    #[arg(long = "no-stream", id = "agent_no_stream")]
    pub no_stream: bool,
}

/// Entry point: run the agent against `target` using `flags`.
///
/// Called from `commands::recon::run` when `--agent` is set.
pub async fn run_agent(target: &str, flags: &AgentFlags, config: &Config) -> Result<()> {
    let (runner, db_path) = build_runner(flags, config)?;

    eprintln!(
        "→ agent running  target={:?}  model={}  budget={:?}",
        target,
        runner.model_identifier(),
        runner.budget(),
    );
    eprintln!("  session db: {}", db_path.display());

    let pretty = PrettyObserver::new();
    let noop = NoopObserver;
    let observer: &dyn AgentObserver = if flags.no_stream { &noop } else { &pretty };

    let outcome = runner
        .run_with_observer(
            target.to_string(),
            build_initial_message(target, flags.note.as_deref()),
            flags.note.clone(),
            observer,
        )
        .await
        .context("agent run failed")?;

    render_outcome(&outcome, flags.output);
    Ok(())
}

/// Resume an interrupted session. Loads its history, verifies the
/// system prompt hasn't drifted (unless `force_prompt_change` is set),
/// and continues the loop. Used by `audit session resume`.
pub async fn resume_agent(
    session_id: &SessionId,
    flags: &AgentFlags,
    force_prompt_change: bool,
    config: &Config,
) -> Result<()> {
    let (runner, db_path) = build_runner(flags, config)?;
    let loaded = runner
        .store()
        .load_session(session_id)
        .with_context(|| format!("loading session {session_id}"))?;

    let target = loaded.session.target.clone();
    let prior_prompt_hash = loaded.session.system_prompt_hash.clone();
    let current_prompt_hash = sha256_hex(runner.system_prompt());

    if prior_prompt_hash != current_prompt_hash && !force_prompt_change {
        anyhow::bail!(
            "system prompt hash has changed since session {session_id} started \
             (was {short_old}, now {short_new}). Re-run with --force-prompt-change \
             to continue with the new prompt, or supply --system-prompt pointing \
             at the original.",
            short_old = short(&prior_prompt_hash),
            short_new = short(&current_prompt_hash),
        );
    }

    eprintln!(
        "→ resuming session {session_id}  target={target:?}  model={}",
        runner.model_identifier(),
    );
    eprintln!("  session db: {}", db_path.display());

    let history = replay_history(&loaded);
    let pretty = PrettyObserver::new();
    let noop = NoopObserver;
    let observer: &dyn AgentObserver = if flags.no_stream { &noop } else { &pretty };

    let outcome = runner
        .resume_with_observer(session_id.clone(), target, history, observer)
        .await
        .context("agent resume failed")?;

    render_outcome(&outcome, flags.output);
    Ok(())
}

fn build_runner(flags: &AgentFlags, config: &Config) -> Result<(AgentRunner, PathBuf)> {
    let api_key = config
        .anthropic_api_key
        .as_deref()
        .context("ANTHROPIC_API_KEY is not set — export it or put it in a .env file")?;
    let model = flags.model.as_deref().unwrap_or(DEFAULT_MODEL);
    let backend: Arc<dyn LlmBackend> = Arc::new(
        AnthropicBackend::with_model(api_key, model).context("initialising Anthropic backend")?,
    );

    let db_path = flags.db.clone().unwrap_or_else(default_db_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("creating session DB parent directory {}", parent.display())
        })?;
    }
    let store = Arc::new(
        SessionStore::open(&db_path)
            .with_context(|| format!("opening session DB at {}", db_path.display()))?,
    );

    let swept = store
        .mark_running_as_interrupted("agent process restart")
        .context("marking stale sessions interrupted")?;
    if swept > 0 {
        tracing::info!(count = swept, "marked stale running sessions as interrupted");
    }

    let system_prompt = load_system_prompt(flags.system_prompt.as_deref())?;
    let github = Arc::new(
        GithubClient::new(config.github_token.as_deref()).context("initialising GitHub client")?,
    );
    let repo_cache = Arc::new(RepoCache::open().context("opening repo cache")?);

    let runner = AgentRunner::new(
        backend,
        standard_registry(),
        Arc::clone(&store),
        Arc::new(config.clone()),
        github,
        repo_cache,
        system_prompt,
        build_budget(flags),
    );
    Ok((runner, db_path))
}

fn build_budget(flags: &AgentFlags) -> Budget {
    let mut b = Budget::default();
    if let Some(v) = flags.max_turns {
        b.max_turns = v;
    }
    if let Some(v) = flags.max_tokens {
        b.max_tokens_total = v;
    }
    if let Some(v) = flags.max_cost_cents {
        b.max_cost_cents = v;
    }
    if let Some(v) = flags.max_duration_secs {
        b.max_duration = Duration::from_secs(v);
    }
    b
}

fn load_system_prompt(path: Option<&Path>) -> Result<String> {
    let Some(p) = path else {
        return Ok(RECON_V1_PROMPT.to_string());
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

/// Rebuild a conversation history from a persisted session.
///
/// Turns are ordered by `turn_index`; each row's `content_json` is the
/// serialised `Vec<ContentBlock>` that went on the wire.
fn replay_history(loaded: &LoadedSession) -> Vec<Message> {
    loaded
        .turns
        .iter()
        .filter_map(|t| {
            let role = match t.role {
                basilisk_agent::TurnRole::User => MessageRole::User,
                basilisk_agent::TurnRole::Assistant => MessageRole::Assistant,
            };
            let content: Vec<basilisk_llm::ContentBlock> =
                serde_json::from_value(t.content.clone()).ok()?;
            Some(Message { role, content })
        })
        .collect()
}

fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}

fn short(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

pub fn render_outcome(outcome: &AgentOutcome, format: OutputFormat) {
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
    println!("stop_reason: {}", describe_stop_reason(&outcome.stop_reason));
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

/// Stderr-writing observer that prints the live progress of an agent
/// run: turn headers, assistant text as it streams, and one line per
/// tool call (`↳ calling` when it starts, `↳ <name>` with duration
/// when it returns).
pub struct PrettyObserver {
    state: Mutex<PrettyState>,
}

#[derive(Default)]
struct PrettyState {
    text_this_turn: bool,
}

impl PrettyObserver {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(PrettyState::default()),
        }
    }

    fn write_line(msg: &str) {
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "{msg}");
    }
}

impl Default for PrettyObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentObserver for PrettyObserver {
    fn on_session_start(&self, session_id: &SessionId) {
        Self::write_line(&format!("── session {session_id} ──"));
    }

    fn on_turn_start(&self, turn: u32) {
        {
            let mut s = self.state.lock().expect("pretty-observer state poisoned");
            s.text_this_turn = false;
        }
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
        Self::write_line(&format!("  ↳ calling {name}"));
    }

    fn on_tool_result(&self, _turn: u32, name: &str, ok: bool, duration_ms: u64) {
        let status = if ok { "ok" } else { "ERROR" };
        Self::write_line(&format!("  ↳ {name}  {status}  ({duration_ms}ms)"));
    }

    fn on_turn_end(&self, _turn: u32, stats: &AgentStats) {
        Self::write_line(&format!(
            "── turn end: {} tokens cumulative, ~{}¢ ──",
            stats.total_tokens(),
            stats.cost_cents,
        ));
    }

    fn on_session_complete(&self, _outcome: &AgentOutcome) {
        // Final summary is rendered by `render_outcome` on stdout.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_overrides_apply_on_top_of_defaults() {
        let flags = AgentFlags {
            note: None,
            db: None,
            system_prompt: None,
            model: None,
            max_turns: Some(5),
            max_tokens: None,
            max_cost_cents: Some(100),
            max_duration_secs: None,
            output: OutputFormat::Pretty,
            no_stream: false,
        };
        let b = build_budget(&flags);
        assert_eq!(b.max_turns, 5);
        assert_eq!(b.max_cost_cents, 100);
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

    #[test]
    fn default_prompt_falls_back_to_recon_v1_when_no_override() {
        let prompt = load_system_prompt(None).unwrap();
        assert_eq!(prompt, RECON_V1_PROMPT);
    }

    #[test]
    fn short_truncates_hashes_to_twelve_chars() {
        assert_eq!(short("abcdef0123456789"), "abcdef012345");
        assert_eq!(short("shorty"), "shorty");
    }
}
