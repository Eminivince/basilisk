//! [`AgentRunner`] skeleton — `CP5b`.
//!
//! Holds every dependency the loop needs (`CP5c`) and exposes the
//! budget-enforcement helpers + a cost estimator. No `run()` body yet:
//! adding it now would force the test surface to grow before we have a
//! mock backend (`CP5d`), which is the kind of "do too much in one go"
//! we're explicitly avoiding.

// CP5b lays the helpers; CP5c calls them from the loop body. Until
// then the fields/helpers look unused to rustc.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use basilisk_core::Config;
use basilisk_git::RepoCache;
use basilisk_github::GithubClient;
use basilisk_llm::{LlmBackend, PricingTable, TokenUsage};

use crate::runner::types::{AgentStats, AgentStopReason, Budget};
use crate::session::SessionStore;
use crate::tool::{SessionId, ToolContext, ToolRegistry};

/// Drives one tool-use loop against an [`LlmBackend`].
///
/// Construct once per process (or re-use across runs); each call to
/// `run()` (`CP5c`) mints a fresh session id and writes its transcript
/// to the supplied [`SessionStore`].
///
/// `system_prompt` is hashed by the loop and stored on the session row
/// so the CLI can detect "you changed the prompt mid-run" on resume.
pub struct AgentRunner {
    pub(crate) backend: Arc<dyn LlmBackend>,
    pub(crate) registry: ToolRegistry,
    pub(crate) store: Arc<SessionStore>,
    pub(crate) config: Arc<Config>,
    pub(crate) github: Arc<GithubClient>,
    pub(crate) repo_cache: Arc<RepoCache>,
    pub(crate) system_prompt: String,
    pub(crate) budget: Budget,
}

impl AgentRunner {
    /// Build a runner. Every dependency is `Arc`-shared so the same
    /// runner can drive many sequential sessions cheaply.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        backend: Arc<dyn LlmBackend>,
        registry: ToolRegistry,
        store: Arc<SessionStore>,
        config: Arc<Config>,
        github: Arc<GithubClient>,
        repo_cache: Arc<RepoCache>,
        system_prompt: impl Into<String>,
        budget: Budget,
    ) -> Self {
        Self {
            backend,
            registry,
            store,
            config,
            github,
            repo_cache,
            system_prompt: system_prompt.into(),
            budget,
        }
    }

    /// Backend identifier — recorded on the session row so a later
    /// `audit session resume` can detect model drift.
    pub fn model_identifier(&self) -> &str {
        self.backend.identifier()
    }

    /// Configured budget. Read-only; if you need a different budget,
    /// build a new runner.
    pub fn budget(&self) -> Budget {
        self.budget
    }

    /// Number of tools the agent can choose from.
    pub fn tool_count(&self) -> usize {
        self.registry.len()
    }

    /// Build the [`ToolContext`] for one in-flight session. Called by
    /// the loop (`CP5c`); exposed `pub(crate)` so tests can drive it.
    pub(crate) fn build_context(&self, session_id: SessionId) -> ToolContext {
        ToolContext {
            config: Arc::clone(&self.config),
            github: Arc::clone(&self.github),
            repo_cache: Arc::clone(&self.repo_cache),
            session_id,
        }
    }

    /// Estimate the cost in cents for one batch of token usage. Looks
    /// up [`PricingTable`] by the backend's identifier; if the model is
    /// unknown, returns `0` and the cost cap is effectively disabled
    /// for the run.
    pub(crate) fn estimate_cost_cents(&self, usage: &TokenUsage) -> u32 {
        PricingTable::for_model(self.backend.identifier()).map_or(0, |p| p.cost_cents(usage))
    }

    /// Check the four budget caps in priority order. Returns the first
    /// reason tripped, or `None` when the loop may continue.
    ///
    /// Order matters for tie-breaking: turn limit > token budget > cost
    /// budget > duration. Rationale: turns are the cheapest signal and
    /// most-deterministic; duration is fuzziest and goes last.
    pub(crate) fn budget_check(
        &self,
        stats: &AgentStats,
        elapsed: Duration,
    ) -> Option<AgentStopReason> {
        if stats.turns >= self.budget.max_turns {
            return Some(AgentStopReason::TurnLimitReached);
        }
        if stats.total_tokens() >= self.budget.max_tokens_total {
            return Some(AgentStopReason::TokenBudgetExhausted);
        }
        if stats.cost_cents >= self.budget.max_cost_cents {
            return Some(AgentStopReason::CostBudgetExhausted);
        }
        if elapsed >= self.budget.max_duration {
            return Some(AgentStopReason::DurationLimitReached);
        }
        None
    }

    /// Convenience: same check, but takes the loop's start [`Instant`]
    /// instead of an already-computed elapsed.
    pub(crate) fn budget_check_at(
        &self,
        stats: &AgentStats,
        started_at: Instant,
    ) -> Option<AgentStopReason> {
        self.budget_check(stats, started_at.elapsed())
    }
}

impl std::fmt::Debug for AgentRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Skip the system prompt (potentially long, sometimes secret-ish)
        // and the Arc dependencies (noisy). Field count intentionally
        // smaller than the struct's field count.
        f.debug_struct("AgentRunner")
            .field("model", &self.backend.identifier())
            .field("tools", &self.registry.len())
            .field("budget", &self.budget)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[allow(clippy::match_wildcard_for_single_variants)]
mod tests {
    use async_trait::async_trait;
    use basilisk_llm::{CompletionStream, LlmError};

    use super::*;
    use crate::session::SessionStore;
    use crate::tools::standard_registry;

    /// Fixed-identifier backend that never produces a stream — enough
    /// for budget + cost-helper tests. Streaming is exercised in
    /// `CP5d`.
    struct StubBackend {
        id: String,
    }

    #[async_trait]
    impl LlmBackend for StubBackend {
        fn identifier(&self) -> &str {
            &self.id
        }
        async fn stream(
            &self,
            _request: basilisk_llm::CompletionRequest,
        ) -> Result<CompletionStream, LlmError> {
            Err(LlmError::Other("stub backend does not stream".into()))
        }
    }

    fn build_runner(model: &str, budget: Budget) -> AgentRunner {
        let backend = Arc::new(StubBackend { id: model.into() });
        let registry = standard_registry();
        let store = Arc::new(SessionStore::open_in_memory().unwrap());
        let config = Arc::new(Config::default());
        let github = Arc::new(GithubClient::new(None).unwrap());
        let dir = tempfile::tempdir().unwrap();
        let repo_cache = Arc::new(RepoCache::open_at(dir.path().to_path_buf()).unwrap());
        std::mem::forget(dir);
        AgentRunner::new(
            backend,
            registry,
            store,
            config,
            github,
            repo_cache,
            "you are a helpful auditor",
            budget,
        )
    }

    #[test]
    fn runner_exposes_basic_metadata() {
        let runner = build_runner("claude-opus-4-7", Budget::default());
        assert_eq!(runner.model_identifier(), "claude-opus-4-7");
        assert_eq!(runner.tool_count(), 11);
        assert_eq!(runner.budget(), Budget::default());
    }

    #[test]
    fn build_context_threads_session_id_through() {
        let runner = build_runner("claude-opus-4-7", Budget::default());
        let id = SessionId::new("abc-123");
        let ctx = runner.build_context(id.clone());
        assert_eq!(ctx.session_id, id);
    }

    #[test]
    fn cost_estimate_uses_known_pricing_table() {
        let runner = build_runner("claude-opus-4-7", Budget::default());
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 0,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        };
        // $15 / 1M input → 1500 cents.
        assert_eq!(runner.estimate_cost_cents(&usage), 1500);
    }

    #[test]
    fn cost_estimate_returns_zero_for_unknown_model() {
        let runner = build_runner("custom/local-llama", Budget::default());
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        };
        assert_eq!(runner.estimate_cost_cents(&usage), 0);
    }

    #[test]
    fn budget_check_passes_when_well_under_limits() {
        let runner = build_runner("claude-opus-4-7", Budget::default());
        let stats = AgentStats::default();
        assert_eq!(runner.budget_check(&stats, Duration::ZERO), None);
    }

    #[test]
    fn budget_check_trips_turn_limit_first() {
        let runner = build_runner("claude-opus-4-7", Budget::default());
        let stats = AgentStats {
            turns: 40,
            usage: TokenUsage {
                input_tokens: u32::MAX,
                output_tokens: u32::MAX,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            },
            cost_cents: u32::MAX,
            ..AgentStats::default()
        };
        assert_eq!(
            runner.budget_check(&stats, Duration::from_secs(86_400)),
            Some(AgentStopReason::TurnLimitReached),
        );
    }

    #[test]
    fn budget_check_trips_token_budget_when_turns_ok() {
        let runner = build_runner("claude-opus-4-7", Budget::default());
        let stats = AgentStats {
            turns: 1,
            usage: TokenUsage {
                input_tokens: u32::MAX,
                output_tokens: 0,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            },
            ..AgentStats::default()
        };
        assert_eq!(
            runner.budget_check(&stats, Duration::ZERO),
            Some(AgentStopReason::TokenBudgetExhausted),
        );
    }

    #[test]
    fn budget_check_trips_cost_budget_when_tokens_ok() {
        let runner = build_runner("claude-opus-4-7", Budget::default());
        let stats = AgentStats {
            turns: 1,
            cost_cents: 500,
            ..AgentStats::default()
        };
        assert_eq!(
            runner.budget_check(&stats, Duration::ZERO),
            Some(AgentStopReason::CostBudgetExhausted),
        );
    }

    #[test]
    fn budget_check_trips_duration_when_others_ok() {
        let runner = build_runner("claude-opus-4-7", Budget::default());
        let stats = AgentStats {
            turns: 1,
            ..AgentStats::default()
        };
        assert_eq!(
            runner.budget_check(&stats, Duration::from_secs(20 * 60)),
            Some(AgentStopReason::DurationLimitReached),
        );
    }

    #[test]
    fn debug_impl_does_not_leak_system_prompt() {
        let runner = build_runner("claude-opus-4-7", Budget::default());
        let s = format!("{runner:?}");
        assert!(s.contains("AgentRunner"));
        assert!(s.contains("claude-opus-4-7"));
        assert!(!s.contains("you are a helpful auditor"));
    }

    #[test]
    fn budget_check_at_uses_instant_elapsed() {
        let runner = build_runner(
            "claude-opus-4-7",
            Budget {
                max_duration: Duration::from_millis(1),
                ..Budget::default()
            },
        );
        let started = Instant::now();
        std::thread::sleep(Duration::from_millis(5));
        let stats = AgentStats {
            turns: 1,
            ..AgentStats::default()
        };
        assert_eq!(
            runner.budget_check_at(&stats, started),
            Some(AgentStopReason::DurationLimitReached),
        );
    }
}
