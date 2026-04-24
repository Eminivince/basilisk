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
    BlockType, CompletionRequest, CompletionResponse, ContentBlock, Delta, LlmBackend, LlmError,
    Message, MessageRole, ModelPricingSource, PricingTable, StopReason, StreamEvent, TokenUsage,
    ToolChoice,
};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use crate::runner::observer::{AgentObserver, NoopObserver, NudgeEvent, NudgeKind};
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

    /// The session store this runner persists into. Exposed so the CLI
    /// (`CP6`) and integration tests can read back transcripts without
    /// threading the `Arc` in separately.
    pub fn store(&self) -> &Arc<SessionStore> {
        &self.store
    }

    /// The system prompt this runner was built with. Exposed so the CLI
    /// can recompute its hash when deciding whether a resume is safe.
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
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
    /// up [`PricingTable`] by the backend's identifier; if the model
    /// is unknown, returns `0` — the cost cap is effectively disabled
    /// for that run. A one-shot warning is logged at session start
    /// (see [`warn_on_unknown_pricing`]) so operators aren't caught
    /// off-guard.
    pub(crate) fn estimate_cost_cents(&self, usage: &TokenUsage) -> u32 {
        let (pricing, _) = PricingTable::for_model(self.backend.identifier());
        pricing.cost_cents(usage)
    }

    /// Emit a one-shot `tracing::warn!` when the agent's model has no
    /// pricing data. Called once at the top of [`drive_loop`] so the
    /// warning lands before any tokens are spent. Returns the source
    /// so callers can branch if they ever want to (current behaviour:
    /// run with cost enforcement disabled).
    fn warn_on_unknown_pricing(&self) -> ModelPricingSource {
        let (_, source) = PricingTable::for_model(self.backend.identifier());
        if source == ModelPricingSource::Unknown {
            warn!(
                model = %self.backend.identifier(),
                "no pricing data for this model — cost enforcement is disabled for \
                 this session. Set `--max-tokens` / BASILISK_MAX_TOKENS to bound spend, \
                 or add a pricing entry to crates/llm/src/pricing.rs.",
            );
        }
        source
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

    /// Drive one tool-use loop end-to-end with no observer attached.
    ///
    /// Equivalent to [`run_with_observer`](Self::run_with_observer) with
    /// a [`NoopObserver`]. See that method for the full semantics.
    pub async fn run(
        &self,
        target: impl Into<String>,
        initial_user_message: impl Into<String>,
        note: Option<String>,
    ) -> Result<AgentOutcome, AgentError> {
        self.run_with_observer(target, initial_user_message, note, &NoopObserver)
            .await
    }

    /// Drive one tool-use loop end-to-end, firing `observer` hooks as
    /// the session progresses.
    ///
    /// Creates a session, seeds it with `initial_user_message`, then
    /// loops:
    ///
    ///   1. Check the budget; stop if any cap is tripped.
    ///   2. Send the conversation history + tool catalogue to the LLM,
    ///      streaming the response and firing `on_text_delta` /
    ///      `on_tool_use_start` hooks as events arrive.
    ///   3. Record the assistant turn (with token + cost accounting).
    ///   4. Dispatch every `ToolUse` block in iteration order, persisting
    ///      one `tool_calls` row per call and firing `on_tool_result`.
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
    pub async fn run_with_observer(
        &self,
        target: impl Into<String>,
        initial_user_message: impl Into<String>,
        note: Option<String>,
        observer: &dyn AgentObserver,
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
        observer.on_session_start(&session_id);
        info!(
            session = %session_id,
            target = %target,
            model = %self.backend.identifier(),
            "agent session started"
        );

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
        let history: Vec<Message> = vec![Message {
            role: MessageRole::User,
            content: initial_blocks,
        }];

        self.drive_loop(
            session_id,
            target,
            history,
            AgentStats::default(),
            started_inst,
            started_at,
            observer,
        )
        .await
    }

    /// Resume an interrupted session. `history` is the replayed
    /// conversation as reconstructed from the persisted turn log; the
    /// runner appends to it rather than seeding. The session row is
    /// transitioned back to `running` before the first new turn so
    /// `record_turn` passes its status guard.
    ///
    /// Stats start fresh — the resumed run's stats cover turns executed
    /// on this resume only, which matches what operators want to see
    /// from `--max-*` budgets on a resume invocation.
    pub async fn resume_with_observer(
        &self,
        session_id: SessionId,
        target: impl Into<String>,
        history: Vec<Message>,
        observer: &dyn AgentObserver,
    ) -> Result<AgentOutcome, AgentError> {
        let target = target.into();
        let started_inst = Instant::now();
        let started_at = SystemTime::now();

        self.store.mark_resumed(&session_id)?;
        observer.on_session_start(&session_id);
        info!(
            session = %session_id,
            target = %target,
            model = %self.backend.identifier(),
            turns_replayed = history.len(),
            "agent session resumed"
        );

        self.drive_loop(
            session_id,
            target,
            history,
            AgentStats::default(),
            started_inst,
            started_at,
            observer,
        )
        .await
    }

    // The loop is long but linear; splitting it further would force
    // callers to chase the control flow across helpers without making
    // any one piece independently meaningful.
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    async fn drive_loop(
        &self,
        session_id: SessionId,
        _target: String,
        mut history: Vec<Message>,
        mut stats: AgentStats,
        started_inst: Instant,
        started_at: SystemTime,
        observer: &dyn AgentObserver,
    ) -> Result<AgentOutcome, AgentError> {
        let context = self.build_context(session_id.clone());
        // One-shot pricing diagnostic — warns when cost enforcement is
        // silently disabled. Fires at session start so operators see
        // it before any tokens are spent.
        self.warn_on_unknown_pricing();
        let mut final_report: Option<FinalReport> = None;
        // Consecutive-text-end guard. When the model text-ends, we
        // inject a nudge user message and flip `tool_choice` to `Any`
        // on the next request — a provider-level constraint, so the
        // model physically cannot text-end again. Two text-ends IN A
        // ROW (nudge didn't fix it) surrenders to avoid budget waste.
        // A successful tool call breaks the streak and resets the
        // counter, so a later text-end gets its own nudge chance.
        // Budget caps are the ultimate bound on runaway sessions.
        let mut consecutive_text_ends: u32 = 0;
        let mut force_tool_choice: Option<ToolChoice> = None;
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
                // `force_tool_choice` is set only on the turn right
                // after a nudge, flipping the request to `Any` so the
                // provider rejects any text-only completion. It's
                // consumed after one use so subsequent turns go back
                // to `Auto`.
                tool_choice: force_tool_choice.take().unwrap_or_default(),
                stop_sequences: Vec::new(),
                cache_system_prompt: true,
            };

            // `turn` used by observer hooks is 1-based and names the
            // turn we're *about* to execute, matching stats.turns after
            // the accumulator fires below.
            let turn_index_for_observer = stats.turns.saturating_add(1);
            observer.on_turn_start(turn_index_for_observer);

            let turn_started = SystemTime::now();
            let response = match self
                .stream_one_turn(turn_index_for_observer, request, observer)
                .await
            {
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
                observer.on_tool_result(turn_index_for_observer, name, !is_err, duration_ms);
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

            observer.on_turn_end(turn_index_for_observer, &stats);

            if finalize_seen {
                break AgentStopReason::ReportFinalized;
            }

            // No tool calls means the model ended its turn with text
            // only. Send a nudge + force `tool_choice=Any` on the
            // next turn. If this text-end is the SECOND in a row
            // (nudge didn't help), surrender — a third attempt would
            // just burn more budget on the same failure. A successful
            // tool call between text-ends resets the counter.
            if tool_results.is_empty() {
                consecutive_text_ends = consecutive_text_ends.saturating_add(1);
                if consecutive_text_ends >= 2 {
                    break AgentStopReason::LlmError {
                        message: "model ended turn without calling a tool or finalize_report \
                                  (even after a nudge — giving up to avoid budget waste)"
                            .into(),
                    };
                }
                // Observer telemetry: one event per nudge half.
                // SoftPrompt: the user-message reminder we inject
                // right now into history.
                // ForceToolChoice: the tool_choice=Any we set for
                // the next request builder.
                // `turn_index_for_observer + 1` is the turn these
                // nudges apply to — i.e. the one the model will
                // execute next, under the nudge's correction.
                let target_turn = turn_index_for_observer.saturating_add(1);
                observer.on_nudge_fired(NudgeEvent {
                    session_id: session_id.clone(),
                    turn_index: target_turn,
                    kind: NudgeKind::SoftPrompt,
                    consecutive_text_ends,
                });
                observer.on_nudge_fired(NudgeEvent {
                    session_id: session_id.clone(),
                    turn_index: target_turn,
                    kind: NudgeKind::ForceToolChoice,
                    consecutive_text_ends,
                });
                stats.nudge_count = stats.nudge_count.saturating_add(2);

                // Hard rails: force the model to emit a tool call on
                // the next turn. `Any` lets it pick between
                // `finalize_report` and another investigation tool —
                // just not text-only again.
                force_tool_choice = Some(ToolChoice::Any);
                let nudge = "Your previous turn ended with assistant text but no tool call. \
                             Text that isn't inside a `finalize_report` tool call is discarded \
                             — the operator will see nothing. If you meant that to be your final \
                             brief, call `finalize_report` now with the markdown as the \
                             `markdown` argument. Otherwise, make another tool call to continue \
                             investigation.";
                let nudge_blocks = vec![ContentBlock::text(nudge)];
                let at = SystemTime::now();
                self.store.record_turn(
                    &session_id,
                    TurnRole::User,
                    &serde_json::to_value(&nudge_blocks)?,
                    None,
                    None,
                    at,
                    at,
                )?;
                history.push(Message {
                    role: MessageRole::User,
                    content: nudge_blocks,
                });
                continue;
            }

            // Successful tool dispatch breaks any text-end streak —
            // the model is making progress, so a later text-end gets
            // its own fresh nudge chance.
            consecutive_text_ends = 0;

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

        let outcome = AgentOutcome {
            session_id,
            stop_reason,
            stats,
            final_report,
            started_at,
            ended_at,
        };
        observer.on_session_complete(&outcome);
        Ok(outcome)
    }

    /// Consume one turn's worth of streaming events, firing observer
    /// hooks as we go, and fold the result into a buffered
    /// [`CompletionResponse`] for the loop to record + dispatch.
    ///
    /// This mirrors `basilisk_llm::backend::collect_stream` but
    /// interleaves observer calls so the CLI can print tokens live.
    async fn stream_one_turn(
        &self,
        turn: u32,
        request: CompletionRequest,
        observer: &dyn AgentObserver,
    ) -> Result<CompletionResponse, LlmError> {
        let mut stream = self.backend.stream(request).await?;
        let mut model = String::new();
        let mut blocks: Vec<StreamingBlock> = Vec::new();
        let mut stop_reason = StopReason::EndTurn;
        let mut usage = TokenUsage::default();

        while let Some(event) = stream.next().await {
            match event? {
                StreamEvent::MessageStart { model: m } => {
                    model = m;
                }
                StreamEvent::ContentBlockStart { index, block } => {
                    let idx = index as usize;
                    if blocks.len() <= idx {
                        blocks.resize_with(idx + 1, StreamingBlock::default);
                    }
                    if let BlockType::ToolUse { id, name } = &block {
                        observer.on_tool_use_start(turn, name, id);
                    }
                    blocks[idx] = StreamingBlock::from_start(block);
                }
                StreamEvent::ContentBlockDelta { index, delta } => {
                    let idx = index as usize;
                    if idx < blocks.len() {
                        if let Delta::TextDelta(s) = &delta {
                            observer.on_text_delta(turn, s);
                        }
                        blocks[idx].push(delta);
                    }
                }
                StreamEvent::ContentBlockStop { .. } => {}
                StreamEvent::MessageDelta {
                    stop_reason: sr,
                    usage: u,
                } => {
                    if let Some(sr) = sr {
                        stop_reason = sr;
                    }
                    if let Some(u) = u {
                        // Output-token count arrives in the final
                        // MessageDelta; input-token count is seeded from
                        // MessageStart. Take the max so streaming and
                        // non-streaming callers see the same numbers.
                        usage.input_tokens = usage.input_tokens.max(u.input_tokens);
                        usage.output_tokens = usage.output_tokens.max(u.output_tokens);
                        if u.cache_read_input_tokens.is_some() {
                            usage.cache_read_input_tokens = u.cache_read_input_tokens;
                        }
                        if u.cache_creation_input_tokens.is_some() {
                            usage.cache_creation_input_tokens = u.cache_creation_input_tokens;
                        }
                    }
                }
                StreamEvent::MessageStop => break,
            }
        }

        let content: Result<Vec<ContentBlock>, LlmError> =
            blocks.into_iter().map(StreamingBlock::finish).collect();
        Ok(CompletionResponse {
            content: content?,
            stop_reason,
            usage,
            model,
        })
    }
}

/// Accumulator for one in-flight content block.
///
/// Mirrors the private `PartialBlock` used by
/// `basilisk_llm::backend::collect_stream`; duplicated here so the
/// runner can interleave observer hooks between events without adding
/// an observer dependency to the LLM crate.
#[derive(Default)]
struct StreamingBlock {
    kind: Option<BlockType>,
    text: String,
    input_json: String,
}

impl StreamingBlock {
    fn from_start(block: BlockType) -> Self {
        Self {
            kind: Some(block),
            text: String::new(),
            input_json: String::new(),
        }
    }

    fn push(&mut self, delta: Delta) {
        match delta {
            Delta::TextDelta(s) => self.text.push_str(&s),
            Delta::InputJsonDelta(s) => self.input_json.push_str(&s),
        }
    }

    fn finish(self) -> Result<ContentBlock, LlmError> {
        match self.kind {
            Some(BlockType::Text) => Ok(ContentBlock::Text { text: self.text }),
            Some(BlockType::ToolUse { id, name }) => {
                // Anthropic sends `{}` when the model produced no input.
                let raw = if self.input_json.is_empty() {
                    "{}".to_string()
                } else {
                    self.input_json
                };
                let input: serde_json::Value = serde_json::from_str(&raw)
                    .map_err(|e| LlmError::ParseError(format!("tool_use input: {e}")))?;
                Ok(ContentBlock::ToolUse { id, name, input })
            }
            None => Err(LlmError::ParseError(
                "content block delta without start".into(),
            )),
        }
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
    use async_trait::async_trait;
    use basilisk_llm::{CompletionStream, LlmError};

    use super::*;
    use crate::session::SessionStore;
    use crate::tools::standard_registry;

    /// Minimal backend that only answers `identifier` — enough to
    /// exercise the budget + cost helpers. End-to-end loop behaviour
    /// is covered by the integration tests that drive `MockLlmBackend`.
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
        let backend: Arc<dyn LlmBackend> = Arc::new(StubBackend { id: model.into() });
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
        let runner = build_runner("custom/totally-unknown-model", Budget::default());
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        };
        assert_eq!(runner.estimate_cost_cents(&usage), 0);
    }

    #[test]
    fn warn_on_unknown_pricing_returns_source_and_doesnt_panic() {
        // Known model — source is Known, no warning expected.
        let known = build_runner("claude-opus-4-7", Budget::default());
        assert_eq!(known.warn_on_unknown_pricing(), ModelPricingSource::Known);

        // OpenRouter-prefixed form — resolves via prefix stripping.
        let via_or = build_runner("openrouter/anthropic/claude-opus-4-7", Budget::default());
        assert_eq!(
            via_or.warn_on_unknown_pricing(),
            ModelPricingSource::ProviderPrefix,
        );

        // Ollama — known (free), not unknown.
        let local = build_runner("ollama/llama3.1:70b", Budget::default());
        assert_eq!(local.warn_on_unknown_pricing(), ModelPricingSource::Known);

        // Truly unknown — warning fires (trace-only; we just assert
        // the source is Unknown and the call is infallible).
        let unknown = build_runner("some-unknown-model-v99", Budget::default());
        assert_eq!(
            unknown.warn_on_unknown_pricing(),
            ModelPricingSource::Unknown,
        );
    }

    #[test]
    fn cost_estimate_uses_openrouter_prefix_routing() {
        // `openrouter/anthropic/...` should price the same as native
        // Anthropic for that model.
        let runner = build_runner("openrouter/anthropic/claude-opus-4-7", Budget::default());
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 0,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        };
        assert_eq!(runner.estimate_cost_cents(&usage), 1500);
    }

    #[test]
    fn cost_estimate_reports_zero_for_explicit_local_providers() {
        let runner = build_runner("ollama/llama3.1:70b", Budget::default());
        let huge_usage = TokenUsage {
            input_tokens: 10_000_000,
            output_tokens: 5_000_000,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        };
        // Explicit free > unknown: the source is Known, not Unknown.
        assert_eq!(runner.estimate_cost_cents(&huge_usage), 0);
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

    // End-to-end loop behaviour lives in `tests/agent_loop.rs`, which
    // drives `MockLlmBackend` from the public `testing` module so the
    // same scaffold is reusable by downstream crates (CLI, e2e harness).
}
