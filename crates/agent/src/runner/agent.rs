//! [`AgentRunner`] — skeleton in `CP5b`, loop body wired in `CP5c`.
//!
//! Holds every dependency the loop needs and exposes:
//!  - budget-enforcement helpers + a cost estimator (`CP5b`),
//!  - the `run` method that actually drives one tool-use loop (`CP5c`).

// `stats`/`status`, `content`/`context` etc. are unavoidable neighbours
// in this module — `stats` aggregates per-run usage while `status`
// names the SQL column; `content` is a content-block field while
// `context` is the tool-context. Renaming makes call sites worse.
#![allow(clippy::similar_names)]

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use basilisk_core::Config;
use basilisk_git::RepoCache;
use basilisk_github::GithubClient;
use basilisk_llm::{
    CompletionRequest, ContentBlock, LlmBackend, Message, MessageRole, PricingTable, TokenUsage,
    ToolChoice,
};
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use crate::runner::types::{AgentError, AgentOutcome, AgentStats, AgentStopReason, Budget};
use crate::session::{SessionStatus, SessionStore, TurnRole};
use crate::tool::{SessionId, ToolContext, ToolRegistry, ToolResult};
use crate::tools::{Confidence, FinalReport, FINALIZE_REPORT_NAME};

/// Hard-coded ceiling on the assistant's reply length per turn. Big
/// enough for verbose tool-use turns + reasoning; small enough that a
/// runaway token storm is bounded. Configurable via Budget once we
/// have a real reason to vary it.
const MAX_TOKENS_PER_TURN: u32 = 8_192;

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

    /// Drive one tool-use loop end-to-end.
    ///
    /// Creates a session, seeds it with `initial_user_message`, then
    /// loops:
    ///
    ///   1. Check the budget; stop if any cap is tripped.
    ///   2. Send the conversation history + tool catalogue to the LLM.
    ///   3. Record the assistant turn (with token + cost accounting).
    ///   4. Dispatch every `ToolUse` block in iteration order, persisting
    ///      one `tool_calls` row per call.
    ///   5. If `finalize_report` was among them, persist the final report
    ///      and exit cleanly.
    ///   6. Otherwise, feed each tool result back as a `ToolResult` block
    ///      in a new user-role turn and loop again.
    ///
    /// On exit (success or otherwise) the session row is transitioned
    /// out of `running` via `SessionStore::mark_stopped`. Returns
    /// [`AgentOutcome`] populated with the stop reason, the cumulative
    /// stats, and (when finalised) the parsed [`FinalReport`].
    ///
    /// The only failures that bubble up as [`AgentError`] are persistence
    /// failures the loop literally cannot recover from — every other
    /// stop is encoded in [`AgentOutcome::stop_reason`].
    // The loop is long but linear; splitting it would force callers to
    // chase the control flow across helpers without making any one
    // piece independently meaningful.
    #[allow(clippy::too_many_lines)]
    pub async fn run(
        &self,
        target: impl Into<String>,
        initial_user_message: impl Into<String>,
        note: Option<String>,
    ) -> Result<AgentOutcome, AgentError> {
        let target = target.into();
        let initial = initial_user_message.into();
        let started_inst = Instant::now();
        let started_at = SystemTime::now();

        let prompt_hash = sha256_hex(&self.system_prompt);
        let session_id = self.store.create_session(
            target.clone(),
            self.backend.identifier(),
            prompt_hash,
            note,
        )?;
        info!(
            session = %session_id,
            target = %target,
            model = %self.backend.identifier(),
            "agent session started"
        );

        let context = self.build_context(session_id.clone());

        // Seed history with the user's initial prompt + persist it.
        let initial_blocks = vec![ContentBlock::text(initial.clone())];
        let initial_started = SystemTime::now();
        self.store.record_turn(
            &session_id,
            TurnRole::User,
            &serde_json::to_value(&initial_blocks)?,
            None,
            None,
            initial_started,
            initial_started,
        )?;
        let mut history: Vec<Message> = vec![Message {
            role: MessageRole::User,
            content: initial_blocks,
        }];

        let mut stats = AgentStats::default();
        let mut final_report: Option<FinalReport> = None;
        let stop_reason: AgentStopReason = loop {
            if let Some(reason) = self.budget_check_at(&stats, started_inst) {
                debug!(?reason, "budget tripped");
                break reason;
            }

            let request = CompletionRequest {
                system: self.system_prompt.clone(),
                messages: history.clone(),
                tools: self.registry.definitions(),
                max_tokens: MAX_TOKENS_PER_TURN,
                temperature: None,
                tool_choice: ToolChoice::default(),
                stop_sequences: Vec::new(),
                cache_system_prompt: true,
            };

            let turn_started = SystemTime::now();
            let response = match self.backend.complete(request).await {
                Ok(r) => r,
                Err(e) => {
                    warn!(error = %e, "llm backend failed");
                    break AgentStopReason::LlmError {
                        message: e.to_string(),
                    };
                }
            };
            let turn_ended = SystemTime::now();

            stats.usage.accumulate(&response.usage);
            stats.turns = stats.turns.saturating_add(1);
            stats.cost_cents = self.estimate_cost_cents(&stats.usage);

            let assistant_blocks = response.content;
            let turn_index = self.store.record_turn(
                &session_id,
                TurnRole::Assistant,
                &serde_json::to_value(&assistant_blocks)?,
                Some(response.usage.input_tokens),
                Some(response.usage.output_tokens),
                turn_started,
                turn_ended,
            )?;

            // Dispatch every tool_use in order; collect result blocks for
            // the follow-up user turn.
            let mut tool_results: Vec<ContentBlock> = Vec::new();
            let mut call_index: u32 = 0;
            let mut finalize_seen = false;

            for block in &assistant_blocks {
                let ContentBlock::ToolUse { id, name, input } = block else {
                    continue;
                };
                let dispatch_started = Instant::now();
                let result = self.registry.dispatch(name, input.clone(), &context).await;
                let duration_ms = duration_to_ms(dispatch_started.elapsed());
                let (output_value, is_err) = result_to_record(&result);
                self.store.record_tool_call(
                    &session_id,
                    turn_index,
                    call_index,
                    id.clone(),
                    name.clone(),
                    input,
                    output_value.as_ref(),
                    is_err,
                    duration_ms,
                )?;
                stats.tool_calls = stats.tool_calls.saturating_add(1);
                call_index = call_index.saturating_add(1);

                if name == FINALIZE_REPORT_NAME {
                    if let ToolResult::Ok(value) = &result {
                        // FinalizeReport tool is the only producer of this
                        // shape, so a parse failure here means the tool was
                        // changed without updating the runner.
                        let report: FinalReport =
                            serde_json::from_value(value.clone()).map_err(|e| {
                                AgentError::Internal(format!(
                                    "finalize_report returned non-FinalReport JSON: {e}"
                                ))
                            })?;
                        self.store.record_final_report(
                            &session_id,
                            report.markdown.clone(),
                            confidence_str(report.confidence),
                            report.notes.clone(),
                        )?;
                        final_report = Some(report);
                        finalize_seen = true;
                        break;
                    }
                    // Tool errored (invalid input from the model). Fall
                    // through and feed the error back as a ToolResult
                    // so the agent can self-correct.
                }
                let (output_str, err_flag) = result.to_content_pair();
                tool_results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: output_str,
                    is_error: err_flag,
                });
            }

            history.push(Message {
                role: MessageRole::Assistant,
                content: assistant_blocks,
            });

            if finalize_seen {
                break AgentStopReason::ReportFinalized;
            }

            // No tool calls means the model ended its turn without doing
            // anything actionable. The loop has nothing to feed back; stop
            // and let the caller decide what to do.
            if tool_results.is_empty() {
                break AgentStopReason::LlmError {
                    message: "model ended turn without calling a tool or finalize_report".into(),
                };
            }

            let tr_at = SystemTime::now();
            self.store.record_turn(
                &session_id,
                TurnRole::User,
                &serde_json::to_value(&tool_results)?,
                None,
                None,
                tr_at,
                tr_at,
            )?;
            history.push(Message {
                role: MessageRole::User,
                content: tool_results,
            });
        };

        let ended_at = SystemTime::now();
        stats.duration_ms = duration_to_ms(started_inst.elapsed());

        let status = if stop_reason.is_success() {
            SessionStatus::Completed
        } else {
            SessionStatus::Failed
        };
        let stats_value = serde_json::to_value(&stats)?;
        self.store
            .mark_stopped(&session_id, stop_reason.tag(), status, &stats_value)?;
        info!(
            session = %session_id,
            stop = stop_reason.tag(),
            turns = stats.turns,
            tool_calls = stats.tool_calls,
            "agent session ended"
        );

        Ok(AgentOutcome {
            session_id,
            stop_reason,
            stats,
            final_report,
            started_at,
            ended_at,
        })
    }
}

fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}

fn confidence_str(c: Confidence) -> &'static str {
    match c {
        Confidence::High => "high",
        Confidence::Medium => "medium",
        Confidence::Low => "low",
    }
}

fn duration_to_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Convert a [`ToolResult`] into the (`output_json`, `is_error`) pair
/// the session store expects.
fn result_to_record(r: &ToolResult) -> (Option<serde_json::Value>, bool) {
    match r {
        ToolResult::Ok(v) => (Some(v.clone()), false),
        ToolResult::Err { message, .. } => (Some(serde_json::json!({ "error": message })), true),
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
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use basilisk_llm::{CompletionResponse, CompletionStream, LlmError, StopReason};

    use super::*;
    use crate::session::SessionStore;
    use crate::tools::standard_registry;

    /// Fixed-identifier backend that never produces a stream — enough
    /// for budget + cost-helper tests where `run` is never called.
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

    /// Backend that hands out canned responses in FIFO order. Overrides
    /// `complete` directly so we don't have to fabricate a stream of
    /// `StreamEvent`s. The richer mock lands in `CP5d`.
    struct ScriptedBackend {
        id: String,
        queue: Mutex<VecDeque<Result<CompletionResponse, LlmError>>>,
    }

    impl ScriptedBackend {
        fn new(id: &str, items: Vec<Result<CompletionResponse, LlmError>>) -> Self {
            Self {
                id: id.into(),
                queue: Mutex::new(items.into()),
            }
        }
    }

    #[async_trait]
    impl LlmBackend for ScriptedBackend {
        fn identifier(&self) -> &str {
            &self.id
        }

        async fn complete(
            &self,
            _request: basilisk_llm::CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            self.queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(LlmError::Other("scripted backend exhausted".into())))
        }

        async fn stream(
            &self,
            _request: basilisk_llm::CompletionRequest,
        ) -> Result<CompletionStream, LlmError> {
            Err(LlmError::Other(
                "scripted backend does not stream; call complete()".into(),
            ))
        }
    }

    fn finalize_response() -> CompletionResponse {
        CompletionResponse {
            content: vec![ContentBlock::ToolUse {
                id: "tu_1".into(),
                name: FINALIZE_REPORT_NAME.into(),
                input: serde_json::json!({
                    "markdown": "# brief\nthe target is a token contract.",
                    "confidence": "medium",
                    "notes": "double-check the upgrade key",
                }),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            },
            model: "claude-opus-4-7".into(),
        }
    }

    fn build_runner(model: &str, budget: Budget) -> AgentRunner {
        build_runner_with_backend(Arc::new(StubBackend { id: model.into() }), budget)
    }

    fn build_runner_with_backend(backend: Arc<dyn LlmBackend>, budget: Budget) -> AgentRunner {
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

    // --- CP5c: `run` happy-path tests ------------------------------------

    #[tokio::test]
    async fn run_finalizes_immediately_when_first_tool_call_is_finalize_report() {
        let backend = Arc::new(ScriptedBackend::new(
            "claude-opus-4-7",
            vec![Ok(finalize_response())],
        ));
        let runner = build_runner_with_backend(backend, Budget::default());

        let outcome = runner
            .run("eth/0xdead", "audit this token", Some("from test".into()))
            .await
            .expect("run succeeds");

        assert_eq!(outcome.stop_reason, AgentStopReason::ReportFinalized);
        assert_eq!(outcome.stats.turns, 1);
        assert_eq!(outcome.stats.tool_calls, 1);
        assert_eq!(outcome.stats.usage.input_tokens, 100);
        assert_eq!(outcome.stats.usage.output_tokens, 50);
        assert!(outcome.stats.cost_cents > 0, "opus pricing should apply");

        let report = outcome.final_report.as_ref().expect("final report set");
        assert_eq!(report.confidence, Confidence::Medium);
        assert!(report.markdown.contains("brief"));
        assert_eq!(
            report.notes.as_deref(),
            Some("double-check the upgrade key")
        );

        // Persistence: session is `completed`; transcript captures the
        // initial user turn + the assistant turn + the finalize call.
        let snap = runner.store.load_session(&outcome.session_id).unwrap();
        assert_eq!(snap.session.status, SessionStatus::Completed);
        assert_eq!(
            snap.session.stop_reason.as_deref(),
            Some("report_finalized")
        );
        assert_eq!(snap.session.target, "eth/0xdead");
        // CP4c collapses `note` + `final_report_notes` into one field.
        let note = snap.session.note.as_deref().expect("note set");
        assert!(note.contains("from test"));
        assert!(note.contains("double-check the upgrade key"));
        assert_eq!(snap.session.final_confidence.as_deref(), Some("medium"));
        assert!(snap
            .session
            .final_report_markdown
            .as_deref()
            .unwrap()
            .contains("brief"));
        assert_eq!(snap.turns.len(), 2);
        assert_eq!(snap.turns[0].role, TurnRole::User);
        assert_eq!(snap.turns[1].role, TurnRole::Assistant);
        assert_eq!(snap.tool_calls.len(), 1);
        assert_eq!(snap.tool_calls[0].tool_name, "finalize_report");
        assert!(!snap.tool_calls[0].is_error);
    }

    #[tokio::test]
    async fn run_returns_immediately_when_turn_budget_is_zero() {
        let backend = Arc::new(ScriptedBackend::new("claude-opus-4-7", vec![]));
        let runner = build_runner_with_backend(
            backend,
            Budget {
                max_turns: 0,
                ..Budget::default()
            },
        );

        let outcome = runner.run("eth/0x", "go", None).await.unwrap();
        assert_eq!(outcome.stop_reason, AgentStopReason::TurnLimitReached);
        assert_eq!(outcome.stats.turns, 0);
        assert!(outcome.final_report.is_none());

        let snap = runner.store.load_session(&outcome.session_id).unwrap();
        assert_eq!(snap.session.status, SessionStatus::Failed);
        assert_eq!(
            snap.session.stop_reason.as_deref(),
            Some("turn_limit_reached")
        );
        // Only the seeded user turn was recorded.
        assert_eq!(snap.turns.len(), 1);
        assert_eq!(snap.tool_calls.len(), 0);
    }

    #[tokio::test]
    async fn run_records_llm_error_when_backend_fails() {
        let backend = Arc::new(ScriptedBackend::new(
            "claude-opus-4-7",
            vec![Err(LlmError::Other("kaboom".into()))],
        ));
        let runner = build_runner_with_backend(backend, Budget::default());

        let outcome = runner.run("eth/0x", "go", None).await.unwrap();
        match outcome.stop_reason {
            AgentStopReason::LlmError { ref message } => {
                assert!(message.contains("kaboom"), "got {message:?}");
            }
            other => panic!("expected LlmError, got {other:?}"),
        }
        assert_eq!(outcome.stats.turns, 0);
        assert!(outcome.final_report.is_none());

        let snap = runner.store.load_session(&outcome.session_id).unwrap();
        assert_eq!(snap.session.status, SessionStatus::Failed);
        assert_eq!(snap.session.stop_reason.as_deref(), Some("llm_error"));
    }

    #[tokio::test]
    async fn run_stops_when_model_ends_turn_without_calling_a_tool() {
        let response = CompletionResponse {
            content: vec![ContentBlock::text("I'm done thinking.")],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 50,
                output_tokens: 5,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            },
            model: "claude-opus-4-7".into(),
        };
        let backend = Arc::new(ScriptedBackend::new("claude-opus-4-7", vec![Ok(response)]));
        let runner = build_runner_with_backend(backend, Budget::default());

        let outcome = runner.run("eth/0x", "go", None).await.unwrap();
        match outcome.stop_reason {
            AgentStopReason::LlmError { ref message } => {
                assert!(
                    message.contains("without calling a tool"),
                    "got {message:?}"
                );
            }
            other => panic!("expected LlmError, got {other:?}"),
        }
        assert_eq!(outcome.stats.turns, 1);
        assert_eq!(outcome.stats.tool_calls, 0);
    }
}
