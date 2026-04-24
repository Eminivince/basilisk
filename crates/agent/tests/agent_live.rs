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
    RECON_V1_PROMPT,
};
use basilisk_core::Config;
use basilisk_git::RepoCache;
use basilisk_github::GithubClient;
use basilisk_llm::{AnthropicBackend, LlmBackend, DEFAULT_MODEL};

/// Build a fully-loaded runner pointed at a scratch session DB +
/// scratch repo cache. Caller owns both tempdirs.
fn build_live_runner(
    config: &Config,
    db_dir: &tempfile::TempDir,
    cache_dir: &tempfile::TempDir,
    budget: Budget,
) -> AgentRunner {
    let api_key = config
        .anthropic_api_key
        .as_deref()
        .expect("ANTHROPIC_API_KEY set");
    let backend: Arc<dyn LlmBackend> = Arc::new(
        AnthropicBackend::with_model(api_key, DEFAULT_MODEL).expect("init anthropic"),
    );
    let store =
        Arc::new(SessionStore::open(db_dir.path().join("sessions.db")).expect("open session db"));
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
        RECON_V1_PROMPT,
        budget,
    )
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
    if config.anthropic_api_key.is_none() {
        eprintln!("skipping: ANTHROPIC_API_KEY not set");
        return None;
    }
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
            max_tokens_total: 300_000,
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
            max_tokens_total: 500_000,
            max_cost_cents: 300, // spec: under $3
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
