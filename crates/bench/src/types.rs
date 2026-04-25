//! Benchmark data types.

use std::time::Duration;

use alloy_primitives::Address;
use basilisk_agent::SessionId;
use serde::{Deserialize, Serialize};

/// One benchmark target: a real, public post-exploit protocol.
///
/// Derives only `Serialize` — targets are defined in code (static
/// slices of `&'static str`), never loaded from disk, so
/// `Deserialize` would require a parallel owned-string variant that
/// buys nothing.
#[derive(Debug, Clone, Serialize)]
pub struct BenchmarkTarget {
    /// Short stable id — used in CLI + history queries.
    pub id: &'static str,
    pub name: &'static str,
    pub chain: &'static str,
    /// Primary contract the exploit targeted. Additional contracts
    /// the agent must discover via `resolve_onchain_system` from
    /// this root.
    pub target_address: Address,
    /// Block to fork from — immediately before the exploit. The
    /// agent should see the code as it looked when vulnerable.
    pub fork_block: u64,
    /// Block where the exploit landed (for reference; the agent
    /// doesn't run against post-exploit state).
    pub exploit_block: u64,
    /// Vulnerability classes this target exercises. Used by the
    /// scorer's keyword matcher and by the CLI for grouping.
    pub vulnerability_classes: &'static [&'static str],
    pub severity: Severity,
    pub expected_findings: &'static [ExpectedFinding],
    /// Post-mortem URLs for human reference.
    pub references: &'static [&'static str],
    /// Anything an evaluator should know (reproducibility caveats,
    /// known variance, etc.).
    pub notes: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Informational,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "critical" => Self::Critical,
            "high" => Self::High,
            "medium" | "med" => Self::Medium,
            "low" => Self::Low,
            _ => Self::Informational,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
            Self::Informational => "info",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ExpectedFinding {
    /// Class label — matches one of the target's
    /// `vulnerability_classes`.
    pub class: &'static str,
    /// Keywords that should appear in a finding's title/summary/
    /// reasoning for it to count.
    pub must_mention: &'static [&'static str],
    /// Keywords that disqualify a match — e.g. a finding about
    /// reentrancy when this isn't a reentrancy bug.
    pub must_not_mention_only: &'static [&'static str],
    /// Minimum severity the agent must assign.
    pub severity_min: Severity,
}

/// The record produced by running against a target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkRun {
    pub target_id: String,
    pub session_id: SessionId,
    pub agent_findings: Vec<AgentFindingSummary>,
    #[serde(with = "duration_serde")]
    pub duration: Duration,
    pub cost_cents: Option<u32>,
    pub turns: u32,
    pub limitations_count: u32,
    pub suspicions_count: u32,
}

/// A trimmed summary of one agent finding — bench scoring needs the
/// title / summary / severity / category to match, not the full
/// finding record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentFindingSummary {
    pub title: String,
    pub severity: String,
    pub category: String,
    pub summary: String,
}

mod duration_serde {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        u64::try_from(d.as_millis()).unwrap_or(u64::MAX).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let ms = u64::deserialize(d)?;
        Ok(Duration::from_millis(ms))
    }
}
