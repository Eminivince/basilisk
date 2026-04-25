//! Live end-to-end agent tests (`CP6e`).
//!
//! These run the real [`AnthropicBackend`] against three real targets:
//!
//!  - `foundry-rs/forge-template` — small Foundry repo (GitHub target),
//!  - USDC on Ethereum mainnet (a canonical proxy),
//!  - Aave V3 `Pool` on Ethereum mainnet (a complex diamond / library
//!    system).
//!
//! Every test is `#[ignore]`-d — they cost real money and need network
//! access. Run them explicitly:
//!
//! ```text
//! cargo test -p basilisk-agent --test agent_live -- --ignored --nocapture
//! ```
//!
//! Requirements at runtime:
//!  - `ANTHROPIC_API_KEY` (read via `Config::load()` → dotenv).
//!  - `ALCHEMY_API_KEY` or `RPC_URL_ETHEREUM` (for the on-chain tests).
//!  - Network reachability to `api.anthropic.com`, `api.github.com`,
//!    and whichever RPC endpoint resolves.
//!
//! The assertions are deliberately loose. LLMs are non-deterministic
//! and the shape of a recon brief will vary. We assert:
//!  - the run finalized (agent called `finalize_report`),
//!  - at least two tool calls happened (the model actually used tools),
//!  - the cost stayed under the spec's per-target cap.
//!
//! Content assertions ("mentions proxy", "mentions a library contract")
//! live in the higher-bar Aave test only — small targets won't always
//! warrant that vocabulary.
//!
//! [`AnthropicBackend`]: basilisk_llm::AnthropicBackend

use std::sync::Arc;
use std::time::Duration;

use basilisk_agent::{
    standard_registry, AgentRunner, AgentStopReason, Budget, NoopObserver, SessionStore,
    RECON_DEFAULT_PROMPT,
};
use basilisk_core::Config;
use basilisk_git::RepoCache;
use basilisk_github::GithubClient;
use basilisk_llm::{
    AnthropicBackend, LlmBackend, OpenAICompatibleBackend, Provider, DEFAULT_MODEL,
};

/// Which LLM provider the test should drive. Mirrors the CLI's
/// `ProviderKind` but lives here so the agent crate's live tests
/// don't need a CLI dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveProvider {
    Anthropic,
    Openrouter,
    Openai,
    Ollama,
    OpenaiCompat,
}

impl LiveProvider {
    fn from_env() -> Self {
        match std::env::var("BASILISK_LLM_PROVIDER").ok().as_deref() {
            Some("openrouter") => Self::Openrouter,
            Some("openai") => Self::Openai,
            Some("ollama") => Self::Ollama,
            Some("openai-compat") => Self::OpenaiCompat,
            Some("anthropic") | None => Self::Anthropic,
            Some(other) => panic!("unknown BASILISK_LLM_PROVIDER: {other}"),
        }
    }

    /// Env var name the live test looks for by default. Operators can
    /// point at a different one via `BASILISK_LLM_API_KEY_ENV`.
    fn default_key_var(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::Openrouter => "OPENROUTER_API_KEY",
            // openai + openai-compat share OPENAI_API_KEY as a default.
            Self::Openai | Self::OpenaiCompat => "OPENAI_API_KEY",
            Self::Ollama => "OLLAMA_API_KEY", // typically empty
        }
    }

    fn requires_key(self) -> bool {
        !matches!(self, Self::Ollama | Self::OpenaiCompat)
    }
}

/// Build a fully-loaded runner pointed at a scratch session DB +
/// scratch repo cache. Backend is selected from
/// `BASILISK_LLM_PROVIDER` (default: anthropic) and honours
/// `BASILISK_LLM_MODEL`, `BASILISK_LLM_BASE_URL`, and
/// `BASILISK_LLM_API_KEY_ENV` — the same env vars the CLI reads.
/// Caller owns both tempdirs.
fn build_live_runner(
    config: &Config,
    db_dir: &tempfile::TempDir,
    cache_dir: &tempfile::TempDir,
    budget: Budget,
) -> AgentRunner {
    let provider = LiveProvider::from_env();
    let key_var = std::env::var("BASILISK_LLM_API_KEY_ENV")
        .ok()
        .unwrap_or_else(|| provider.default_key_var().to_string());
    let api_key = std::env::var(&key_var).unwrap_or_default();
    let model = std::env::var("BASILISK_LLM_MODEL").ok();
    let base_url = std::env::var("BASILISK_LLM_BASE_URL").ok();

    let backend: Arc<dyn LlmBackend> = match provider {
        LiveProvider::Anthropic => {
            let model = model.as_deref().unwrap_or(DEFAULT_MODEL);
            Arc::new(AnthropicBackend::with_model(api_key, model).expect("init anthropic"))
        }
        LiveProvider::Openrouter => {
            let model = model.as_deref().unwrap_or("anthropic/claude-opus-4-7");
            Arc::new(
                OpenAICompatibleBackend::with_provider_and_model(
                    Provider::OpenRouter,
                    api_key,
                    model,
                )
                .expect("init openrouter"),
            )
        }
        LiveProvider::Openai => {
            let model = model.as_deref().unwrap_or("gpt-4o");
            Arc::new(
                OpenAICompatibleBackend::with_provider_and_model(Provider::OpenAi, api_key, model)
                    .expect("init openai"),
            )
        }
        LiveProvider::Ollama => {
            let model = model.as_deref().unwrap_or("llama3.1");
            let backend = match base_url.as_deref() {
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
            };
            Arc::new(backend.expect("init ollama"))
        }
        LiveProvider::OpenaiCompat => {
            let base = base_url
                .as_deref()
                .expect("BASILISK_LLM_BASE_URL is required for openai-compat");
            let model = model
                .as_deref()
                .expect("BASILISK_LLM_MODEL is required for openai-compat");
            Arc::new(
                OpenAICompatibleBackend::with_base_model_and_provider(
                    base,
                    api_key,
                    model,
                    Provider::Custom,
                )
                .expect("init openai-compat"),
            )
        }
    };

    let db_path = db_dir.path().join("sessions.db");
    let store = Arc::new(SessionStore::open(&db_path).expect("open session db"));
    let scratchpad_store = Arc::new(
        basilisk_scratchpad::ScratchpadStore::open(&db_path).expect("open scratchpad store"),
    );
    let github =
        Arc::new(GithubClient::new(config.github_token.as_deref()).expect("github client"));
    let repo_cache =
        Arc::new(RepoCache::open_at(cache_dir.path().to_path_buf()).expect("repo cache"));

    AgentRunner::new(
        backend,
        standard_registry(),
        store,
        Arc::new(config.clone()),
        github,
        repo_cache,
        RECON_DEFAULT_PROMPT,
        budget,
    )
    .with_scratchpad(scratchpad_store)
}

fn initial_message(target: &str) -> String {
    format!(
        "Target: {target}\n\n\
         Please perform reconnaissance. Classify the target, pull any sources that \
         exist, and investigate notable patterns. Call `finalize_report` when you have \
         enough to write a useful recon brief for a human reviewer."
    )
}

fn load_config_or_skip() -> Option<Config> {
    let config = Config::load().ok()?;
    let provider = LiveProvider::from_env();
    if provider.requires_key() {
        let key_var = std::env::var("BASILISK_LLM_API_KEY_ENV")
            .ok()
            .unwrap_or_else(|| provider.default_key_var().to_string());
        if std::env::var(&key_var)
            .unwrap_or_default()
            .trim()
            .is_empty()
        {
            eprintln!("skipping: {key_var} not set (BASILISK_LLM_PROVIDER={provider:?})");
            return None;
        }
    }
    eprintln!("live test provider: {provider:?}");
    Some(config)
}

fn report_outcome(label: &str, outcome: &basilisk_agent::AgentOutcome) {
    eprintln!("\n=== {label} — session {} ===", outcome.session_id);
    eprintln!(
        "stop={}, turns={}, tool_calls={}, tokens={}, cost={}¢, duration={}ms",
        outcome.stop_reason.tag(),
        outcome.stats.turns,
        outcome.stats.tool_calls,
        outcome.stats.total_tokens(),
        outcome.stats.cost_cents,
        outcome.stats.duration_ms,
    );
    if let Some(report) = &outcome.final_report {
        eprintln!("--- final report ({:?}) ---", report.confidence);
        eprintln!("{}", report.markdown);
        eprintln!("--- end report ---\n");
    } else {
        eprintln!("(no final report)");
    }
}

#[ignore = "live: hits real Anthropic API + GitHub; costs money"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_live_forge_template_github() {
    let Some(config) = load_config_or_skip() else {
        return;
    };

    let db_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let runner = build_live_runner(
        &config,
        &db_dir,
        &cache_dir,
        Budget {
            max_turns: 20,
            max_tokens_total: 200_000,
            max_cost_cents: 100, // spec: under $1
            max_duration: Duration::from_secs(600),
        },
    );

    let target = "https://github.com/foundry-rs/forge-template";
    let outcome = runner
        .run_with_observer(target, initial_message(target), None, &NoopObserver)
        .await
        .expect("agent run ok");

    report_outcome("forge-template", &outcome);

    assert!(
        matches!(outcome.stop_reason, AgentStopReason::ReportFinalized),
        "expected report_finalized, got {:?}",
        outcome.stop_reason,
    );
    assert!(
        outcome.stats.tool_calls >= 2,
        "expected >=2 tool calls, got {}",
        outcome.stats.tool_calls,
    );
    assert!(
        outcome.stats.cost_cents <= 100,
        "cost blew past $1: {}¢",
        outcome.stats.cost_cents,
    );
    let report = outcome.final_report.expect("finalized");
    assert!(!report.markdown.trim().is_empty());
}

#[ignore = "live: hits real Anthropic API + real RPC; costs money"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_live_usdc_mainnet_proxy() {
    let Some(config) = load_config_or_skip() else {
        return;
    };

    let db_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let runner = build_live_runner(
        &config,
        &db_dir,
        &cache_dir,
        Budget {
            max_turns: 25,
            // USDC's proxy + FiatTokenV2_2 implementation is ~280k
            // tokens on a typical OpenRouter run (no prompt caching);
            // 500k gives comfortable headroom for one nudge turn.
            max_tokens_total: 500_000,
            max_cost_cents: 150, // USDC is proxy-heavy; a bit more headroom than $1
            max_duration: Duration::from_secs(900),
        },
    );

    let target = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
    let outcome = runner
        .run_with_observer(target, initial_message(target), None, &NoopObserver)
        .await
        .expect("agent run ok");

    report_outcome("USDC", &outcome);

    assert!(
        matches!(outcome.stop_reason, AgentStopReason::ReportFinalized),
        "expected report_finalized, got {:?}",
        outcome.stop_reason,
    );
    assert!(outcome.stats.tool_calls >= 2);
    assert!(
        outcome.stats.cost_cents <= 150,
        "cost: {}¢",
        outcome.stats.cost_cents,
    );
    let report = outcome.final_report.expect("finalized");
    assert!(!report.markdown.trim().is_empty());
}

#[ignore = "live: hits real Anthropic API + real RPC; costs real money (>$1 expected)"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_live_aave_v3_pool_mainnet() {
    let Some(config) = load_config_or_skip() else {
        return;
    };

    let db_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let runner = build_live_runner(
        &config,
        &db_dir,
        &cache_dir,
        Budget {
            max_turns: 40,
            // Aave V3 Pool is a proxy fronting a ~9-library diamond
            // -like implementation — resolving the full system + the
            // Sourcify source produces tool results north of 200k
            // tokens each, and the conversation history accumulates
            // across turns. On OpenRouter (no Anthropic prompt
            // caching passthrough) a typical recon run burns ~600k
            // tokens; 1M gives room for one nudge + investigative
            // headroom. Native Anthropic with prompt caching enabled
            // would hit the spec's $3 target — this budget matches
            // the OpenRouter reality.
            max_tokens_total: 1_000_000,
            max_cost_cents: 300, // spec: under $3 (native Anthropic assumption)
            max_duration: Duration::from_secs(1200),
        },
    );

    // Aave V3 Pool proxy on Ethereum mainnet.
    let target = "0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2";
    let outcome = runner
        .run_with_observer(target, initial_message(target), None, &NoopObserver)
        .await
        .expect("agent run ok");

    report_outcome("Aave V3 Pool", &outcome);

    assert!(
        matches!(outcome.stop_reason, AgentStopReason::ReportFinalized),
        "expected report_finalized, got {:?}",
        outcome.stop_reason,
    );
    assert!(outcome.stats.tool_calls >= 3);
    assert!(
        outcome.stats.cost_cents <= 300,
        "cost: {}¢",
        outcome.stats.cost_cents,
    );
    let report = outcome.final_report.expect("finalized");
    let md = report.markdown.to_lowercase();
    // Loose content assertions — Aave V3 is proxy-shaped, so any
    // competent recon mentions either term.
    assert!(
        md.contains("proxy") || md.contains("implementation"),
        "expected proxy/implementation vocabulary in Aave report",
    );
}

/// Set 8 / CP8.8 live test.
///
/// End-to-end exercise of working memory against the smallest
/// target. Asserts the scratchpad was actually populated — not just
/// that the tools exist. Prints the full markdown render so the
/// set-8 report can include what the agent wrote.
///
/// Budget stays the same as the forge-template test (under $1). The
/// assertion shape is deliberately loose: we don't know exactly how
/// the model will organize its thinking, only that at least one of
/// the seven tracking sections should have content by the end of a
/// recon.
#[ignore = "live: hits real LLM API + GitHub; costs money"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scratchpad_live_forge_template_writes_working_memory() {
    let Some(config) = load_config_or_skip() else {
        return;
    };

    let db_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("sessions.db");

    let runner = build_live_runner(
        &config,
        &db_dir,
        &cache_dir,
        Budget {
            max_turns: 20,
            max_tokens_total: 200_000,
            max_cost_cents: 100,
            max_duration: Duration::from_secs(600),
        },
    );

    let target = "https://github.com/foundry-rs/forge-template";
    let outcome = runner
        .run_with_observer(
            target,
            scratchpad_initial_message(target),
            None,
            &NoopObserver,
        )
        .await
        .expect("agent run ok");

    report_outcome("forge-template + scratchpad", &outcome);

    assert!(
        matches!(outcome.stop_reason, AgentStopReason::ReportFinalized),
        "expected report_finalized, got {:?}",
        outcome.stop_reason,
    );
    assert!(outcome.stats.tool_calls >= 2);

    // Inspect the scratchpad the agent left behind.
    let pads =
        basilisk_scratchpad::ScratchpadStore::open(&db_path).expect("reopen scratchpad store");
    let sp = pads
        .load(outcome.session_id.as_str())
        .expect("load scratchpad")
        .expect("scratchpad present after live run");

    eprintln!("\n=== scratchpad after live run ===");
    eprintln!("{}", basilisk_scratchpad::render_markdown(&sp));
    eprintln!("=== end scratchpad ===\n");

    // The prompt explicitly encourages scratchpad use — assert at
    // least one section beyond the default prose block has content.
    let any_section_populated = sp.sections.iter().any(|(key, section)| {
        let has_content = match section {
            basilisk_scratchpad::Section::Prose(p) => !p.markdown.trim().is_empty(),
            basilisk_scratchpad::Section::Items(i) => !i.items.is_empty(),
        };
        if has_content {
            eprintln!("  populated: {}", key.wire_name());
        }
        has_content
    });
    assert!(
        any_section_populated,
        "expected agent to write at least one scratchpad section; none were populated",
    );

    // Scratchpad should have saved at least once (create + at
    // minimum one write → ≥ 2 revisions).
    let revs = pads
        .list_revisions(outcome.session_id.as_str())
        .expect("list revisions");
    assert!(
        revs.len() >= 2,
        "expected ≥2 revisions (create + at least one write), got {}",
        revs.len(),
    );
}

fn scratchpad_initial_message(target: &str) -> String {
    format!(
        "Target: {target}\n\n\
         Please perform reconnaissance on this target. You have a scratchpad available — \
         use it. As you learn about the project, write a paragraph into `system_understanding` \
         describing how it's organised. When something about the project prompts a question \
         you can't immediately answer, append to `open_questions`. If you notice anything \
         surprising or suspicious (even if unprovable during recon), append to \
         `suspicions_not_yet_confirmed`. If a tool can't tell you something you wanted to \
         know, append to `limitations_noticed`. Call `finalize_report` when you have enough \
         to write a useful recon brief — referring back to your scratchpad as you synthesise."
    )
}
