//! `finalize_report` — the agent's done signal.
//!
//! When called, the tool returns an `Ok` payload describing the final
//! report. The agent loop (CP5) recognises this tool by name, extracts
//! the payload as the session's final output, and stops the loop. For
//! CP3 the tool is a regular tool implementation — the interception
//! logic lives where the loop lives.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::tool::{Tool, ToolContext, ToolResult};

/// Canonical tool name. Kept as a `pub const` so CP5's loop can
/// compare against `finalize_report::NAME` without hardcoding the
/// string literal in two places.
pub const NAME: &str = "finalize_report";

/// The payload shape the agent writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalReport {
    pub markdown: String,
    pub confidence: Confidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

pub struct FinalizeReport;

#[derive(Deserialize)]
struct Input {
    markdown: String,
    confidence: Confidence,
    #[serde(default)]
    notes: Option<String>,
}

#[async_trait]
impl Tool for FinalizeReport {
    fn name(&self) -> &'static str {
        NAME
    }

    fn description(&self) -> &'static str {
        "Signal that the reconnaissance is complete. Submits your final markdown brief, a \
         confidence level, and optional notes for human review. Calling this tool stops the \
         session — once you've got enough information to write a useful brief, call this and \
         don't keep exploring. A good recon brief is 300-800 words for a simple target, \
         1500-2500 for a complex one."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["markdown", "confidence"],
            "properties": {
                "markdown": {
                    "type": "string",
                    "description": "The recon brief as markdown. Suggested sections: summary, system map, key contracts/files, notable patterns, scoping notes, open questions."
                },
                "confidence": {
                    "type": "string",
                    "enum": ["high", "medium", "low"],
                    "description": "How confident you are in the characterisation. Use `low` when key information was unavailable (unverified contract, RPC range limits, unresolved imports)."
                },
                "notes": {
                    "type": "string",
                    "description": "Optional free-form notes for the human reviewer — caveats, things you'd want to check if you had more budget, specific files/addresses to look at."
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let parsed: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::err(format!("invalid input: {e}"), false),
        };
        if parsed.markdown.trim().is_empty() {
            return ToolResult::err("markdown is empty; nothing to report", false);
        }
        let report = FinalReport {
            markdown: parsed.markdown,
            confidence: parsed.confidence,
            notes: parsed.notes,
        };
        ToolResult::ok(report)
    }
}

#[cfg(test)]
#[allow(clippy::match_wildcard_for_single_variants)]
mod tests {
    use super::*;

    async fn run(input: serde_json::Value) -> ToolResult {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::test(dir.path().to_path_buf());
        std::mem::forget(dir);
        FinalizeReport.execute(input, &ctx).await
    }

    #[tokio::test]
    async fn accepts_valid_report() {
        let res = run(serde_json::json!({
            "markdown": "# Summary\n\nThis is the report.",
            "confidence": "high",
            "notes": "double-check the admin key",
        }))
        .await;
        match res {
            ToolResult::Ok(v) => {
                assert_eq!(v["confidence"], "high");
                assert!(v["markdown"].as_str().unwrap().contains("Summary"));
                assert!(v["notes"].as_str().is_some());
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_markdown_rejected_as_non_retryable() {
        let res = run(serde_json::json!({
            "markdown": "   \n\t  ",
            "confidence": "medium",
        }))
        .await;
        match res {
            ToolResult::Err { message, retryable } => {
                assert!(message.contains("empty"));
                assert!(!retryable);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_confidence_is_non_retryable() {
        let res = run(serde_json::json!({
            "markdown": "hi",
        }))
        .await;
        match res {
            ToolResult::Err { retryable, .. } => assert!(!retryable),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn name_matches_exported_const() {
        assert_eq!(FinalizeReport.name(), NAME);
    }
}
