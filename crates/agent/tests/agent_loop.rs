//! End-to-end loop scenarios — `CP5d`.
//!
//! Everything here drives [`AgentRunner::run`] through
//! [`MockLlmBackend`] + [`EchoTool`] from the public `testing` module,
//! so the same scaffold is reusable by the CLI tests in `CP6` and the
//! live-tests in `CP8`.
//!
//! What's covered:
//!  - immediate finalize (one assistant turn, `finalize_report`)
//!  - multi-turn: tool call → result fed back → finalize on next turn
//!  - parallel tool calls within one assistant turn
//!  - tool error fed back, agent recovers, finalizes
//!  - budget exhaustion mid-run (turn + cost caps)
//!  - LLM backend error → graceful `LlmError` stop + persisted transcript

use std::sync::Arc;

use basilisk_agent::testing::{build_test_runner, EchoTool, MockLlmBackend, MockResponse};
use basilisk_agent::tools::{Confidence, FINALIZE_REPORT_NAME};
use basilisk_agent::{
    AgentObserver, AgentOutcome, AgentStats, AgentStopReason, Budget, NudgeEvent, NudgeKind,
    SessionId, SessionStatus, TurnRole,
};
use basilisk_llm::{LlmBackend, LlmError, TokenUsage};

fn make_runner(
    backend: Arc<dyn LlmBackend>,
    budget: Budget,
) -> (basilisk_agent::AgentRunner, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let runner = build_test_runner(backend, dir.path().to_path_buf(), budget);
    (runner, dir)
}

#[tokio::test]
async fn immediate_finalize_completes_with_one_turn() {
    let backend = Arc::new(MockLlmBackend::new("claude-opus-4-7"));
    backend.push(
        MockResponse::new()
            .text("writing the brief now")
            .finalize(
                "tu_f",
                "# brief\ntarget is an ERC-20.",
                Confidence::High,
                None,
            )
            .usage(200, 80)
            .build(),
    );

    let (runner, _dir) = make_runner(backend, Budget::default());
    let outcome = runner
        .run("eth/0xdead", "audit this", None)
        .await
        .expect("run ok");

    assert_eq!(outcome.stop_reason, AgentStopReason::ReportFinalized);
    assert_eq!(outcome.stats.turns, 1);
    assert_eq!(outcome.stats.tool_calls, 1);
    let report = outcome.final_report.expect("report set");
    assert_eq!(report.confidence, Confidence::High);
    assert!(report.markdown.contains("ERC-20"));

    let snap = runner.store().load_session(&outcome.session_id).unwrap();
    assert_eq!(snap.session.status, SessionStatus::Completed);
    assert_eq!(
        snap.session.stop_reason.as_deref(),
        Some("report_finalized")
    );
    // user (initial) + assistant (the finalize turn)
    assert_eq!(snap.turns.len(), 2);
    assert_eq!(snap.tool_calls.len(), 1);
    assert_eq!(snap.tool_calls[0].tool_name, FINALIZE_REPORT_NAME);
}

#[tokio::test]
async fn multi_turn_echo_then_finalize_persists_full_transcript() {
    let backend = Arc::new(MockLlmBackend::new("claude-opus-4-7"));
    // Turn 1: agent calls echo.
    backend.push(
        MockResponse::new()
            .tool_use("tu_echo", EchoTool::NAME, serde_json::json!({"q": "what?"}))
            .usage(100, 40)
            .build(),
    );
    // Turn 2: agent finalises.
    backend.push(
        MockResponse::new()
            .finalize(
                "tu_f",
                "# report\necho returned as expected",
                Confidence::Medium,
                Some("iterate more".into()),
            )
            .usage(150, 60)
            .build(),
    );

    let (runner, _dir) = make_runner(backend, Budget::default());
    let outcome = runner.run("eth/0x", "go", None).await.unwrap();

    assert_eq!(outcome.stop_reason, AgentStopReason::ReportFinalized);
    assert_eq!(outcome.stats.turns, 2);
    assert_eq!(outcome.stats.tool_calls, 2);
    // Tokens accumulate across turns.
    assert_eq!(outcome.stats.usage.input_tokens, 250);
    assert_eq!(outcome.stats.usage.output_tokens, 100);
    let report = outcome.final_report.unwrap();
    assert_eq!(report.confidence, Confidence::Medium);
    assert_eq!(report.notes.as_deref(), Some("iterate more"));

    let snap = runner.store().load_session(&outcome.session_id).unwrap();
    // user(initial) + assistant(echo) + user(tool_result) + assistant(finalize)
    assert_eq!(snap.turns.len(), 4);
    assert_eq!(snap.turns[0].role, TurnRole::User);
    assert_eq!(snap.turns[1].role, TurnRole::Assistant);
    assert_eq!(snap.turns[2].role, TurnRole::User);
    assert_eq!(snap.turns[3].role, TurnRole::Assistant);
    assert_eq!(snap.tool_calls.len(), 2);
    assert_eq!(snap.tool_calls[0].tool_name, EchoTool::NAME);
    assert!(!snap.tool_calls[0].is_error);
    assert_eq!(snap.tool_calls[1].tool_name, FINALIZE_REPORT_NAME);
}

#[tokio::test]
async fn parallel_tool_calls_in_one_turn_all_dispatch_before_next_llm_call() {
    let backend = Arc::new(MockLlmBackend::new("claude-opus-4-7"));
    backend.push(
        MockResponse::new()
            .tool_use("tu_a", EchoTool::NAME, serde_json::json!({"n": 1}))
            .tool_use("tu_b", EchoTool::NAME, serde_json::json!({"n": 2}))
            .tool_use("tu_c", EchoTool::NAME, serde_json::json!({"n": 3}))
            .usage(100, 50)
            .build(),
    );
    backend.push(
        MockResponse::new()
            .finalize("tu_f", "# done", Confidence::High, None)
            .usage(120, 20)
            .build(),
    );

    let (runner, _dir) = make_runner(backend, Budget::default());
    let outcome = runner.run("eth/0x", "go", None).await.unwrap();

    assert_eq!(outcome.stop_reason, AgentStopReason::ReportFinalized);
    assert_eq!(outcome.stats.turns, 2);
    assert_eq!(outcome.stats.tool_calls, 4, "3 echoes + 1 finalize");

    let snap = runner.store().load_session(&outcome.session_id).unwrap();
    // All three echoes share the same turn_index; the runner assigns
    // call_index 0..2 within that turn.
    let echo_calls: Vec<_> = snap
        .tool_calls
        .iter()
        .filter(|c| c.tool_name == EchoTool::NAME)
        .collect();
    assert_eq!(echo_calls.len(), 3);
    let first_turn = echo_calls[0].turn_index;
    assert!(
        echo_calls.iter().all(|c| c.turn_index == first_turn),
        "parallel tool calls live on the same turn"
    );
    assert_eq!(echo_calls[0].call_index, 0);
    assert_eq!(echo_calls[1].call_index, 1);
    assert_eq!(echo_calls[2].call_index, 2);
}

#[tokio::test]
async fn agent_recovers_from_unknown_tool_error() {
    let backend = Arc::new(MockLlmBackend::new("claude-opus-4-7"));
    // Turn 1: agent calls a tool that doesn't exist. The registry
    // returns a non-retryable error that the loop feeds back as a
    // ToolResult content block — not a fatal stop.
    backend.push(
        MockResponse::new()
            .tool_use("tu_1", "this_tool_does_not_exist", serde_json::json!({}))
            .usage(50, 30)
            .build(),
    );
    // Turn 2: agent sees the error and finalises instead.
    backend.push(
        MockResponse::new()
            .finalize(
                "tu_f",
                "# apology\ni tried a missing tool; here's what i have",
                Confidence::Low,
                None,
            )
            .usage(80, 40)
            .build(),
    );

    let (runner, _dir) = make_runner(backend, Budget::default());
    let outcome = runner.run("eth/0x", "go", None).await.unwrap();

    assert_eq!(outcome.stop_reason, AgentStopReason::ReportFinalized);
    assert_eq!(outcome.stats.tool_calls, 2, "bad call + finalize");
    let report = outcome.final_report.unwrap();
    assert_eq!(report.confidence, Confidence::Low);

    let snap = runner.store().load_session(&outcome.session_id).unwrap();
    let bad = snap
        .tool_calls
        .iter()
        .find(|c| c.tool_name == "this_tool_does_not_exist")
        .expect("bad call recorded");
    assert!(bad.is_error, "unknown-tool dispatch recorded as error");
}

#[tokio::test]
async fn finalize_with_invalid_input_stays_in_loop_until_agent_recovers() {
    let backend = Arc::new(MockLlmBackend::new("claude-opus-4-7"));
    // Turn 1: agent calls finalize_report with empty markdown — the
    // tool itself rejects this as non-retryable. The loop MUST NOT
    // treat a failing finalize_report as `ReportFinalized`; it feeds
    // the error back and keeps going.
    backend.push(
        MockResponse::new()
            .tool_use(
                "tu_bad_f",
                FINALIZE_REPORT_NAME,
                serde_json::json!({
                    "markdown": "   ",
                    "confidence": "high",
                }),
            )
            .usage(50, 20)
            .build(),
    );
    // Turn 2: agent retries with a valid report.
    backend.push(
        MockResponse::new()
            .finalize("tu_f", "# now valid", Confidence::High, None)
            .usage(60, 30)
            .build(),
    );

    let (runner, _dir) = make_runner(backend, Budget::default());
    let outcome = runner.run("eth/0x", "go", None).await.unwrap();

    assert_eq!(outcome.stop_reason, AgentStopReason::ReportFinalized);
    assert_eq!(outcome.stats.turns, 2);
    assert_eq!(outcome.stats.tool_calls, 2);

    let snap = runner.store().load_session(&outcome.session_id).unwrap();
    let calls: Vec<_> = snap
        .tool_calls
        .iter()
        .filter(|c| c.tool_name == FINALIZE_REPORT_NAME)
        .collect();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].is_error, "first finalize attempt was rejected");
    assert!(!calls[1].is_error, "second attempt succeeded");
}

#[tokio::test]
async fn turn_limit_trips_mid_run_even_when_backend_has_more_responses_queued() {
    let backend = Arc::new(MockLlmBackend::new("claude-opus-4-7"));
    // Queue a 3-turn script: echo → echo → finalize
    for i in 0..2 {
        backend.push(
            MockResponse::new()
                .tool_use(
                    format!("tu_{i}"),
                    EchoTool::NAME,
                    serde_json::json!({ "n": i }),
                )
                .usage(50, 20)
                .build(),
        );
    }
    backend.push(
        MockResponse::new()
            .finalize("tu_f", "# done", Confidence::High, None)
            .build(),
    );

    // But cap the loop at 2 turns — finalize never gets reached.
    let (runner, _dir) = make_runner(
        backend.clone(),
        Budget {
            max_turns: 2,
            ..Budget::default()
        },
    );
    let outcome = runner.run("eth/0x", "go", None).await.unwrap();

    assert_eq!(outcome.stop_reason, AgentStopReason::TurnLimitReached);
    assert_eq!(outcome.stats.turns, 2);
    assert_eq!(outcome.stats.tool_calls, 2, "both echoes dispatched");
    assert!(outcome.final_report.is_none());
    // The finalize response is still in the queue, proving we stopped
    // short.
    assert_eq!(backend.remaining(), 1);

    let snap = runner.store().load_session(&outcome.session_id).unwrap();
    assert_eq!(snap.session.status, SessionStatus::Failed);
    assert_eq!(
        snap.session.stop_reason.as_deref(),
        Some("turn_limit_reached")
    );
}

#[tokio::test]
async fn cost_budget_trips_once_cumulative_usage_crosses_cap() {
    let backend = Arc::new(MockLlmBackend::new("claude-opus-4-7"));
    // Each turn: 500k input tokens, 0 output → $7.50 cost per turn.
    // Cap at 1000 cents → first turn accumulates 750 cents (under),
    // second turn pushes it to 1500 (over) and the loop stops before
    // the third call.
    for _ in 0..3 {
        backend.push(
            MockResponse::new()
                .tool_use("tu", EchoTool::NAME, serde_json::json!({}))
                .usage(500_000, 0)
                .build(),
        );
    }

    let (runner, _dir) = make_runner(
        backend.clone(),
        Budget {
            max_cost_cents: 1000,
            // Give the token cap plenty of headroom so it doesn't win.
            max_tokens_total: u64::MAX,
            ..Budget::default()
        },
    );
    let outcome = runner.run("eth/0x", "go", None).await.unwrap();

    assert_eq!(outcome.stop_reason, AgentStopReason::CostBudgetExhausted);
    assert_eq!(outcome.stats.turns, 2);
    assert!(outcome.stats.cost_cents >= 1000);
    assert!(outcome.final_report.is_none());
    assert_eq!(backend.remaining(), 1);
}

#[tokio::test]
async fn llm_error_mid_run_persists_partial_transcript() {
    let backend = Arc::new(MockLlmBackend::new("claude-opus-4-7"));
    // Turn 1: real response with echo.
    backend.push(
        MockResponse::new()
            .tool_use("tu_1", EchoTool::NAME, serde_json::json!({"x": 1}))
            .usage(40, 20)
            .build(),
    );
    // Turn 2: backend fails. Loop records the stop, does NOT lose the
    // first turn.
    backend.push_err(LlmError::Other("http 500: upstream flaked".into()));

    let (runner, _dir) = make_runner(backend, Budget::default());
    let outcome = runner.run("eth/0x", "go", None).await.unwrap();

    match outcome.stop_reason {
        AgentStopReason::LlmError { ref message } => {
            assert!(message.contains("upstream flaked"), "got {message:?}");
        }
        other => panic!("got {other:?}"),
    }
    assert_eq!(outcome.stats.turns, 1);
    assert_eq!(outcome.stats.tool_calls, 1);
    assert!(outcome.final_report.is_none());

    let snap = runner.store().load_session(&outcome.session_id).unwrap();
    assert_eq!(snap.session.status, SessionStatus::Failed);
    assert_eq!(snap.session.stop_reason.as_deref(), Some("llm_error"));
    // First assistant turn + tool result turn survived.
    assert_eq!(snap.turns.len(), 3);
    assert_eq!(snap.tool_calls.len(), 1);
}

#[tokio::test]
async fn zero_turn_budget_records_only_seed_turn() {
    let backend = Arc::new(MockLlmBackend::new("claude-opus-4-7"));
    let (runner, _dir) = make_runner(
        backend,
        Budget {
            max_turns: 0,
            ..Budget::default()
        },
    );
    let outcome = runner.run("eth/0x", "go", None).await.unwrap();

    assert_eq!(outcome.stop_reason, AgentStopReason::TurnLimitReached);
    assert_eq!(outcome.stats.turns, 0);
    let snap = runner.store().load_session(&outcome.session_id).unwrap();
    assert_eq!(snap.turns.len(), 1);
    assert_eq!(snap.turns[0].role, TurnRole::User);
}

#[tokio::test]
async fn two_consecutive_text_only_turns_give_up_after_nudge() {
    // The runner nudges once when a turn text-ends; if the next turn
    // also text-ends, it surrenders. Two text-only responses queued
    // here exercise that path.
    let backend = Arc::new(MockLlmBackend::new("claude-opus-4-7"));
    backend.push(
        MockResponse::new()
            .text("I'm not sure what to do here.")
            .usage(30, 10)
            .build(),
    );
    backend.push(
        MockResponse::new()
            .text("Still not sure, sorry.")
            .usage(20, 8)
            .build(),
    );

    let (runner, _dir) = make_runner(backend, Budget::default());
    let outcome = runner.run("eth/0x", "go", None).await.unwrap();

    match outcome.stop_reason {
        AgentStopReason::LlmError { ref message } => {
            assert!(
                message.contains("even after a nudge"),
                "expected the post-nudge surrender message, got {message:?}",
            );
        }
        other => panic!("got {other:?}"),
    }
    assert_eq!(outcome.stats.turns, 2, "nudge burns one extra turn");
    assert_eq!(outcome.stats.tool_calls, 0);
    assert!(outcome.final_report.is_none());

    // The persisted transcript must include the nudge turn so
    // `audit session show` tells the truth about what went on the
    // wire. Expected turn count: seed user + turn-1 assistant +
    // nudge user + turn-2 assistant = 4.
    let snap = runner.store().load_session(&outcome.session_id).unwrap();
    assert_eq!(snap.turns.len(), 4);
    // The nudge turn is the third row (index 2) and must be user-role
    // carrying a reminder about `finalize_report`.
    assert_eq!(snap.turns[2].role, TurnRole::User);
    let nudge_content = snap.turns[2].content.to_string();
    assert!(
        nudge_content.contains("finalize_report"),
        "nudge turn should reference finalize_report; got {nudge_content}",
    );
}

#[tokio::test]
async fn recover_then_text_end_again_gets_a_fresh_nudge() {
    // Regression for a live-run bug: the nudge flag was session-wide,
    // so a session that recovered from one text-end and later hit a
    // SECOND (non-consecutive) text-end surrendered instead of
    // nudging again. The invariant is "consecutive text-ends," not
    // "has ever nudged."
    //
    // Sequence: echo (ok) → text (nudge 1, Any) → echo (recovers,
    // streak reset) → text (nudge 2, Any) → finalize.
    let backend = Arc::new(MockLlmBackend::new("claude-opus-4-7"));
    backend.push(
        MockResponse::new()
            .tool_use("tu_1", EchoTool::NAME, serde_json::json!({"a": 1}))
            .build(),
    );
    backend.push(MockResponse::new().text("I'm thinking...").build()); // text-end #1
    backend.push(
        MockResponse::new()
            .tool_use("tu_2", EchoTool::NAME, serde_json::json!({"a": 2}))
            .build(),
    ); // forced recovery
    backend.push(MockResponse::new().text("Still thinking...").build()); // text-end #2
    backend.push(
        MockResponse::new()
            .finalize("tu_f", "# done", Confidence::Medium, None)
            .build(),
    );

    let (runner, _dir) = make_runner(backend.clone(), Budget::default());
    let outcome = runner.run("eth/0x", "go", None).await.unwrap();

    assert_eq!(
        outcome.stop_reason,
        AgentStopReason::ReportFinalized,
        "session should finalize despite two non-consecutive text-ends",
    );

    // Two forced-tool-choice turns in this sequence. Both must carry
    // `Any` — proof that the nudge was re-armed after recovery.
    let choices = backend.tool_choices();
    let any_turns: Vec<_> = choices
        .iter()
        .filter(|c| matches!(c, basilisk_llm::ToolChoice::Any))
        .collect();
    assert_eq!(
        any_turns.len(),
        2,
        "expected two nudge turns forcing Any, got choices={choices:?}",
    );
}

#[tokio::test]
async fn nudge_fires_both_halves_and_increments_stats_count() {
    // Text-end on turn 1 → runner injects nudge and forces Any on
    // turn 2 → model finalizes. Observer must see both nudge halves
    // (SoftPrompt + ForceToolChoice), both tagged with turn_index=2
    // and streak=1; stats.nudge_count must be 2.
    let backend = Arc::new(MockLlmBackend::new("claude-opus-4-7"));
    backend.push(MockResponse::new().text("Here's my brief: ...").build());
    backend.push(
        MockResponse::new()
            .finalize("tu_f", "# brief", Confidence::Medium, None)
            .build(),
    );

    let (runner, _dir) = make_runner(backend, Budget::default());
    let obs = RecordingObserver::default();
    let outcome = runner
        .run_with_observer("eth/0x", "go", None, &obs)
        .await
        .expect("run ok");

    assert_eq!(outcome.stop_reason, AgentStopReason::ReportFinalized);
    assert_eq!(
        outcome.stats.nudge_count, 2,
        "each nudge fires both halves (soft + force); one recovery run = 2 events",
    );

    let events = obs.take();
    let nudges: Vec<&String> = events.iter().filter(|e| e.starts_with("nudge/")).collect();
    assert_eq!(nudges.len(), 2, "exactly two nudge events: {events:?}");
    assert_eq!(nudges[0].as_str(), "nudge/soft/turn=2/streak=1");
    assert_eq!(nudges[1].as_str(), "nudge/force/turn=2/streak=1");
}

#[tokio::test]
async fn text_only_turn_followed_by_tool_call_recovers_via_nudge() {
    // Model text-ends on turn 1, receives the nudge, and recovers by
    // calling finalize_report on turn 2. Exercises the recovery path
    // and proves a successful session can include one nudged turn.
    let backend = Arc::new(MockLlmBackend::new("claude-opus-4-7"));
    backend.push(
        MockResponse::new()
            .text("Here's my brief: ...")
            .usage(80, 20)
            .build(),
    );
    backend.push(
        MockResponse::new()
            .finalize("tu_f", "# brief\nok", Confidence::Medium, None)
            .usage(30, 10)
            .build(),
    );

    let (runner, _dir) = make_runner(backend.clone(), Budget::default());
    let outcome = runner.run("eth/0x", "go", None).await.unwrap();

    assert_eq!(outcome.stop_reason, AgentStopReason::ReportFinalized);
    assert_eq!(outcome.stats.turns, 2);
    assert!(outcome.final_report.is_some());

    // Hard-rails guard: turn 1 goes out with `Auto` (model's choice);
    // turn 2 — the post-nudge turn — must go out with `Any` so the
    // provider rejects another text-only completion.
    let choices = backend.tool_choices();
    assert_eq!(choices.len(), 2, "two turns, two captured requests");
    assert!(
        matches!(choices[0], basilisk_llm::ToolChoice::Auto),
        "first turn should use Auto, got {:?}",
        choices[0],
    );
    assert!(
        matches!(choices[1], basilisk_llm::ToolChoice::Any),
        "post-nudge turn should use Any, got {:?}",
        choices[1],
    );
}

#[tokio::test]
async fn token_usage_is_recorded_per_turn_in_the_session_store() {
    let backend = Arc::new(MockLlmBackend::new("claude-opus-4-7"));
    backend.push(
        MockResponse::new()
            .text("thinking")
            .finalize("tu_f", "# done", Confidence::High, None)
            .usage(123, 45)
            .build(),
    );

    let (runner, _dir) = make_runner(backend, Budget::default());
    let outcome = runner.run("eth/0x", "go", None).await.unwrap();
    assert_eq!(outcome.stop_reason, AgentStopReason::ReportFinalized);

    let snap = runner.store().load_session(&outcome.session_id).unwrap();
    let assistant = snap
        .turns
        .iter()
        .find(|t| t.role == TurnRole::Assistant)
        .unwrap();
    assert_eq!(assistant.tokens_in, Some(123));
    assert_eq!(assistant.tokens_out, Some(45));
}

#[tokio::test]
async fn repeated_runs_on_same_runner_mint_distinct_sessions() {
    let backend = Arc::new(MockLlmBackend::new("claude-opus-4-7"));
    backend.push(
        MockResponse::new()
            .finalize("tu_f1", "# first", Confidence::High, None)
            .build(),
    );
    backend.push(
        MockResponse::new()
            .finalize("tu_f2", "# second", Confidence::Low, None)
            .build(),
    );

    let (runner, _dir) = make_runner(backend, Budget::default());

    let a = runner.run("target-a", "go", None).await.unwrap();
    let b = runner.run("target-b", "go", None).await.unwrap();

    assert_ne!(a.session_id, b.session_id);
    assert_eq!(a.stop_reason, AgentStopReason::ReportFinalized);
    assert_eq!(b.stop_reason, AgentStopReason::ReportFinalized);

    let sa = runner.store().load_session(&a.session_id).unwrap();
    let sb = runner.store().load_session(&b.session_id).unwrap();
    assert_eq!(sa.session.target, "target-a");
    assert_eq!(sb.session.target, "target-b");
    assert!(sa.session.final_report_markdown.unwrap().contains("first"));
    assert!(sb.session.final_report_markdown.unwrap().contains("second"));
}

#[tokio::test]
async fn accumulated_token_usage_includes_cache_reads_and_writes() {
    let backend = Arc::new(MockLlmBackend::new("claude-opus-4-7"));
    let resp_with_cache = {
        let mut r = MockResponse::new()
            .finalize("tu_f", "# done", Confidence::High, None)
            .build();
        r.usage = TokenUsage {
            input_tokens: 50,
            output_tokens: 20,
            cache_read_input_tokens: Some(10),
            cache_creation_input_tokens: Some(5),
        };
        r
    };
    backend.push(resp_with_cache);

    let (runner, _dir) = make_runner(backend, Budget::default());
    let outcome = runner.run("eth/0x", "go", None).await.unwrap();

    assert_eq!(outcome.stats.usage.input_tokens, 50);
    assert_eq!(outcome.stats.usage.output_tokens, 20);
    assert_eq!(outcome.stats.usage.cache_read_input_tokens, Some(10));
    assert_eq!(outcome.stats.usage.cache_creation_input_tokens, Some(5));
    assert_eq!(outcome.stats.total_tokens(), 85);
}

/// Recording observer — captures every hook invocation in order so the
/// test can assert the full event sequence.
#[derive(Default)]
struct RecordingObserver {
    events: std::sync::Mutex<Vec<String>>,
}

impl RecordingObserver {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }
    fn push(&self, s: impl Into<String>) {
        self.events.lock().unwrap().push(s.into());
    }
}

impl AgentObserver for RecordingObserver {
    fn on_session_start(&self, _: &SessionId) {
        self.push("session_start");
    }
    fn on_turn_start(&self, turn: u32) {
        self.push(format!("turn_start/{turn}"));
    }
    fn on_text_delta(&self, turn: u32, text: &str) {
        self.push(format!("text/{turn}/{text}"));
    }
    fn on_tool_use_start(&self, turn: u32, name: &str, _id: &str) {
        self.push(format!("tool_start/{turn}/{name}"));
    }
    fn on_tool_result(&self, turn: u32, name: &str, ok: bool, _: u64) {
        self.push(format!("tool_result/{turn}/{name}/{ok}"));
    }
    fn on_turn_end(&self, turn: u32, stats: &AgentStats) {
        self.push(format!("turn_end/{turn}/turns={}", stats.turns));
    }
    fn on_session_complete(&self, outcome: &AgentOutcome) {
        self.push(format!("session_complete/{}", outcome.stop_reason.tag()));
    }
    fn on_nudge_fired(&self, event: NudgeEvent) {
        let kind = match event.kind {
            NudgeKind::SoftPrompt => "soft",
            NudgeKind::ForceToolChoice => "force",
        };
        self.push(format!(
            "nudge/{kind}/turn={}/streak={}",
            event.turn_index, event.consecutive_text_ends,
        ));
    }
}

#[tokio::test]
async fn observer_fires_hooks_in_order_across_multi_turn_session() {
    // Turn 1: text + echo tool_use. Turn 2: finalize.
    let backend = Arc::new(MockLlmBackend::new("claude-opus-4-7"));
    backend.push(
        MockResponse::new()
            .text("starting recon")
            .tool_use("tu_e", EchoTool::NAME, serde_json::json!({"k": "v"}))
            .usage(120, 50)
            .build(),
    );
    backend.push(
        MockResponse::new()
            .text("done, writing brief")
            .finalize("tu_f", "# brief", Confidence::Medium, None)
            .usage(130, 30)
            .build(),
    );

    let (runner, _dir) = make_runner(backend, Budget::default());
    let obs = RecordingObserver::default();
    let outcome = runner
        .run_with_observer("eth/0xabc", "go", None, &obs)
        .await
        .expect("run ok");
    assert_eq!(outcome.stop_reason, AgentStopReason::ReportFinalized);

    let events = obs.take();

    // Session boundaries at the extremes.
    assert_eq!(events.first().unwrap(), "session_start");
    assert_eq!(events.last().unwrap(), "session_complete/report_finalized");

    // Every turn has a start/end pair in the right order.
    let turn_start_positions: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.starts_with("turn_start/"))
        .map(|(i, _)| i)
        .collect();
    let turn_end_positions: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.starts_with("turn_end/"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(turn_start_positions.len(), 2, "two turn_starts");
    assert_eq!(turn_end_positions.len(), 2, "two turn_ends");
    assert!(turn_start_positions[0] < turn_end_positions[0]);
    assert!(turn_end_positions[0] < turn_start_positions[1]);
    assert!(turn_start_positions[1] < turn_end_positions[1]);

    // Turn 1 fires: text delta → echo tool_start (while streaming) →
    // echo tool_result (after dispatch) → turn_end.
    let turn1: Vec<&String> = events[turn_start_positions[0]..=turn_end_positions[0]]
        .iter()
        .collect();
    let has = |needle: &str| turn1.iter().any(|e| e.as_str() == needle);
    assert!(has("text/1/starting recon"), "turn1 text: {turn1:?}");
    assert!(has("tool_start/1/echo"), "turn1 tool_start: {turn1:?}");
    assert!(
        has("tool_result/1/echo/true"),
        "turn1 tool_result: {turn1:?}"
    );

    // Tool start must precede tool result (streamed before dispatch).
    let start_idx = turn1
        .iter()
        .position(|e| e.as_str() == "tool_start/1/echo")
        .unwrap();
    let result_idx = turn1
        .iter()
        .position(|e| e.as_str() == "tool_result/1/echo/true")
        .unwrap();
    assert!(start_idx < result_idx);

    // Turn 2 finalizes; we see the finalize_report tool_start + result.
    let turn2: Vec<&String> = events[turn_start_positions[1]..=turn_end_positions[1]]
        .iter()
        .collect();
    assert!(
        turn2
            .iter()
            .any(|e| e.as_str() == "tool_start/2/finalize_report"),
        "turn2 tool_start: {turn2:?}",
    );
    assert!(
        turn2
            .iter()
            .any(|e| e.as_str() == "tool_result/2/finalize_report/true"),
        "turn2 tool_result: {turn2:?}",
    );
}

#[tokio::test]
async fn observer_does_not_receive_tool_events_on_llm_error() {
    // Backend errors immediately — no turn should fire tool hooks.
    let backend = Arc::new(MockLlmBackend::new("claude-opus-4-7"));
    backend.push_err(LlmError::Other("network went away".into()));

    let (runner, _dir) = make_runner(backend, Budget::default());
    let obs = RecordingObserver::default();
    let outcome = runner
        .run_with_observer("eth/0x", "go", None, &obs)
        .await
        .unwrap();

    let events = obs.take();
    assert!(
        matches!(outcome.stop_reason, AgentStopReason::LlmError { .. }),
        "{:?}",
        outcome.stop_reason,
    );
    assert!(events.iter().any(|e| e.starts_with("turn_start/")));
    assert!(
        events
            .iter()
            .all(|e| !e.starts_with("tool_start/") && !e.starts_with("tool_result/")),
        "no tool events expected, got {events:?}",
    );
    assert!(events
        .last()
        .unwrap()
        .starts_with("session_complete/llm_error"));
}

// --- Set 9 / CP9.7: ordering rail ---------------------------------------

/// The rail blocks the first `finalize_report` when no
/// `finalize_self_critique` has run, surfaces a retryable nudge
/// error, and lets the agent re-enter with a proper critique.
#[tokio::test]
async fn ordering_rail_blocks_finalize_then_allows_after_self_critique() {
    use basilisk_agent::testing::build_test_runner_with_self_critique;
    use basilisk_agent::tools::FINALIZE_SELF_CRITIQUE_NAME;

    let backend = Arc::new(MockLlmBackend::new("claude-opus-4-7"));

    // Turn 1: agent tries finalize_report without a critique.
    backend.push(
        MockResponse::new()
            .finalize("tu_f1", "# premature\nI'm done.", Confidence::Medium, None)
            .usage(60, 30)
            .build(),
    );
    // Turn 2: agent reflects, then finalizes. Both tool calls in
    // one assistant turn — the rail passes because the critique
    // lands before finalize in the dispatch order.
    backend.push(
        MockResponse::new()
            .tool_use(
                "tu_crit",
                FINALIZE_SELF_CRITIQUE_NAME,
                serde_json::json!({
                    "findings_quality_assessment": "thin — only one scan",
                    "methodology_gaps": "didn't probe callers thoroughly",
                    "what_to_improve": "use find_callers_of earlier",
                }),
            )
            .finalize(
                "tu_f2",
                "# now properly\nhere is the report after reflection.",
                Confidence::Medium,
                None,
            )
            .usage(150, 80)
            .build(),
    );

    let dir = tempfile::tempdir().unwrap();
    let runner =
        build_test_runner_with_self_critique(backend, dir.path().to_path_buf(), Budget::default());
    let outcome = runner
        .run("eth/0xdead", "audit this", None)
        .await
        .expect("run ok");

    assert_eq!(outcome.stop_reason, AgentStopReason::ReportFinalized);
    assert_eq!(outcome.stats.turns, 2);
    let report = outcome.final_report.expect("final report");
    assert!(report.markdown.contains("after reflection"));

    // Inspect the persisted transcript: there should be a
    // rail-nudge tool_call row for turn 1's finalize_report (with
    // is_error=true), then the critique + the real finalize on turn 2.
    let snap = runner.store().load_session(&outcome.session_id).unwrap();
    let finalize_rows: Vec<_> = snap
        .tool_calls
        .iter()
        .filter(|r| r.tool_name == FINALIZE_REPORT_NAME)
        .collect();
    assert_eq!(finalize_rows.len(), 2, "two finalize attempts recorded");
    assert!(finalize_rows[0].is_error, "first finalize was blocked");
    assert!(!finalize_rows[1].is_error, "second finalize succeeded");
}

/// Second-attempt escape hatch: if the agent insists on finalizing
/// without reflecting (pathological case), the rail force-injects
/// a stub critique so the run can terminate rather than spinning.
#[tokio::test]
async fn ordering_rail_force_injects_on_second_attempt() {
    use basilisk_agent::testing::build_test_runner_with_self_critique;

    let backend = Arc::new(MockLlmBackend::new("claude-opus-4-7"));

    // Turn 1: finalize without critique — nudged.
    backend.push(
        MockResponse::new()
            .finalize("tu_f1", "# go", Confidence::Medium, None)
            .usage(40, 10)
            .build(),
    );
    // Turn 2: stubbornly finalizes again without critique — rail
    // force-injects a synthetic critique and lets it through.
    backend.push(
        MockResponse::new()
            .finalize("tu_f2", "# still going", Confidence::Medium, None)
            .usage(40, 10)
            .build(),
    );

    let dir = tempfile::tempdir().unwrap();
    let runner =
        build_test_runner_with_self_critique(backend, dir.path().to_path_buf(), Budget::default());
    let outcome = runner
        .run("eth/0xdead", "audit", None)
        .await
        .expect("run ok");

    assert_eq!(outcome.stop_reason, AgentStopReason::ReportFinalized);
    let snap = runner.store().load_session(&outcome.session_id).unwrap();
    // The force-injected critique lives in session_feedback as a row.
    let n = runner
        .store()
        .count_feedback(&outcome.session_id, "self_critique")
        .unwrap();
    assert_eq!(n, 1, "rail injected one synthetic critique");
    // Two finalize attempts; first blocked, second allowed.
    let finalize_rows: Vec<_> = snap
        .tool_calls
        .iter()
        .filter(|r| r.tool_name == FINALIZE_REPORT_NAME)
        .collect();
    assert_eq!(finalize_rows.len(), 2);
    assert!(finalize_rows[0].is_error);
    assert!(!finalize_rows[1].is_error);
}

// --- Set 9.5 / CP9.5.8: --vuln integration test ----------------------------
//
// The smoke test that would have caught CP9.12 (registry/prompt swap missing
// from build_runner) and 8d67037 (initial-message asking for "reconnaissance"
// regardless of --vuln). Uses the real vuln_registry + VULN_V2_PROMPT, scripts
// the LLM through a representative phase-structured run, and asserts the
// critical-path wiring stays correct.

#[tokio::test]
async fn vuln_run_validates_prompt_registry_and_rail() {
    use basilisk_agent::testing::build_vuln_test_runner;
    use basilisk_agent::tools::{
        vuln_registry, FINALIZE_SELF_CRITIQUE_NAME, VULN_V2_PROMPT,
    };
    use sha2::{Digest, Sha256};

    // Redirect feedback JSONL writes to a tempdir so the test
    // doesn't pollute the operator's ~/.basilisk/feedback/.
    let feedback_dir = tempfile::tempdir().expect("feedback tempdir");
    std::env::set_var("BASILISK_FEEDBACK_DIR", feedback_dir.path());

    // 25 tools is the contract. Recon is 14, knowledge-enhanced 18,
    // vuln 25 (= 18 + 4 analytical + 3 self-critique).
    let reg = vuln_registry();
    assert_eq!(reg.len(), 25, "vuln_registry should have 25 tools");
    for required in [
        "classify_target",
        "resolve_onchain_system",
        "search_knowledge_base",
        "find_callers_of",
        "trace_state_dependencies",
        "simulate_call_chain",
        "build_and_run_foundry_test",
        "record_finding",
        "record_suspicion",
        "record_limitation",
        FINALIZE_SELF_CRITIQUE_NAME,
        "finalize_report",
    ] {
        assert!(
            reg.get(required).is_some(),
            "vuln_registry missing required tool {required}",
        );
    }

    let backend = Arc::new(MockLlmBackend::new("claude-sonnet-4-6"));

    // Turn 1 (assistant): premature finalize_report — rail should
    // intercept and nudge.
    backend.push(
        MockResponse::new()
            .finalize(
                "tu_premature",
                "# brief\nDone already!",
                Confidence::High,
                None,
            )
            .usage(60, 30)
            .build(),
    );
    // Turn 2 (assistant): respond to nudge with the prescribed
    // sequence — record_suspicion, record_finding,
    // finalize_self_critique, then finalize_report. All in one
    // assistant turn so dispatch order matters.
    backend.push(
        MockResponse::new()
            .tool_use(
                "tu_susp",
                "record_suspicion",
                serde_json::json!({
                    "description": "test suspicion: looks off in foo()",
                    "location": "Foo.sol:42",
                    "why_unconfirmed": "would need a fork PoC"
                }),
            )
            .tool_use(
                "tu_find",
                "record_finding",
                serde_json::json!({
                    "title": "Test finding",
                    "severity": "high",
                    "category": "reentrancy",
                    "summary": "test summary",
                    "target": "0xtest",
                    "confidence": "theoretical"
                }),
            )
            .tool_use(
                "tu_crit",
                FINALIZE_SELF_CRITIQUE_NAME,
                serde_json::json!({
                    "findings_quality_assessment": "one finding, one suspicion",
                    "methodology_gaps": "no fork sim",
                    "what_to_improve": "add simulate_call_chain on the suspicious path"
                }),
            )
            .finalize(
                "tu_final",
                "# Findings\n\nThe brief, properly framed.",
                Confidence::Medium,
                None,
            )
            .usage(200, 120)
            .build(),
    );

    let dir = tempfile::tempdir().unwrap();
    let runner = build_vuln_test_runner(backend, dir.path().to_path_buf(), Budget::default());
    let outcome = runner
        .run("0xtest-target", "audit", None)
        .await
        .expect("vuln run ok");

    // 1) Stop reason is success.
    assert_eq!(outcome.stop_reason, AgentStopReason::ReportFinalized);

    // 2) Prompt-hash assertion: the session row's prompt_hash must
    // match sha256(VULN_V2_PROMPT). This is the load-bearing check
    // that the prompt swap actually happened.
    let snap = runner.store().load_session(&outcome.session_id).unwrap();
    let mut hasher = Sha256::new();
    hasher.update(VULN_V2_PROMPT.as_bytes());
    let expected_hash = hex::encode(hasher.finalize());
    assert_eq!(
        snap.session.system_prompt_hash, expected_hash,
        "session prompt_hash does not match VULN_V2_PROMPT — registry/prompt swap regression",
    );

    // 3) Ordering rail fired. First finalize_report attempt should
    // be persisted as is_error=true; the eventual finalize_report
    // (after critique) is not_error.
    let finalize_rows: Vec<_> = snap
        .tool_calls
        .iter()
        .filter(|r| r.tool_name == "finalize_report")
        .collect();
    assert_eq!(finalize_rows.len(), 2, "expected 2 finalize_report attempts");
    assert!(
        finalize_rows[0].is_error,
        "first finalize_report should be blocked by rail",
    );
    assert!(
        !finalize_rows[1].is_error,
        "second finalize_report (after critique) should succeed",
    );

    // 4) Both record_finding and record_suspicion were called.
    let kinds: std::collections::HashSet<String> = snap
        .tool_calls
        .iter()
        .map(|r| r.tool_name.clone())
        .collect();
    assert!(kinds.contains("record_suspicion"));
    assert!(kinds.contains("record_finding"));
    assert!(kinds.contains(FINALIZE_SELF_CRITIQUE_NAME));

    // 5) session_feedback got the structured records (the moat-fix
    // assertion). Without the structured-recording channel actually
    // populating, the suspicion lives only in the markdown — Set 9
    // observed exactly this and Set 9.5 fixes it.
    let store = runner.store();
    let suspicions = store
        .count_feedback(&outcome.session_id, "suspicion")
        .unwrap();
    let critiques = store
        .count_feedback(&outcome.session_id, "self_critique")
        .unwrap();
    assert_eq!(suspicions, 1, "suspicion didn't reach session_feedback");
    assert_eq!(critiques, 1, "self_critique didn't reach session_feedback");
}
