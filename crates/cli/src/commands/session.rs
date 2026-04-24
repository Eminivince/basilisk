//! `audit session` — inspect, resume, and delete agent sessions.
//!
//! Four subcommands:
//!
//!  - [`SessionCmd::List`] — recent sessions, one line each.
//!  - [`SessionCmd::Show`] — full transcript; supports
//!    `--format json` and `--report-only`.
//!  - [`SessionCmd::Resume`] — continue an interrupted session. The
//!    system prompt must match what the session started with
//!    (sha-256 hash); pass `--force-prompt-change` to override.
//!  - [`SessionCmd::Delete`] — remove a session row from SQLite
//!    (cascades to turns + tool_calls). Prompts for confirmation
//!    unless `--yes` is set.

use std::io::{self, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use basilisk_agent::{
    default_db_path, LoadedSession, SessionId, SessionStatus, SessionStore, SessionSummary,
    ToolCallRecord, TurnRecord, TurnRole,
};
use basilisk_core::Config;
use clap::{Args, Subcommand, ValueEnum};

use crate::commands::agent_runner::{self, AgentFlags};

#[derive(Debug, Subcommand)]
pub enum SessionCmd {
    /// List recent sessions.
    List(ListArgs),
    /// Show a session's full transcript, or just its final report.
    Show(ShowArgs),
    /// Continue an interrupted session where it left off.
    Resume(ResumeArgs),
    /// Delete a session (and its turns + tool calls) from the DB.
    Delete(DeleteArgs),
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Path to the session database. Defaults to `~/.basilisk/sessions.db`.
    #[arg(long)]
    pub db: Option<PathBuf>,

    /// Max number of sessions to show. Newest first.
    #[arg(long, default_value_t = 20)]
    pub limit: usize,

    /// Only show sessions with this status.
    #[arg(long, value_enum)]
    pub status: Option<StatusFilter>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum StatusFilter {
    Running,
    Completed,
    Failed,
    Interrupted,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Session ID to show.
    pub id: String,

    /// Path to the session database. Defaults to `~/.basilisk/sessions.db`.
    #[arg(long)]
    pub db: Option<PathBuf>,

    /// Render format.
    #[arg(long, value_enum, default_value_t = ShowFormat::Pretty)]
    pub format: ShowFormat,

    /// Print only the final report markdown (skip transcript + metadata).
    #[arg(long)]
    pub report_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ShowFormat {
    Pretty,
    Json,
}

#[derive(Debug, Args)]
pub struct ResumeArgs {
    /// Session ID to resume.
    pub id: String,

    /// Allow resume even if the system prompt has changed since the
    /// session started.
    #[arg(long)]
    pub force_prompt_change: bool,

    #[command(flatten)]
    pub flags: AgentFlags,
}

#[derive(Debug, Args)]
pub struct DeleteArgs {
    /// Session ID to delete.
    pub id: String,

    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub yes: bool,

    /// Path to the session database. Defaults to `~/.basilisk/sessions.db`.
    #[arg(long)]
    pub db: Option<PathBuf>,
}

pub async fn run(cmd: &SessionCmd, config: &Config) -> Result<()> {
    match cmd {
        SessionCmd::List(args) => run_list(args),
        SessionCmd::Show(args) => run_show(args),
        SessionCmd::Resume(args) => run_resume(args, config).await,
        SessionCmd::Delete(args) => run_delete(args),
    }
}

fn resolve_db(db: Option<&std::path::Path>) -> Result<SessionStore> {
    let path = db
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(default_db_path);
    SessionStore::open(&path).with_context(|| format!("opening session DB at {}", path.display()))
}

fn run_list(args: &ListArgs) -> Result<()> {
    let store = resolve_db(args.db.as_deref())?;
    let status_filter = args.status.map(|s| match s {
        StatusFilter::Running => SessionStatus::Running,
        StatusFilter::Completed => SessionStatus::Completed,
        StatusFilter::Failed => SessionStatus::Failed,
        StatusFilter::Interrupted => SessionStatus::Interrupted,
    });
    let limit = u32::try_from(args.limit).unwrap_or(u32::MAX);
    let rows = store
        .list_sessions(Some(limit), status_filter)
        .context("listing sessions")?;

    if rows.is_empty() {
        println!("(no sessions)");
        return Ok(());
    }
    for row in &rows {
        println!("{}", format_summary_line(row));
    }
    Ok(())
}

fn format_summary_line(s: &SessionSummary) -> String {
    let when = format_time(s.created_at);
    let target = truncate(&s.target, 40);
    let confidence = s.final_confidence.as_deref().unwrap_or("-");
    format!(
        "{id:12}  {when}  {status:<12}  {confidence:<7}  {target}",
        id = short_id(&s.id),
        when = when,
        status = s.status.as_str(),
        confidence = confidence,
        target = target,
    )
}

fn run_show(args: &ShowArgs) -> Result<()> {
    let store = resolve_db(args.db.as_deref())?;
    let session_id = SessionId::new(&args.id);
    let loaded = store
        .load_session(&session_id)
        .with_context(|| format!("loading session {}", args.id))?;

    if args.report_only {
        if let Some(md) = &loaded.session.final_report_markdown {
            println!("{md}");
        } else {
            eprintln!("(session has no final report)");
        }
        return Ok(());
    }

    match args.format {
        ShowFormat::Json => {
            let json = serde_json::to_string_pretty(&loaded).context("serialising session")?;
            println!("{json}");
        }
        ShowFormat::Pretty => render_pretty(&loaded),
    }
    Ok(())
}

fn render_pretty(loaded: &LoadedSession) {
    let s = &loaded.session;
    println!("── session {} ──", s.id);
    println!("target:        {}", s.target);
    println!("model:         {}", s.model);
    println!("status:        {}", s.status.as_str());
    if let Some(sr) = &s.stop_reason {
        println!("stop_reason:   {sr}");
    }
    println!("created_at:    {}", format_time(s.created_at));
    println!("updated_at:    {}", format_time(s.updated_at));
    println!("prompt_hash:   {}", &s.system_prompt_hash[..16]);
    if let Some(note) = &s.note {
        println!("note:          {note}");
    }
    if !s.stats.is_null() {
        println!("stats:         {}", s.stats);
    }
    println!();

    for turn in &loaded.turns {
        render_turn(turn);
        let calls: Vec<&ToolCallRecord> = loaded
            .tool_calls
            .iter()
            .filter(|tc| tc.turn_index == turn.turn_index)
            .collect();
        for call in calls {
            render_tool_call(call);
        }
        println!();
    }

    if let Some(md) = &s.final_report_markdown {
        println!("── final report ({}) ──", s.final_confidence.as_deref().unwrap_or("?"));
        println!("{md}");
    }
}

fn render_turn(t: &TurnRecord) {
    let role = match t.role {
        TurnRole::User => "user",
        TurnRole::Assistant => "assistant",
    };
    let tokens = match (t.tokens_in, t.tokens_out) {
        (Some(i), Some(o)) => format!("  (in={i}, out={o})"),
        _ => String::new(),
    };
    println!("── turn {} [{role}]{tokens} ──", t.turn_index);
    for block in t.content.as_array().into_iter().flatten() {
        print_block(block);
    }
}

fn print_block(block: &serde_json::Value) {
    let Some(obj) = block.as_object() else {
        return;
    };
    // ContentBlock is serialized with an internally-tagged "type" field
    // by the LLM crate. Don't assume shape — print what we can.
    if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
        println!("  text: {}", truncate_multiline(text, 500));
    } else if let (Some(name), Some(input)) = (
        obj.get("name").and_then(|v| v.as_str()),
        obj.get("input"),
    ) {
        println!("  tool_use: {name}({})", compact_json(input));
    } else if let Some(content) = obj.get("content").and_then(|v| v.as_str()) {
        let is_err = obj.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
        let tag = if is_err { "tool_result ERR" } else { "tool_result" };
        println!("  {tag}: {}", truncate_multiline(content, 300));
    } else {
        println!("  {block}");
    }
}

fn render_tool_call(tc: &ToolCallRecord) {
    let status = if tc.is_error { "ERROR" } else { "ok" };
    println!(
        "  ↳ {name}  {status}  ({ms}ms)",
        name = tc.tool_name,
        status = status,
        ms = tc.duration_ms,
    );
}

async fn run_resume(args: &ResumeArgs, config: &Config) -> Result<()> {
    let session_id = SessionId::new(&args.id);
    agent_runner::resume_agent(&session_id, &args.flags, args.force_prompt_change, config).await
}

fn run_delete(args: &DeleteArgs) -> Result<()> {
    let store = resolve_db(args.db.as_deref())?;
    let session_id = SessionId::new(&args.id);

    if !args.yes {
        // Confirm by loading and showing a short summary first, then
        // waiting on y/N input.
        let loaded = store
            .load_session(&session_id)
            .with_context(|| format!("loading session {}", args.id))?;
        eprintln!(
            "About to delete session {} (target={}, status={}, turns={}).",
            loaded.session.id,
            loaded.session.target,
            loaded.session.status.as_str(),
            loaded.turns.len(),
        );
        eprint!("Type 'y' to confirm: ");
        let _ = io::stderr().flush();
        let mut buf = [0u8; 2];
        let n = io::stdin().read(&mut buf).unwrap_or(0);
        let answer = std::str::from_utf8(&buf[..n]).unwrap_or("").trim();
        if answer != "y" && answer != "Y" {
            eprintln!("aborted");
            return Ok(());
        }
    }

    store
        .delete_session(&session_id)
        .with_context(|| format!("deleting session {}", args.id))?;
    println!("deleted {}", args.id);
    Ok(())
}

// ---- helpers ----------------------------------------------------------

fn short_id(id: &str) -> &str {
    id.get(..10).unwrap_or(id)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

fn truncate_multiline(s: &str, max: usize) -> String {
    let one_line = s.replace('\n', " ⏎ ");
    truncate(&one_line, max)
}

fn compact_json(v: &serde_json::Value) -> String {
    let s = serde_json::to_string(v).unwrap_or_else(|_| "<?>".into());
    truncate(&s, 200)
}

fn format_time(ts: std::time::SystemTime) -> String {
    // Compact ISO-ish without pulling in chrono. Seconds since epoch is
    // fine for a listing — operators can copy the id into `show` for
    // full detail.
    ts.duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("{}", d.as_secs()))
        .unwrap_or_else(|_| "0".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_id_takes_first_ten_chars_or_less() {
        assert_eq!(short_id("abcdef1234ghi"), "abcdef1234");
        assert_eq!(short_id("short"), "short");
    }

    #[test]
    fn truncate_preserves_short_strings_and_adds_ellipsis() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("0123456789abc", 8), "0123456…");
    }

    #[test]
    fn truncate_multiline_collapses_newlines() {
        let out = truncate_multiline("a\nb\nc", 10);
        assert!(out.contains("⏎"), "{out:?}");
        assert!(!out.contains('\n'));
    }
}
