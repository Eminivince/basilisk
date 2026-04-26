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
    AgentStopReason, Budget, LoadedSession, NoopObserver, NudgeEvent, NudgeKind, SessionId,
    SessionStore, RECON_DEFAULT_PROMPT,
};
use basilisk_core::Config;
use basilisk_git::RepoCache;
use basilisk_github::GithubClient;
use basilisk_llm::{
    AnthropicBackend, LlmBackend, Message, MessageRole, OpenAICompatibleBackend, Provider,
    DEFAULT_MODEL, DEFAULT_VULN_MODEL,
};
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

/// Which LLM backend to drive.
///
///  - `anthropic` — native `api.anthropic.com/v1/messages` (default).
///  - `openrouter` — `openrouter.ai/api/v1/chat/completions`, any model
///    `OpenRouter` proxies. Requires `OPENROUTER_API_KEY` or
///    `--llm-api-key-env <VAR>`.
///  - `openai` — `api.openai.com/v1/chat/completions`. Requires
///    `OPENAI_API_KEY`.
///  - `ollama` — `http://localhost:11434/v1/chat/completions`. No
///    API key required by default.
///  - `openai-compat` — custom `OpenAI`-compatible endpoint. Supply
///    `--llm-base-url <url>` and optionally `--llm-api-key-env <VAR>`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum ProviderKind {
    #[default]
    Anthropic,
    Openrouter,
    Openai,
    Ollama,
    #[value(name = "openai-compat")]
    OpenaiCompat,
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
#[derive(Debug, Args, Default, Clone)]
pub struct AgentFlags {
    /// Free-form note attached to the session row.
    #[arg(long = "session-note", id = "agent_session_note", value_name = "TEXT")]
    pub note: Option<String>,

    /// Path to the session database. Defaults to `~/.basilisk/sessions.db`.
    #[arg(
        long = "db",
        id = "agent_db",
        value_name = "PATH",
        env = "BASILISK_SESSION_DB"
    )]
    pub db: Option<PathBuf>,

    /// Path to a file containing the system prompt. Overrides the
    /// embedded default (`recon_v2.md`). Two shipped versions live in
    /// `crates/agent/src/prompts/`:
    ///
    ///  - `recon_v2.md` — current default (set-6.5): tighter report
    ///    style, length ceilings, no-boilerplate rule.
    ///  - `recon_v1.md` — original set-6 prompt, kept for comparison.
    ///    Point at it to reproduce older runs.
    ///
    /// You can also point at a working copy of either file to iterate
    /// on the prompt without rebuilding the binary.
    #[arg(
        long = "system-prompt",
        id = "agent_system_prompt",
        value_name = "PATH",
        env = "BASILISK_SYSTEM_PROMPT"
    )]
    pub system_prompt: Option<PathBuf>,

    /// Model id. Meaning depends on `--provider`:
    ///
    ///  - anthropic: `claude-opus-4-7`, `claude-sonnet-4-6`, …
    ///  - openrouter: `anthropic/claude-opus-4-7`, `openai/gpt-4o`,
    ///    `meta-llama/llama-3.1-70b-instruct`, …
    ///  - openai: `gpt-4o`, `gpt-4o-mini`, …
    ///  - ollama / openai-compat: whatever the server exposes, e.g.
    ///    `llama3.1:70b`, `qwen2.5-coder:32b`.
    #[arg(
        long = "model",
        id = "agent_model",
        value_name = "MODEL",
        env = "BASILISK_LLM_MODEL"
    )]
    pub model: Option<String>,

    /// Which LLM backend to drive. Default: `anthropic`.
    #[arg(
        long = "provider",
        id = "agent_provider",
        value_enum,
        default_value_t = ProviderKind::Anthropic,
        value_name = "PROVIDER",
        env = "BASILISK_LLM_PROVIDER",
    )]
    pub provider: ProviderKind,

    /// Override the API base URL. Only meaningful with
    /// `--provider openai-compat` (default: none) and with
    /// `--provider ollama` (default: `http://localhost:11434/v1`).
    /// Ignored for `anthropic`/`openrouter`/`openai` — those have
    /// fixed endpoints.
    #[arg(
        long = "llm-base-url",
        id = "agent_llm_base_url",
        value_name = "URL",
        env = "BASILISK_LLM_BASE_URL"
    )]
    pub llm_base_url: Option<String>,

    /// Name of the env var to read the API key from. Defaults per
    /// provider: `ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY`,
    /// `OPENAI_API_KEY`. Ignored for ollama (no key required). Use
    /// this when you have a custom env var name or want to pick
    /// between keys at runtime.
    #[arg(
        long = "llm-api-key-env",
        id = "agent_llm_api_key_env",
        value_name = "VAR",
        env = "BASILISK_LLM_API_KEY_ENV"
    )]
    pub llm_api_key_env: Option<String>,

    /// Max LLM turns.
    #[arg(
        long = "max-turns",
        id = "agent_max_turns",
        value_name = "N",
        env = "BASILISK_MAX_TURNS"
    )]
    pub max_turns: Option<u32>,

    /// Max total tokens (input + output + cache).
    #[arg(
        long = "max-tokens",
        id = "agent_max_tokens",
        value_name = "N",
        env = "BASILISK_MAX_TOKENS"
    )]
    pub max_tokens: Option<u64>,

    /// Max estimated spend in cents.
    #[arg(
        long = "max-cost",
        id = "agent_max_cost",
        value_name = "CENTS",
        env = "BASILISK_MAX_COST_CENTS"
    )]
    pub max_cost_cents: Option<u32>,

    /// Max wall-clock duration in seconds (agent only).
    #[arg(
        long = "agent-max-duration",
        id = "agent_max_duration",
        value_name = "SECS",
        env = "BASILISK_MAX_DURATION_SECS"
    )]
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
    #[arg(long = "no-stream", id = "agent_no_stream", env = "BASILISK_NO_STREAM")]
    pub no_stream: bool,

    /// Vulnerability-reasoning mode. Swaps the registry to
    /// `vuln_registry` (25 tools: recon + knowledge + analytical +
    /// self-critique), swaps the default system prompt to
    /// `vuln_v2.md` (Set 9.5 — strengthened structured-recording
    /// discipline), bumps the default turn budget to 100, defaults
    /// the model to Claude Sonnet 4.6 (cheaper than Opus; opt into
    /// Opus via `--model claude-opus-4-7` for high-stakes targets),
    /// and wires an anvil-backed execution backend for
    /// `simulate_call_chain` and `build_and_run_foundry_test`.
    /// Requires Foundry on PATH and a reachable mainnet archive RPC
    /// (see `ALCHEMY_API_KEY` / `MAINNET_RPC_URL` in `.env.example`).
    /// Knowledge-base retrieval is wired when an embedding provider
    /// is configured; missing knowledge degrades the tools to typed
    /// errors but the run continues.
    #[arg(long, id = "agent_vuln")]
    pub vuln: bool,
}

/// Entry point: run the agent against `target` using `flags`.
///
/// Called from `commands::recon::run` when `--agent` is set.
/// Variant of [`run_agent`] that returns the `AgentOutcome` instead
/// of just rendering it. Used by `audit bench run` which needs the
/// outcome to score + persist.
pub async fn run_agent_with_outcome(
    target: &str,
    flags: &AgentFlags,
    config: &Config,
) -> Result<AgentOutcome> {
    let (mut runner, db_path) = build_runner(flags, config)?;

    if flags.vuln {
        match super::knowledge::open_kb(config).await {
            Ok(kb) => {
                runner = runner.with_knowledge(Arc::new(kb));
                eprintln!("  vuln-mode: knowledge base attached");
            }
            Err(e) => {
                eprintln!(
                    "  vuln-mode: knowledge base unavailable — knowledge tools will \
                     return errors but run continues ({e})"
                );
            }
        }
        if let Err(e) = basilisk_exec::AnvilForkBackend::require_binary() {
            eprintln!("  vuln-mode: anvil binary check — {e}");
        }
    }

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
            build_initial_message_for(target, flags.note.as_deref(), flags.vuln),
            flags.note.clone(),
            observer,
        )
        .await
        .context("agent run failed")?;
    render_outcome(&outcome, flags.output);
    Ok(outcome)
}

pub async fn run_agent(target: &str, flags: &AgentFlags, config: &Config) -> Result<()> {
    let (mut runner, db_path) = build_runner(flags, config)?;

    // Vuln-mode knowledge wiring happens here (async) so the whole
    // build path can stay sync in build_runner.
    if flags.vuln {
        match super::knowledge::open_kb(config).await {
            Ok(kb) => {
                runner = runner.with_knowledge(Arc::new(kb));
                eprintln!("  vuln-mode: knowledge base attached");
            }
            Err(e) => {
                eprintln!(
                    "  vuln-mode: knowledge base unavailable — knowledge tools will \
                     return errors but run continues ({e})"
                );
            }
        }
        if let Err(e) = basilisk_exec::AnvilForkBackend::require_binary() {
            eprintln!("  vuln-mode: anvil binary check — {e}");
        }
    }

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
            build_initial_message_for(target, flags.note.as_deref(), flags.vuln),
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
    let backend = build_backend(flags, config)?;

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
        tracing::info!(
            count = swept,
            "marked stale running sessions as interrupted"
        );
    }

    let system_prompt = load_system_prompt(flags.system_prompt.as_deref())?;
    let github = Arc::new(
        GithubClient::new(config.github_token.as_deref()).context("initialising GitHub client")?,
    );
    let repo_cache = Arc::new(RepoCache::open().context("opening repo cache")?);

    // Open the scratchpad store against the same SQLite file so
    // `audit session scratchpad show <id>` resolves by default
    // without `--db`. The SessionStore migration already created
    // the scratchpad tables, so this open is just a handle.
    let scratchpad_store = Arc::new(
        basilisk_scratchpad::ScratchpadStore::open(&db_path).context("opening scratchpad store")?,
    );

    // Registry + prompt selection depends on `--vuln`. Recon flows keep
    // standard_registry + recon_v2.md by default. Vuln flows use
    // vuln_registry (25 tools) and VULN_V3_PROMPT unless the operator
    // overrode via `--system-prompt` / BASILISK_SYSTEM_PROMPT.
    //
    // v3 (current) is the adversarial-mode mandate: drainage-only,
    // mandatory PoC for every finding, explicit out-of-scope categories.
    // v2 (Set 9.5) is broader-scope vulnerability hunting with
    // structured-recording discipline; v1 (Set 9) is the original.
    // Both kept in-tree for A/B comparison.
    let (registry, system_prompt_selected) = if flags.vuln {
        let prompt = if flags.system_prompt.is_some() {
            system_prompt
        } else {
            basilisk_agent::VULN_V3_PROMPT.to_string()
        };
        (basilisk_agent::vuln_registry(), prompt)
    } else {
        (standard_registry(), system_prompt)
    };

    let mut runner = AgentRunner::new(
        backend,
        registry,
        Arc::clone(&store),
        Arc::new(config.clone()),
        github,
        repo_cache,
        system_prompt_selected,
        build_budget(flags),
    )
    .with_scratchpad(scratchpad_store);

    // Vuln runs always get an anvil-backed exec so simulate_call_chain
    // and build_and_run_foundry_test have a backend. Missing Foundry
    // is surfaced at session-start in run_agent_with_outcome.
    if flags.vuln {
        let exec: Arc<dyn basilisk_exec::ExecutionBackend> =
            Arc::new(basilisk_exec::AnvilForkBackend::new());
        runner = runner.with_exec(exec);
    }

    Ok((runner, db_path))
}

/// Construct the LLM backend dictated by `flags.provider` + its
/// provider-specific inputs (env var, base URL, model).
///
/// Lookup order for the API key, by provider:
///  - anthropic: `--llm-api-key-env` → `ANTHROPIC_API_KEY`.
///  - openrouter: `--llm-api-key-env` → `OPENROUTER_API_KEY`.
///  - openai: `--llm-api-key-env` → `OPENAI_API_KEY`.
///  - ollama: not required; any non-empty value is accepted if passed.
///  - openai-compat: `--llm-api-key-env` (defaults to `OPENAI_API_KEY`).
// The function is a flat dispatch — one arm per ProviderKind. Splitting
// each arm into its own helper would fragment the provider-selection
// logic across five functions that share no interesting state, so we
// accept the length over the refactor.
#[allow(clippy::too_many_lines)]
fn build_backend(flags: &AgentFlags, config: &Config) -> Result<Arc<dyn LlmBackend>> {
    fn resolve_key(
        flags: &AgentFlags,
        config: &Config,
        default_var: &str,
        config_field: Option<&str>,
    ) -> Option<String> {
        if let Some(var) = flags.llm_api_key_env.as_deref() {
            return non_empty_env(var);
        }
        // Prefer the parsed Config field (covers dotenv) before falling back
        // to a direct env lookup on the default var name.
        match config_field {
            Some("anthropic") => config
                .anthropic_api_key
                .clone()
                .or_else(|| non_empty_env(default_var)),
            Some("openrouter") => config
                .openrouter_api_key
                .clone()
                .or_else(|| non_empty_env(default_var)),
            Some("openai") => config
                .openai_api_key
                .clone()
                .or_else(|| non_empty_env(default_var)),
            _ => non_empty_env(default_var),
        }
    }

    // Set 9.5 / CP9.5.2: --vuln defaults to Sonnet, recon defaults to
    // Opus. Operators override via --model. Cost dynamics on a typical
    // vuln run with Opus are ~$25; with Sonnet, ~$3-5 — calibrating
    // whether the quality tradeoff is worth keeping Sonnet as default.
    let anthropic_default = if flags.vuln {
        DEFAULT_VULN_MODEL
    } else {
        DEFAULT_MODEL
    };
    let openrouter_default = if flags.vuln {
        "anthropic/claude-sonnet-4-6"
    } else {
        "anthropic/claude-opus-4-7"
    };

    match flags.provider {
        ProviderKind::Anthropic => {
            let api_key = resolve_key(flags, config, "ANTHROPIC_API_KEY", Some("anthropic"))
                .context(
                "Anthropic API key is not set — export ANTHROPIC_API_KEY (or --llm-api-key-env)",
            )?;
            let model = flags.model.as_deref().unwrap_or(anthropic_default);
            let backend = AnthropicBackend::with_model(api_key, model)
                .context("initialising Anthropic backend")?;
            Ok(Arc::new(backend))
        }
        ProviderKind::Openrouter => {
            let api_key = resolve_key(flags, config, "OPENROUTER_API_KEY", Some("openrouter"))
                .context(
                "OpenRouter API key is not set — export OPENROUTER_API_KEY (or --llm-api-key-env)",
            )?;
            let model = flags.model.as_deref().unwrap_or(openrouter_default);
            let backend = OpenAICompatibleBackend::with_provider_and_model(
                Provider::OpenRouter,
                api_key,
                model,
            )
            .context("initialising OpenRouter backend")?;
            Ok(Arc::new(backend))
        }
        ProviderKind::Openai => {
            let api_key = resolve_key(flags, config, "OPENAI_API_KEY", Some("openai")).context(
                "OpenAI API key is not set — export OPENAI_API_KEY (or --llm-api-key-env)",
            )?;
            let model = flags.model.as_deref().unwrap_or("gpt-4o");
            let backend =
                OpenAICompatibleBackend::with_provider_and_model(Provider::OpenAi, api_key, model)
                    .context("initialising OpenAI backend")?;
            Ok(Arc::new(backend))
        }
        ProviderKind::Ollama => {
            // Ollama accepts an empty key. An override is allowed if the
            // operator has put a proxy in front of it.
            let api_key = resolve_key(flags, config, "OLLAMA_API_KEY", None).unwrap_or_default();
            let model = flags.model.as_deref().unwrap_or("llama3.1");
            let backend = match flags.llm_base_url.as_deref() {
                Some(base) => OpenAICompatibleBackend::with_base_model_and_provider(
                    base,
                    api_key,
                    model,
                    Provider::Ollama,
                ),
                None => OpenAICompatibleBackend::with_provider_and_model(
                    Provider::Ollama,
                    api_key,
                    model,
                ),
            }
            .context("initialising Ollama backend")?;
            Ok(Arc::new(backend))
        }
        ProviderKind::OpenaiCompat => {
            let base = flags
                .llm_base_url
                .as_deref()
                .context("--llm-base-url is required with --provider openai-compat")?;
            let api_key =
                resolve_key(flags, config, "OPENAI_API_KEY", Some("openai")).unwrap_or_default();
            let model = flags
                .model
                .as_deref()
                .context("--model is required with --provider openai-compat")?;
            let backend = OpenAICompatibleBackend::with_base_model_and_provider(
                base,
                api_key,
                model,
                Provider::Custom,
            )
            .context("initialising openai-compat backend")?;
            Ok(Arc::new(backend))
        }
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn build_budget(flags: &AgentFlags) -> Budget {
    let mut b = Budget::default();
    // Vuln-mode defaults are more generous — Set 9's vuln reasoning
    // expects 30-100 turns on the Investigation phase alone. These
    // are ceilings; operators override via --max-*.
    if flags.vuln {
        b.max_turns = 100;
        b.max_tokens_total = 2_000_000;
        b.max_cost_cents = 5_000; // $50
        b.max_duration = Duration::from_secs(60 * 60); // 1h
    }
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
        return Ok(RECON_DEFAULT_PROMPT.to_string());
    };
    std::fs::read_to_string(p)
        .with_context(|| format!("reading system prompt from {}", p.display()))
}

/// Picks the recon vs vuln framing. The `--vuln` path
/// from CP9.12 needs the user-role message to actually ask for
/// vulnerability hunting; otherwise the agent (correctly) follows
/// the literal "perform reconnaissance" wording even though the
/// system prompt is `vuln_v1.md`.
fn build_initial_message_for(target: &str, note: Option<&str>, vuln: bool) -> String {
    let mut msg = if vuln {
        format!(
            "Target: {target}\n\n\
             Hunt for vulnerabilities in this target. Read the system prompt's three-phase \
             discipline and follow it: build the model (Discovery), test specific hypotheses \
             with the analytical and exec tools (Investigation), then synthesize.\n\n\
             Use the tools liberally — `find_callers_of` and `trace_state_dependencies` to \
             pair external calls with state effects; `simulate_call_chain` to spot-check \
             attack sequences against a forked block; `build_and_run_foundry_test` to upgrade \
             a strong suspicion into a Confirmed finding. Call `record_suspicion` for every \
             hunch you can't confirm, `record_limitation` for every wall you hit. Call \
             `finalize_self_critique` then `finalize_report` only when you've actually done \
             the investigation, not just described the architecture."
        )
    } else {
        format!(
            "Target: {target}\n\n\
             Please perform reconnaissance. Classify the target, pull any sources that exist, \
             and investigate notable patterns. Call `finalize_report` when you have enough \
             to write a useful recon brief for a human reviewer."
        )
    };
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
    println!(
        "stop_reason: {}",
        describe_stop_reason(&outcome.stop_reason)
    );
    println!(
        "stats: {} turns, {} tool calls, {} tokens, ~{}¢, {}ms{}",
        outcome.stats.turns,
        outcome.stats.tool_calls,
        outcome.stats.total_tokens(),
        outcome.stats.cost_cents,
        outcome.stats.duration_ms,
        if outcome.stats.nudge_count > 0 {
            format!(", {} nudge events", outcome.stats.nudge_count)
        } else {
            String::new()
        },
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

    fn on_nudge_fired(&self, event: NudgeEvent) {
        // Clear any mid-text state so the warning lands on its own
        // line even if the model was streaming prose when we cut in.
        let had_text = {
            let mut s = self.state.lock().expect("pretty-observer state poisoned");
            let was = s.text_this_turn;
            s.text_this_turn = false;
            was
        };
        if had_text {
            Self::write_line("");
        }
        let kind = match event.kind {
            NudgeKind::SoftPrompt => "soft prompt",
            NudgeKind::ForceToolChoice => "forcing tool call on next turn",
        };
        Self::write_line(&format!(
            "  ⚠ runner nudge (consecutive text-ends: {}): {}",
            event.consecutive_text_ends, kind,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags_fixture() -> AgentFlags {
        AgentFlags {
            note: None,
            db: None,
            system_prompt: None,
            model: None,
            provider: ProviderKind::Anthropic,
            llm_base_url: None,
            llm_api_key_env: None,
            max_turns: None,
            max_tokens: None,
            max_cost_cents: None,
            max_duration_secs: None,
            output: OutputFormat::Pretty,
            no_stream: false,
            vuln: false,
        }
    }

    #[test]
    fn budget_overrides_apply_on_top_of_defaults() {
        let mut flags = flags_fixture();
        flags.max_turns = Some(5);
        flags.max_cost_cents = Some(100);
        let b = build_budget(&flags);
        assert_eq!(b.max_turns, 5);
        assert_eq!(b.max_cost_cents, 100);
        assert_eq!(b.max_tokens_total, Budget::default().max_tokens_total);
        assert_eq!(b.max_duration, Budget::default().max_duration);
    }

    fn err_string(r: Result<Arc<dyn LlmBackend>>) -> String {
        match r {
            Ok(_) => panic!("expected error, got Ok"),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn build_backend_anthropic_requires_key() {
        let flags = flags_fixture();
        let msg = err_string(build_backend(&flags, &Config::default()));
        assert!(msg.contains("Anthropic API key"), "got: {msg}");
    }

    #[test]
    fn build_backend_openrouter_uses_openrouter_key_field() {
        let mut flags = flags_fixture();
        flags.provider = ProviderKind::Openrouter;
        flags.model = Some("anthropic/claude-opus-4-7".into());
        let config = Config {
            openrouter_api_key: Some("sk-or-test".into()),
            ..Config::default()
        };
        let backend = build_backend(&flags, &config).expect("builds");
        assert_eq!(backend.identifier(), "openrouter/anthropic/claude-opus-4-7");
    }

    #[test]
    fn build_backend_ollama_works_without_any_api_key() {
        let mut flags = flags_fixture();
        flags.provider = ProviderKind::Ollama;
        flags.model = Some("qwen2.5-coder:32b".into());
        let backend = build_backend(&flags, &Config::default()).expect("builds");
        assert_eq!(backend.identifier(), "ollama/qwen2.5-coder:32b");
    }

    #[test]
    fn build_backend_openai_compat_requires_base_url() {
        let mut flags = flags_fixture();
        flags.provider = ProviderKind::OpenaiCompat;
        flags.model = Some("some-model".into());
        let msg = err_string(build_backend(&flags, &Config::default()));
        assert!(msg.contains("--llm-base-url"), "got: {msg}");
    }

    #[test]
    fn build_backend_openai_compat_requires_model() {
        let mut flags = flags_fixture();
        flags.provider = ProviderKind::OpenaiCompat;
        flags.llm_base_url = Some("http://localhost:8080/v1".into());
        let msg = err_string(build_backend(&flags, &Config::default()));
        assert!(msg.contains("--model"), "got: {msg}");
    }

    #[test]
    fn initial_message_embeds_target_and_note() {
        let msg = build_initial_message_for("eth/0xdead", Some("trusted author"), false);
        assert!(msg.contains("eth/0xdead"));
        assert!(msg.contains("finalize_report"));
        assert!(msg.contains("trusted author"));
    }

    #[test]
    fn initial_message_omits_note_section_when_absent() {
        let msg = build_initial_message_for("x", None, false);
        assert!(!msg.contains("Operator note"));
    }

    #[test]
    fn initial_message_vuln_branch_asks_for_vulnerability_hunting() {
        let recon = build_initial_message_for("0xabc", None, false);
        let vuln = build_initial_message_for("0xabc", None, true);
        assert!(recon.contains("recon"));
        assert!(!vuln.contains("perform reconnaissance"));
        // Vuln branch should reference the hunt + the discipline tools.
        assert!(vuln.contains("Hunt for vulnerabilities"));
        assert!(vuln.contains("record_suspicion"));
        assert!(vuln.contains("record_limitation"));
        assert!(vuln.contains("finalize_self_critique"));
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
    fn default_prompt_falls_back_to_recon_default_when_no_override() {
        let prompt = load_system_prompt(None).unwrap();
        assert_eq!(prompt, RECON_DEFAULT_PROMPT);
    }

    #[test]
    fn short_truncates_hashes_to_twelve_chars() {
        assert_eq!(short("abcdef0123456789"), "abcdef012345");
        assert_eq!(short("shorty"), "shorty");
    }
}
