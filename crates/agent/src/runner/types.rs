//! Vocabulary the [`AgentRunner`] (`CP5b`) and its loop body (`CP5c`)
//! share. Landed in `CP5a`; nothing here knows about tools, the LLM,
//! or the session store, so it stays cheap to round-trip through JSON
//! for `audit session show`.
//!
//! [`AgentRunner`]: super::AgentRunner

use std::time::{Duration, SystemTime};

use basilisk_llm::TokenUsage;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::tools::FinalReport;
use crate::SessionId;

/// Hard caps on what one agent run may consume.
///
/// Every cap is enforced independently. The first one tripped wins —
/// the loop stops and emits the matching [`AgentStopReason`]. Defaults
/// are conservative-but-not-stingy: enough for a non-trivial audit, not
/// so much that a runaway loop can rack up real money.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// Maximum number of LLM turns. One turn = one assistant response,
    /// regardless of how many tool calls it contains.
    pub max_turns: u32,
    /// Maximum total tokens across input + output + cache reads + cache
    /// writes. Coarse but cheap to check.
    pub max_tokens_total: u64,
    /// Maximum estimated spend in cents. Computed from
    /// [`basilisk_llm::PricingTable`]; if the model is unknown, the
    /// cost cap is effectively disabled.
    pub max_cost_cents: u32,
    /// Maximum wall-clock duration from [`AgentRunner::run`] entry to
    /// loop exit.
    ///
    /// [`AgentRunner::run`]: https://docs.rs/basilisk-agent (CP5b)
    pub max_duration: Duration,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_turns: 40,
            max_tokens_total: 500_000,
            max_cost_cents: 500,
            max_duration: Duration::from_secs(20 * 60),
        }
    }
}

/// Why the agent loop stopped.
///
/// Persisted as the lowercase tag (e.g. `"report_finalized"`) in the
/// `sessions.stop_reason` column so `audit session ls` can group + filter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentStopReason {
    /// Agent called `finalize_report`. The only "happy path" exit.
    ReportFinalized,
    /// Hit [`Budget::max_turns`] without finalising.
    TurnLimitReached,
    /// Cumulative token usage exceeded [`Budget::max_tokens_total`].
    TokenBudgetExhausted,
    /// Cumulative cost exceeded [`Budget::max_cost_cents`].
    CostBudgetExhausted,
    /// Wall-clock time exceeded [`Budget::max_duration`].
    DurationLimitReached,
    /// LLM backend returned an error the loop could not recover from
    /// (HTTP failure, malformed response, auth).
    LlmError { message: String },
    /// A tool call produced an error the loop classified as fatal —
    /// today that's only the unknown-tool case (`retryable: false`),
    /// since regular tool errors are surfaced to the agent as content
    /// for it to reason about.
    ToolError { tool: String, message: String },
    /// External cancellation (Ctrl-C, parent task drop).
    UserInterrupt,
}

impl AgentStopReason {
    /// Short tag persisted to `sessions.stop_reason`. Matches the
    /// `serde(rename_all = "snake_case")` output above.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::ReportFinalized => "report_finalized",
            Self::TurnLimitReached => "turn_limit_reached",
            Self::TokenBudgetExhausted => "token_budget_exhausted",
            Self::CostBudgetExhausted => "cost_budget_exhausted",
            Self::DurationLimitReached => "duration_limit_reached",
            Self::LlmError { .. } => "llm_error",
            Self::ToolError { .. } => "tool_error",
            Self::UserInterrupt => "user_interrupt",
        }
    }

    /// `true` when the loop exited because the agent finished its work
    /// cleanly. Maps to [`SessionStatus::Completed`]; everything else
    /// maps to [`SessionStatus::Failed`].
    ///
    /// [`SessionStatus::Completed`]: crate::SessionStatus::Completed
    /// [`SessionStatus::Failed`]: crate::SessionStatus::Failed
    pub fn is_success(&self) -> bool {
        matches!(self, Self::ReportFinalized)
    }
}

/// Cumulative usage across one agent run.
///
/// Serialised verbatim into the `sessions.stats_json` column so
/// `audit session show` can render it without re-deriving anything.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStats {
    /// Number of assistant turns the loop drove.
    pub turns: u32,
    /// Number of tool calls the agent issued.
    pub tool_calls: u32,
    /// Aggregated token usage across every turn.
    pub usage: TokenUsage,
    /// Estimated spend in cents. Zero when the model is missing from
    /// [`basilisk_llm::PricingTable`].
    pub cost_cents: u32,
    /// Wall-clock duration from loop entry to exit, in milliseconds.
    pub duration_ms: u64,
}

impl AgentStats {
    /// Total tokens (input + output + cache read + cache write).
    /// Mirrors [`TokenUsage::total`] but lives on the stats so the
    /// budget check can read one struct.
    pub fn total_tokens(&self) -> u64 {
        self.usage.total()
    }
}

/// What [`AgentRunner::run`] hands back when the loop exits.
///
/// Always present, regardless of stop reason. `final_report` is `Some`
/// only when [`AgentStopReason::ReportFinalized`] fired; every other
/// stop leaves it `None` so the caller can persist a partial transcript
/// and tell the user the run died early.
///
/// [`AgentRunner::run`]: https://docs.rs/basilisk-agent (CP5b)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentOutcome {
    pub session_id: SessionId,
    pub stop_reason: AgentStopReason,
    pub stats: AgentStats,
    /// Present iff the agent called `finalize_report`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_report: Option<FinalReport>,
    /// When the loop entered [`AgentRunner::run`].
    ///
    /// [`AgentRunner::run`]: https://docs.rs/basilisk-agent (CP5b)
    #[serde(with = "crate::session::time_serde")]
    pub started_at: SystemTime,
    /// When the loop exited.
    #[serde(with = "crate::session::time_serde")]
    pub ended_at: SystemTime,
}

/// Errors the runner can raise out of band of [`AgentStopReason`].
///
/// Anything in here aborts the loop *and* the session-store write —
/// distinct from [`AgentStopReason`] variants which represent a
/// graceful-but-unsuccessful end where everything was persisted.
#[derive(Debug, Error)]
pub enum AgentError {
    /// Persistence layer failed (DB locked, schema mismatch, JSON
    /// (de)serialisation). Loop cannot continue without a working
    /// store.
    #[error("session store error: {0}")]
    Session(#[from] crate::SessionError),
    /// Internal invariant violation — a bug, not a user error. The
    /// inner string explains what went wrong.
    #[error("internal: {0}")]
    Internal(String),
}

#[cfg(test)]
#[allow(clippy::match_wildcard_for_single_variants)]
mod tests {
    use super::*;

    #[test]
    fn budget_default_values_are_what_we_documented() {
        let b = Budget::default();
        assert_eq!(b.max_turns, 40);
        assert_eq!(b.max_tokens_total, 500_000);
        assert_eq!(b.max_cost_cents, 500);
        assert_eq!(b.max_duration, Duration::from_secs(1200));
    }

    #[test]
    fn budget_is_copy_and_round_trips_through_json() {
        let b = Budget::default();
        let s = serde_json::to_string(&b).unwrap();
        let parsed: Budget = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, b);
    }

    #[test]
    fn stop_reason_tags_match_serde_snake_case() {
        let cases = [
            (AgentStopReason::ReportFinalized, "report_finalized"),
            (AgentStopReason::TurnLimitReached, "turn_limit_reached"),
            (
                AgentStopReason::TokenBudgetExhausted,
                "token_budget_exhausted",
            ),
            (
                AgentStopReason::CostBudgetExhausted,
                "cost_budget_exhausted",
            ),
            (
                AgentStopReason::DurationLimitReached,
                "duration_limit_reached",
            ),
            (AgentStopReason::UserInterrupt, "user_interrupt"),
        ];
        for (r, tag) in cases {
            assert_eq!(r.tag(), tag);
            // serde tag matches our manual tag().
            let v = serde_json::to_value(&r).unwrap();
            assert_eq!(v["kind"], tag);
        }
    }

    #[test]
    fn stop_reason_payload_variants_serialise_their_fields() {
        let r = AgentStopReason::LlmError {
            message: "boom".into(),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["kind"], "llm_error");
        assert_eq!(v["message"], "boom");

        let r = AgentStopReason::ToolError {
            tool: "read_file".into(),
            message: "nope".into(),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["kind"], "tool_error");
        assert_eq!(v["tool"], "read_file");
        assert_eq!(v["message"], "nope");
    }

    #[test]
    fn stop_reason_is_success_only_for_finalized() {
        assert!(AgentStopReason::ReportFinalized.is_success());
        assert!(!AgentStopReason::TurnLimitReached.is_success());
        assert!(!AgentStopReason::TokenBudgetExhausted.is_success());
        assert!(!AgentStopReason::CostBudgetExhausted.is_success());
        assert!(!AgentStopReason::DurationLimitReached.is_success());
        assert!(!AgentStopReason::UserInterrupt.is_success());
        assert!(!AgentStopReason::LlmError {
            message: String::new()
        }
        .is_success());
        assert!(!AgentStopReason::ToolError {
            tool: String::new(),
            message: String::new()
        }
        .is_success());
    }

    #[test]
    fn stats_default_is_all_zero() {
        let s = AgentStats::default();
        assert_eq!(s.turns, 0);
        assert_eq!(s.tool_calls, 0);
        assert_eq!(s.cost_cents, 0);
        assert_eq!(s.duration_ms, 0);
        assert_eq!(s.total_tokens(), 0);
    }

    #[test]
    fn stats_total_tokens_sums_every_token_category() {
        let s = AgentStats {
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_input_tokens: Some(5),
                cache_creation_input_tokens: Some(7),
            },
            ..AgentStats::default()
        };
        assert_eq!(s.total_tokens(), 10 + 20 + 5 + 7);
    }

    #[test]
    fn outcome_round_trips_through_json() {
        use crate::tools::Confidence;
        let now = SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_000);
        let outcome = AgentOutcome {
            session_id: SessionId::new("s-1"),
            stop_reason: AgentStopReason::ReportFinalized,
            stats: AgentStats::default(),
            final_report: Some(FinalReport {
                markdown: "# report".into(),
                confidence: Confidence::Medium,
                notes: Some("careful".into()),
            }),
            started_at: now,
            ended_at: now + Duration::from_secs(5),
        };
        let s = serde_json::to_string(&outcome).unwrap();
        let parsed: AgentOutcome = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, outcome);
    }

    #[test]
    fn outcome_without_report_skips_field_in_json() {
        let now = SystemTime::UNIX_EPOCH;
        let outcome = AgentOutcome {
            session_id: SessionId::new("s-1"),
            stop_reason: AgentStopReason::TurnLimitReached,
            stats: AgentStats::default(),
            final_report: None,
            started_at: now,
            ended_at: now,
        };
        let v = serde_json::to_value(&outcome).unwrap();
        assert!(v.get("final_report").is_none());
    }
}
