//! Typed records the [`SessionStore`] (`CP4b`/`CP4c`) reads and writes.
//!
//! Each record mirrors exactly one row in `schema.sql`. Enum variants
//! serialise as lowercase tags — the same string goes to the `status`
//! / `stop_reason` / `role` columns — so the DB is inspectable with
//! plain `sqlite3 basilisk.db`.
//!
//! `CP4a` wires the types; `CP4b` adds the methods that persist them.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Where a session is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    /// Actively running. On next startup, stale `Running` rows are
    /// moved to [`Self::Interrupted`] — the agent either crashed, was
    /// killed, or lost power.
    Running,
    /// Agent called `finalize_report` and the loop exited cleanly.
    Completed,
    /// Loop exited with a non-finalize stop — turn limit, cost cap,
    /// unrecoverable tool error, LLM error.
    Failed,
    /// `Running` session was rescued from a prior run. Resume or delete.
    Interrupted,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "interrupted" => Self::Interrupted,
            _ => return None,
        })
    }
}

/// Which side of the conversation a turn belongs to.
///
/// Mirrors [`basilisk_llm::MessageRole`] but lives here to avoid a
/// forced dependency direction for downstream code that only wants to
/// read session records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnRole {
    User,
    Assistant,
}

impl TurnRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "user" => Self::User,
            "assistant" => Self::Assistant,
            _ => return None,
        })
    }
}

/// One row of `sessions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    #[serde(with = "crate::session::time_serde")]
    pub created_at: SystemTime,
    #[serde(with = "crate::session::time_serde")]
    pub updated_at: SystemTime,
    pub target: String,
    pub model: String,
    pub system_prompt_hash: String,
    pub status: SessionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_report_markdown: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_confidence: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Opaque JSON blob. `CP5`'s `AgentStats` shape lands here; `CP4`
    /// doesn't know the concrete structure.
    pub stats: serde_json::Value,
}

/// One row of `turns`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnRecord {
    pub session_id: String,
    pub turn_index: u32,
    pub role: TurnRole,
    /// Serialised `Vec<basilisk_llm::ContentBlock>`. Plain JSON so the
    /// DB is inspectable and the `CP4` persistence layer doesn't need
    /// to know the LLM crate's types.
    pub content: serde_json::Value,
    pub tokens_in: Option<u32>,
    pub tokens_out: Option<u32>,
    #[serde(with = "crate::session::time_serde")]
    pub started_at: SystemTime,
    #[serde(with = "crate::session::time_serde")]
    pub ended_at: SystemTime,
}

/// One row of `tool_calls`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub session_id: String,
    pub turn_index: u32,
    pub call_index: u32,
    pub tool_use_id: String,
    pub tool_name: String,
    pub input: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
    pub is_error: bool,
    pub duration_ms: u64,
}

/// Summary row for `list_sessions`. Cheaper than full `SessionRecord`
/// because it skips the stats blob + final report markdown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    #[serde(with = "crate::session::time_serde")]
    pub created_at: SystemTime,
    pub target: String,
    pub model: String,
    pub status: SessionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_confidence: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_status_round_trips() {
        for s in [
            SessionStatus::Running,
            SessionStatus::Completed,
            SessionStatus::Failed,
            SessionStatus::Interrupted,
        ] {
            assert_eq!(SessionStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(SessionStatus::parse("nonsense"), None);
    }

    #[test]
    fn turn_role_round_trips() {
        assert_eq!(TurnRole::parse("user"), Some(TurnRole::User));
        assert_eq!(TurnRole::parse("assistant"), Some(TurnRole::Assistant));
        assert_eq!(TurnRole::parse("system"), None);
    }

    #[test]
    fn session_status_serialises_as_lowercase_tag() {
        let s = serde_json::to_string(&SessionStatus::Completed).unwrap();
        assert_eq!(s, "\"completed\"");
    }
}
