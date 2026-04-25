//! Heuristic scoring of a [`BenchmarkRun`] against a
//! [`BenchmarkTarget`]'s expected findings.
//!
//! Matching rules (in order):
//!
//!   1. Every expected finding is tested against every agent finding.
//!      A pair matches when: any `must_mention` keyword appears in
//!      the agent's title / summary / reasoning / category (case-
//!      insensitive); none of the `must_not_mention_only` keywords
//!      dominate; and the agent's severity is ≥ `severity_min`.
//!   2. Each expected finding can be matched at most once (first hit
//!      wins). The agent's remaining findings count as false
//!      positives — unless their category matches any of the target's
//!      `vulnerability_classes`, in which case we treat them as
//!      plausibly-in-scope and don't flag as FP.
//!   3. `coverage_percent = matches / expected.len()` (0.0 if no
//!      expected findings; which shouldn't happen in practice).
//!
//! The matching is heuristic by design. `audit bench review <run-id>`
//! opens an interactive adjudication where the operator can override
//! automatic matches; per-target `notes` can flag known ambiguities.

use serde::{Deserialize, Serialize};

use crate::types::{AgentFindingSummary, BenchmarkRun, BenchmarkTarget, ExpectedFinding, Severity};

/// Score one run against its target.
pub fn score(target: &BenchmarkTarget, run: &BenchmarkRun) -> BenchmarkScore {
    let mut matches = Vec::new();
    let mut consumed = vec![false; run.agent_findings.len()];
    let mut misses = Vec::new();

    for expected in target.expected_findings {
        let mut matched_idx = None;
        for (i, agent) in run.agent_findings.iter().enumerate() {
            if consumed[i] {
                continue;
            }
            if finding_matches(expected, agent) {
                matched_idx = Some(i);
                break;
            }
        }
        if let Some(i) = matched_idx {
            consumed[i] = true;
            matches.push(FindingMatch {
                expected_class: expected.class.into(),
                agent_finding_title: run.agent_findings[i].title.clone(),
                agent_finding_severity: run.agent_findings[i].severity.clone(),
            });
        } else {
            misses.push(ExpectedMiss {
                class: expected.class.into(),
                must_mention: expected
                    .must_mention
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
                severity_min: expected.severity_min.as_str().to_string(),
            });
        }
    }

    let mut false_positives = Vec::new();
    for (i, agent) in run.agent_findings.iter().enumerate() {
        if consumed[i] {
            continue;
        }
        // Grace: if the agent's category matches one of the target's
        // declared vulnerability classes, the finding is at least
        // in-scope even if it didn't match a specific expected entry.
        // Those go to `plausible_extras` rather than `false_positives`.
        let in_scope = target
            .vulnerability_classes
            .iter()
            .any(|c| agent.category.to_ascii_lowercase().contains(*c));
        if in_scope {
            // Nothing to record at the score level — the human review
            // surface inspects each agent finding against every
            // expected one, and an "extra in-scope" finding may still
            // be a legitimate new catch.
        } else {
            false_positives.push(agent.clone());
        }
    }

    let expected_count = target.expected_findings.len().max(1);
    #[allow(clippy::cast_precision_loss)]
    let coverage_percent = matches.len() as f32 / expected_count as f32 * 100.0;

    BenchmarkScore {
        target_id: target.id.to_string(),
        target_name: target.name.to_string(),
        matches,
        misses,
        false_positives,
        coverage_percent,
    }
}

fn finding_matches(expected: &ExpectedFinding, agent: &AgentFindingSummary) -> bool {
    let haystack = format!(
        "{} {} {} {}",
        agent.title, agent.summary, agent.category, agent.severity,
    )
    .to_ascii_lowercase();
    // Severity check.
    if Severity::parse(&agent.severity) < expected.severity_min {
        return false;
    }
    // Disqualifiers: if the only thing in the agent finding is a
    // disqualifying keyword and nothing from `must_mention`, skip.
    let has_disqualifier = expected
        .must_not_mention_only
        .iter()
        .any(|k| haystack.contains(&k.to_ascii_lowercase()));
    let has_required = expected
        .must_mention
        .iter()
        .any(|k| haystack.contains(&k.to_ascii_lowercase()));
    if has_disqualifier && !has_required {
        return false;
    }
    has_required
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkScore {
    pub target_id: String,
    pub target_name: String,
    pub matches: Vec<FindingMatch>,
    pub misses: Vec<ExpectedMiss>,
    pub false_positives: Vec<AgentFindingSummary>,
    pub coverage_percent: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindingMatch {
    pub expected_class: String,
    pub agent_finding_title: String,
    pub agent_finding_severity: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedMiss {
    pub class: String,
    pub must_mention: Vec<String>,
    pub severity_min: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Severity;
    use std::time::Duration;

    fn mk_agent(title: &str, severity: &str, category: &str, summary: &str) -> AgentFindingSummary {
        AgentFindingSummary {
            title: title.into(),
            severity: severity.into(),
            category: category.into(),
            summary: summary.into(),
        }
    }

    fn mk_run(findings: Vec<AgentFindingSummary>) -> BenchmarkRun {
        BenchmarkRun {
            target_id: "test".into(),
            session_id: basilisk_agent::SessionId::new("sess"),
            agent_findings: findings,
            duration: Duration::ZERO,
            cost_cents: None,
            turns: 0,
            limitations_count: 0,
            suspicions_count: 0,
        }
    }

    static EXPECTED_ONE: [ExpectedFinding; 1] = [ExpectedFinding {
        class: "reentrancy",
        must_mention: &["reentrancy", "callback"],
        must_not_mention_only: &[],
        severity_min: Severity::High,
    }];

    static TARGET_ONE: BenchmarkTarget = BenchmarkTarget {
        id: "t1",
        name: "Test 1",
        chain: "ethereum",
        target_address: alloy_primitives::Address::ZERO,
        fork_block: 0,
        exploit_block: 0,
        vulnerability_classes: &["reentrancy"],
        severity: Severity::High,
        expected_findings: &EXPECTED_ONE,
        references: &[],
        notes: "",
    };

    #[test]
    fn matching_finding_passes_severity_and_keyword() {
        let run = mk_run(vec![mk_agent(
            "Reentrancy via ERC-777 hook",
            "high",
            "reentrancy",
            "callback re-enters transfer",
        )]);
        let s = score(&TARGET_ONE, &run);
        assert_eq!(s.matches.len(), 1);
        assert!(s.misses.is_empty());
        assert!((s.coverage_percent - 100.0).abs() < 0.01);
    }

    #[test]
    fn too_low_severity_does_not_match() {
        let run = mk_run(vec![mk_agent(
            "Reentrancy concern",
            "low",
            "reentrancy",
            "callback exists",
        )]);
        let s = score(&TARGET_ONE, &run);
        assert!(s.matches.is_empty());
        assert_eq!(s.misses.len(), 1);
    }

    #[test]
    fn missing_keyword_does_not_match() {
        let run = mk_run(vec![mk_agent(
            "Oracle staleness",
            "high",
            "oracle",
            "price feed stale",
        )]);
        let s = score(&TARGET_ONE, &run);
        assert!(s.matches.is_empty());
        // Different category from target's vulnerability_classes →
        // false positive.
        assert_eq!(s.false_positives.len(), 1);
    }

    #[test]
    fn in_scope_extra_finding_does_not_count_as_false_positive() {
        let run = mk_run(vec![
            mk_agent(
                "Reentrancy via ERC-777 hook",
                "high",
                "reentrancy",
                "callback re-enters transfer",
            ),
            mk_agent(
                "Another reentrancy issue",
                "medium",
                "reentrancy",
                "different callback",
            ),
        ]);
        let s = score(&TARGET_ONE, &run);
        assert_eq!(s.matches.len(), 1);
        // Second reentrancy is in-scope; no false positive recorded.
        assert!(s.false_positives.is_empty());
    }

    #[test]
    fn coverage_percent_is_zero_when_no_matches() {
        let run = mk_run(vec![mk_agent(
            "Something unrelated",
            "high",
            "gas_optimization",
            "loops are inefficient",
        )]);
        let s = score(&TARGET_ONE, &run);
        assert!(s.matches.is_empty());
        assert!(s.coverage_percent.abs() < 0.01);
    }

    #[test]
    fn empty_run_produces_full_miss_set() {
        let run = mk_run(vec![]);
        let s = score(&TARGET_ONE, &run);
        assert!(s.matches.is_empty());
        assert_eq!(s.misses.len(), TARGET_ONE.expected_findings.len());
    }

    #[test]
    fn severity_ordering_respects_enum() {
        assert!(Severity::Critical > Severity::High);
        assert!(Severity::High > Severity::Medium);
        assert!(Severity::Medium > Severity::Low);
        assert!(Severity::Low > Severity::Informational);
    }

    #[test]
    fn severity_parse_is_case_insensitive() {
        assert_eq!(Severity::parse("HIGH"), Severity::High);
        assert_eq!(Severity::parse("critical"), Severity::Critical);
        assert_eq!(Severity::parse("med"), Severity::Medium);
        assert_eq!(Severity::parse("nonesuch"), Severity::Informational);
    }
}
