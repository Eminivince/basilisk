//! Self-critique tools — the third arm of Set 9's "make the agent
//! honest about its own limits" infrastructure. Three tools:
//!
//!   - [`RecordLimitation`] — the agent documents a wall it hit (no
//!     tool for X; unverified contract; undocumented protocol intent).
//!   - [`RecordSuspicion`] — the agent surfaces a hunch it can't
//!     elevate to a finding.
//!   - [`FinalizeSelfCritique`] — a mandatory reflection step before
//!     `finalize_report`. The agent summarises findings quality,
//!     methodology gaps, and what it would improve. The ordering
//!     rail (CP9.7) refuses `finalize_report` without it.
//!
//! Each tool persists to two places:
//!   - The session's `session_feedback` table (for in-session
//!     querying + scripted review).
//!   - An operator-readable JSONL file under
//!     `~/.basilisk/feedback/<kind>.jsonl` so the operator can grep
//!     across sessions. Top-of-mind line of evidence for "what
//!     should Set 10 build?"

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::tool::{Tool, ToolContext, ToolResult};

pub const RECORD_LIMITATION_NAME: &str = "record_limitation";
pub const RECORD_SUSPICION_NAME: &str = "record_suspicion";
pub const FINALIZE_SELF_CRITIQUE_NAME: &str = "finalize_self_critique";

// ---------- payload types ---------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitationRecord {
    pub description: String,
    pub what_was_attempted: String,
    pub what_would_help: String,
    pub severity: LimitationSeverity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LimitationSeverity {
    Minor,
    Moderate,
    Blocking,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuspicionRecord {
    pub description: String,
    pub location: String,
    pub why_unconfirmed: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfCritiqueRecord {
    pub findings_quality_assessment: String,
    pub methodology_gaps: String,
    pub what_to_improve: String,
}

// ---------- tools -----------------------------------------------------

pub struct RecordLimitation;

#[async_trait]
impl Tool for RecordLimitation {
    fn name(&self) -> &'static str {
        RECORD_LIMITATION_NAME
    }

    fn description(&self) -> &'static str {
        "Call this whenever a tool returned less than you needed, a data source was unavailable, \
         or a capability didn't exist. These are not failures — they are the tool's roadmap. Be \
         specific: what did you want to do, what stopped you, what would have helped? Every \
         limitation noted today shapes what Basilisk gains tomorrow. A run with zero limitations \
         on a complex target almost always means the agent suppressed them — limitations \
         mentioned only in the final markdown are invisible to the development loop, so surface \
         them here even when they feel small."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["description", "what_was_attempted", "what_would_help", "severity"],
            "properties": {
                "description": {"type": "string", "description": "What you wanted to know or do."},
                "what_was_attempted": {"type": "string", "description": "What you tried (tools, approaches, retrievals)."},
                "what_would_help": {"type": "string", "description": "A tool, data source, or capability that would have unblocked you."},
                "severity": {"type": "string", "enum": ["minor", "moderate", "blocking"]}
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let rec: LimitationRecord = match serde_json::from_value(input) {
            Ok(r) => r,
            Err(e) => return ToolResult::err(format!("invalid input: {e}"), false),
        };
        let wire = rec.to_wire(&ctx.session_id.0);
        persist_feedback(ctx, "limitation", &wire);
        append_feedback_jsonl("limitations.jsonl", ctx, &wire);
        ToolResult::ok(serde_json::json!({
            "recorded": true,
            "kind": "limitation",
            "session_id": ctx.session_id.0,
        }))
    }
}

pub struct RecordSuspicion;

#[async_trait]
impl Tool for RecordSuspicion {
    fn name(&self) -> &'static str {
        RECORD_SUSPICION_NAME
    }

    fn description(&self) -> &'static str {
        "Use this whenever you've identified something that looks off but you can't confirm \
         it's a bug. Suspicions are not weak findings — they are structurally different. They \
         say 'something here deserves attention, and here's why I can't finish the investigation \
         myself.' Surface generously. A run with zero suspicions on a complex target means the \
         agent didn't try. Suspicions written into the final markdown report instead of through \
         this tool are invisible to Basilisk's memory and cannot be retrieved by future audits — \
         use this tool, then optionally also mention them in the report."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["description", "location", "why_unconfirmed"],
            "properties": {
                "description": {"type": "string", "description": "What you suspect might be wrong."},
                "location": {"type": "string", "description": "Where — contract address, file:line, function name."},
                "why_unconfirmed": {"type": "string", "description": "What's preventing you from elevating this to a finding (missing data, need a PoC you can't build, etc.)."}
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let rec: SuspicionRecord = match serde_json::from_value(input) {
            Ok(r) => r,
            Err(e) => return ToolResult::err(format!("invalid input: {e}"), false),
        };
        let wire = rec.to_wire(&ctx.session_id.0);
        persist_feedback(ctx, "suspicion", &wire);
        append_feedback_jsonl("suspicions.jsonl", ctx, &wire);
        ToolResult::ok(serde_json::json!({
            "recorded": true,
            "kind": "suspicion",
            "session_id": ctx.session_id.0,
        }))
    }
}

pub struct FinalizeSelfCritique;

#[async_trait]
impl Tool for FinalizeSelfCritique {
    fn name(&self) -> &'static str {
        FINALIZE_SELF_CRITIQUE_NAME
    }

    fn description(&self) -> &'static str {
        "Reflect on this audit before submitting the final report. You MUST call this before \
         `finalize_report`; the runner enforces the ordering. Three honest questions to answer: \
         how good are your findings, where did your methodology come up short, and what would \
         you do differently next time against a similar target."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["findings_quality_assessment", "methodology_gaps", "what_to_improve"],
            "properties": {
                "findings_quality_assessment": {
                    "type": "string",
                    "description": "How strong is each finding? Which ones do you trust the most, which the least, and why? If you recorded no findings, say that and justify."
                },
                "methodology_gaps": {
                    "type": "string",
                    "description": "Where did you take shortcuts, skip investigation, or make assumptions that a senior auditor would push back on?"
                },
                "what_to_improve": {
                    "type": "string",
                    "description": "Concrete: tools/retrievals/patterns you'd use differently on a comparable target."
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let rec: SelfCritiqueRecord = match serde_json::from_value(input) {
            Ok(r) => r,
            Err(e) => return ToolResult::err(format!("invalid input: {e}"), false),
        };
        let wire = rec.to_wire(&ctx.session_id.0);
        persist_feedback(ctx, "self_critique", &wire);
        append_feedback_jsonl("self_critiques.jsonl", ctx, &wire);
        ToolResult::ok(serde_json::json!({
            "recorded": true,
            "kind": "self_critique",
            "session_id": ctx.session_id.0,
        }))
    }
}

// ---------- persistence helpers ---------------------------------------

impl LimitationRecord {
    fn to_wire(&self, session_id: &str) -> serde_json::Value {
        serde_json::json!({
            "session_id": session_id,
            "recorded_at_ms": now_ms(),
            "kind": "limitation",
            "description": self.description,
            "what_was_attempted": self.what_was_attempted,
            "what_would_help": self.what_would_help,
            "severity": match self.severity {
                LimitationSeverity::Minor => "minor",
                LimitationSeverity::Moderate => "moderate",
                LimitationSeverity::Blocking => "blocking",
            },
        })
    }
}

impl SuspicionRecord {
    fn to_wire(&self, session_id: &str) -> serde_json::Value {
        serde_json::json!({
            "session_id": session_id,
            "recorded_at_ms": now_ms(),
            "kind": "suspicion",
            "description": self.description,
            "location": self.location,
            "why_unconfirmed": self.why_unconfirmed,
        })
    }
}

impl SelfCritiqueRecord {
    fn to_wire(&self, session_id: &str) -> serde_json::Value {
        serde_json::json!({
            "session_id": session_id,
            "recorded_at_ms": now_ms(),
            "kind": "self_critique",
            "findings_quality_assessment": self.findings_quality_assessment,
            "methodology_gaps": self.methodology_gaps,
            "what_to_improve": self.what_to_improve,
        })
    }
}

/// Feedback directory — `~/.basilisk/feedback/`. Overridable via
/// `BASILISK_FEEDBACK_DIR` for tests.
fn feedback_dir() -> Option<PathBuf> {
    if let Ok(s) = std::env::var("BASILISK_FEEDBACK_DIR") {
        if !s.trim().is_empty() {
            return Some(PathBuf::from(s));
        }
    }
    let home = dirs::home_dir()?;
    Some(home.join(".basilisk").join("feedback"))
}

/// Best-effort DB write via `SessionStore::record_feedback`. The
/// JSONL append runs regardless so operators always have a visible
/// trail; if the DB write fails we log and move on — the rail in
/// the runner is the load-bearing check.
fn persist_feedback(ctx: &ToolContext, kind: &str, wire: &serde_json::Value) {
    let Some(store) = ctx.session_store.as_ref() else {
        return;
    };
    let Ok(payload) = serde_json::to_string(wire) else {
        return;
    };
    if let Err(e) = store.record_feedback(&ctx.session_id, kind, &payload) {
        warn!(error = %e, "failed to record feedback to session DB");
    }
}

fn append_feedback_jsonl(file_name: &str, _ctx: &ToolContext, wire: &serde_json::Value) {
    use std::io::Write;
    let Some(dir) = feedback_dir() else {
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!(error = %e, "failed to create feedback dir");
        return;
    }
    let path = dir.join(file_name);
    let line = match serde_json::to_string(wire) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "failed to serialise feedback");
            return;
        }
    };
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(mut f) => {
            if let Err(e) = writeln!(f, "{line}") {
                warn!(error = %e, "failed to append feedback");
            }
        }
        Err(e) => {
            warn!(error = %e, path = %path.display(), "failed to open feedback file");
        }
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    // Tests here mutate a process-wide env var (BASILISK_FEEDBACK_DIR).
    // Serialise them so parallel runs don't race each other — one
    // test's `remove_var` can otherwise land between another test's
    // `set_var` and its file read.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn ctx() -> ToolContext {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::test(dir.path().to_path_buf());
        std::mem::forget(dir);
        ctx
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Runtime::new().unwrap().block_on(f)
    }

    #[test]
    fn limitation_schema_rejects_missing_fields() {
        let t = RecordLimitation;
        let r = block_on(t.execute(serde_json::json!({"description": "x"}), &ctx()));
        assert!(matches!(r, ToolResult::Err { .. }));
    }

    #[test]
    fn limitation_happy_path_appends_jsonl() {
        let _g = ENV_LOCK.lock().unwrap();
        let feedback_dir = tempdir().unwrap();
        std::env::set_var("BASILISK_FEEDBACK_DIR", feedback_dir.path());
        let c = ctx();
        let t = RecordLimitation;
        let r = block_on(t.execute(
            serde_json::json!({
                "description": "wanted decompiled bytecode",
                "what_was_attempted": "tried resolve_onchain_contract",
                "what_would_help": "decompile_bytecode tool",
                "severity": "moderate"
            }),
            &c,
        ));
        match r {
            ToolResult::Ok(v) => {
                assert_eq!(v["kind"], "limitation");
                assert_eq!(v["session_id"], c.session_id.0);
            }
            ToolResult::Err { message, .. } => panic!("got error: {message}"),
        }
        let body = std::fs::read_to_string(feedback_dir.path().join("limitations.jsonl")).unwrap();
        assert!(body.contains("decompiled bytecode"));
        assert!(body.contains("\"severity\":\"moderate\""));
        std::env::remove_var("BASILISK_FEEDBACK_DIR");
    }

    #[test]
    fn suspicion_round_trip() {
        let _g = ENV_LOCK.lock().unwrap();
        let feedback_dir = tempdir().unwrap();
        std::env::set_var("BASILISK_FEEDBACK_DIR", feedback_dir.path());
        let c = ctx();
        let t = RecordSuspicion;
        let r = block_on(t.execute(
            serde_json::json!({
                "description": "possibly reentrancy via ERC-777 hook",
                "location": "0xabc...:transfer",
                "why_unconfirmed": "no test harness for ERC-777 tokens"
            }),
            &c,
        ));
        assert!(matches!(r, ToolResult::Ok(_)));
        let body = std::fs::read_to_string(feedback_dir.path().join("suspicions.jsonl")).unwrap();
        assert!(body.contains("ERC-777"));
        std::env::remove_var("BASILISK_FEEDBACK_DIR");
    }

    #[test]
    fn self_critique_round_trip() {
        let _g = ENV_LOCK.lock().unwrap();
        let feedback_dir = tempdir().unwrap();
        std::env::set_var("BASILISK_FEEDBACK_DIR", feedback_dir.path());
        let c = ctx();
        let t = FinalizeSelfCritique;
        let r = block_on(t.execute(
            serde_json::json!({
                "findings_quality_assessment": "two findings are solid; one is speculative",
                "methodology_gaps": "didn't test governance path thoroughly",
                "what_to_improve": "run simulate_call_chain on the exec path before finalizing"
            }),
            &c,
        ));
        assert!(matches!(r, ToolResult::Ok(_)));
        let body =
            std::fs::read_to_string(feedback_dir.path().join("self_critiques.jsonl")).unwrap();
        assert!(body.contains("methodology_gaps"));
        std::env::remove_var("BASILISK_FEEDBACK_DIR");
    }

    #[test]
    fn schemas_include_all_required_fields() {
        let l = RecordLimitation.input_schema();
        let s = RecordSuspicion.input_schema();
        let c = FinalizeSelfCritique.input_schema();
        for schema in [l, s, c] {
            assert_eq!(schema["type"], "object");
            assert!(schema["required"].as_array().is_some());
            assert!(schema["properties"].as_object().is_some());
        }
    }

    #[test]
    fn names_match_constants() {
        assert_eq!(RecordLimitation.name(), RECORD_LIMITATION_NAME);
        assert_eq!(RecordSuspicion.name(), RECORD_SUSPICION_NAME);
        assert_eq!(FinalizeSelfCritique.name(), FINALIZE_SELF_CRITIQUE_NAME);
    }

    // The Arc import silences "unused" when running tests without
    // the registry-composition helper touching Arc.
    fn _silence_unused() {
        let _ = Arc::new(());
    }
}
