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
use basilisk_agent::{AgentStopReason, Budget, SessionStatus, TurnRole};
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
async fn end_turn_without_tool_call_is_surfaced_as_llm_error() {
    let backend = Arc::new(MockLlmBackend::new("claude-opus-4-7"));
    // No tool_use, just text → agent "gives up" mid-audit.
    backend.push(
        MockResponse::new()
            .text("I'm not sure what to do here.")
            .usage(30, 10)
            .build(),
    );

    let (runner, _dir) = make_runner(backend, Budget::default());
    let outcome = runner.run("eth/0x", "go", None).await.unwrap();

    match outcome.stop_reason {
        AgentStopReason::LlmError { ref message } => {
            assert!(
                message.contains("without calling a tool"),
                "got {message:?}"
            );
        }
        other => panic!("got {other:?}"),
    }
    assert_eq!(outcome.stats.turns, 1);
    assert_eq!(outcome.stats.tool_calls, 0);
    assert!(outcome.final_report.is_none());
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
