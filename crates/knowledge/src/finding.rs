//! [`FindingRecord`], [`Correction`], and related types.
//!
//! Shape:
//!  - `FindingRecord` — what the agent produces and
//!    [`KnowledgeBase::record_finding`] stores.
//!  - `Correction` — what the user attaches after-the-fact
//!    ([`KnowledgeBase::record_correction`]).
//!  - `UserVerdict` — "confirmed" / "dismissed" / "corrected";
//!    drives retrieval weighting in Set 9's reasoning pass.
//!
//! Storage: one `user_findings` `LanceDB` row per finding AND one
//! per correction. Correction rows carry `is_correction = true`
//! and `corrects_id = Some(<target>)`; retrieval joins the two
//! in `KnowledgeBase::search` so callers always see the
//! correction alongside the finding it corrects.
//!
//! [`KnowledgeBase::record_finding`]: crate::knowledge_base::KnowledgeBase::record_finding
//! [`KnowledgeBase::record_correction`]: crate::knowledge_base::KnowledgeBase::record_correction

use serde::{Deserialize, Serialize};

/// Stable id for a finding. Derived from
/// `sha256(session_id | finding_title | target)` in
/// [`KnowledgeBase::record_finding`]; carried on the CLI via
/// `audit knowledge correct <id>` / `show <id>` / etc.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FindingId(pub String);

impl FindingId {
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for FindingId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Shape of a finding the agent produces (via the
/// `record_finding` tool in CP7.9) or a human records via the
/// CLI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FindingRecord {
    pub title: String,
    pub severity: String,
    pub category: String,
    pub summary: String,
    pub vulnerable_code: Option<String>,
    pub location: Option<FindingLocation>,
    pub reasoning: Option<String>,
    #[serde(default)]
    pub related_findings: Vec<String>,
    pub poc_sketch: Option<String>,
}

impl FindingRecord {
    /// Text fed to the embedding provider. Title + summary +
    /// reasoning + vulnerable code are the signal the model
    /// should retrieve against; `related_findings` and `poc_sketch`
    /// stay in metadata.
    #[must_use]
    pub fn embed_text(&self) -> String {
        let mut s = String::new();
        s.push_str(&self.title);
        s.push_str("\n\n");
        s.push_str(&self.summary);
        if let Some(r) = &self.reasoning {
            s.push_str("\n\nReasoning:\n");
            s.push_str(r);
        }
        if let Some(code) = &self.vulnerable_code {
            s.push_str("\n\nVulnerable code:\n");
            s.push_str(code);
        }
        s
    }
}

/// Where the finding lives. For on-chain targets, `file` is the
/// address string and `contract` is the contract name (if known);
/// `line_range` is `None`. For source-side targets, `file` is the
/// path and `line_range` is `Some((start, end))`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindingLocation {
    pub file: String,
    pub line_range: Option<(u32, u32)>,
    pub function: Option<String>,
    pub contract: Option<String>,
}

/// Human-supplied correction to a prior finding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Correction {
    /// Why the original was wrong.
    pub reason: String,
    /// Optional corrected severity.
    pub corrected_severity: Option<String>,
    /// Optional corrected category.
    pub corrected_category: Option<String>,
}

/// Human verdict on a recorded finding.
///
/// `Confirmed` = reviewer agreed the finding is real.
/// `Dismissed` = false positive; agent should weight similar
/// patterns *down* next time. `Corrected` = partially wrong; the
/// correction reason explains what was off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserVerdict {
    Confirmed,
    Dismissed,
    Corrected,
}

impl UserVerdict {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Dismissed => "dismissed",
            Self::Corrected => "corrected",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finding_id_round_trips_through_string() {
        let id = FindingId::new("abc123");
        assert_eq!(id.as_str(), "abc123");
        assert_eq!(id.to_string(), "abc123");
    }

    #[test]
    fn finding_id_round_trips_through_json() {
        let id = FindingId::new("abc");
        let j = serde_json::to_string(&id).unwrap();
        assert_eq!(j, "\"abc\"");
        let back: FindingId = serde_json::from_str(&j).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn embed_text_includes_title_summary_reasoning_and_code() {
        let r = FindingRecord {
            title: "Reentrancy in withdraw".into(),
            severity: "high".into(),
            category: "reentrancy".into(),
            summary: "Attacker can re-enter during withdrawal".into(),
            vulnerable_code: Some("function withdraw() { ... }".into()),
            location: None,
            reasoning: Some("The balance update happens after the external call".into()),
            related_findings: vec![],
            poc_sketch: None,
        };
        let t = r.embed_text();
        assert!(t.contains("Reentrancy"));
        assert!(t.contains("Attacker"));
        assert!(t.contains("Reasoning:"));
        assert!(t.contains("balance update"));
        assert!(t.contains("Vulnerable code:"));
        assert!(t.contains("withdraw"));
    }

    #[test]
    fn embed_text_omits_empty_sections() {
        let r = FindingRecord {
            title: "T".into(),
            severity: "low".into(),
            category: "c".into(),
            summary: "S".into(),
            vulnerable_code: None,
            location: None,
            reasoning: None,
            related_findings: vec![],
            poc_sketch: None,
        };
        let t = r.embed_text();
        assert!(!t.contains("Reasoning:"));
        assert!(!t.contains("Vulnerable code:"));
    }

    #[test]
    fn verdict_string_roundtrip_through_json() {
        for v in [
            UserVerdict::Confirmed,
            UserVerdict::Dismissed,
            UserVerdict::Corrected,
        ] {
            let j = serde_json::to_string(&v).unwrap();
            let back: UserVerdict = serde_json::from_str(&j).unwrap();
            assert_eq!(v, back);
            assert_eq!(j, format!("\"{}\"", v.as_str()));
        }
    }
}
