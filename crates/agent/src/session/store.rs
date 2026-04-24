// `stats` + `status` are unavoidable neighbours — `status` is a DB
// column the schema pins, `stats` is the JSON blob paired with it.
// Silence the similar-names lint at the module level rather than
// renaming one to something awkward.
#![allow(clippy::similar_names)]

//! `SessionStore` — `SQLite`-backed persistence for agent sessions.
//!
//! `CP4a` lays the plumbing: open an on-disk (or in-memory) database,
//! apply the schema idempotently, and hand back a handle callers can
//! share across threads. `CP4b` adds the write path (`create_session`,
//! `record_turn`, `record_tool_call`, `record_final_report`,
//! `mark_stopped`); `CP4c` adds the read / maintenance path
//! (`load_session`, `list_sessions`, `delete_session`,
//! `mark_running_as_interrupted`).
//!
//! Concurrency: the agent runs one session per process, but tool
//! calls execute on a tokio runtime that may schedule dispatches
//! across worker threads. `Connection` isn't `Sync`, so we wrap it in
//! a `Mutex` and share via `Arc<SessionStore>`. That's the right
//! trade-off here: writes are infrequent, the critical sections are
//! short, and `SQLite` serialises writes at the file level anyway.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::SystemTime,
};

use rusqlite::{params, Connection};

use crate::{
    session::{
        error::SessionError,
        time_serde::to_millis,
        types::{SessionStatus, TurnRole},
    },
    tool::SessionId,
};

/// Bundled schema DDL — applied on every open, `IF NOT EXISTS` guarded
/// so it's a no-op on subsequent opens.
const SCHEMA_SQL: &str = include_str!("schema.sql");

/// Current schema version. Bump when the schema changes; `apply_schema`
/// sets it on every open so a fresh DB starts at the current version
/// and future migrations land alongside the bump.
pub const SCHEMA_VERSION: u32 = 2;

/// Shared handle to the session database.
///
/// Cloneable — internally ref-counted. Callers typically hand a
/// `Arc<SessionStore>` into the `ToolContext` so the loop and every
/// tool call see the same database.
#[derive(Clone)]
pub struct SessionStore {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    conn: Mutex<Connection>,
}

impl SessionStore {
    /// Open (or create) a session database at `path`. Applies the
    /// schema idempotently. The parent directory must already exist.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let path = path.into();
        let conn = Connection::open(&path)?;
        Self::apply_schema(&conn)?;
        Ok(Self {
            inner: Arc::new(Inner {
                path,
                conn: Mutex::new(conn),
            }),
        })
    }

    /// Open an in-memory database — every call returns a fresh,
    /// independent store. For tests.
    pub fn open_in_memory() -> Result<Self, SessionError> {
        let conn = Connection::open_in_memory()?;
        Self::apply_schema(&conn)?;
        Ok(Self {
            inner: Arc::new(Inner {
                path: PathBuf::from(":memory:"),
                conn: Mutex::new(conn),
            }),
        })
    }

    /// Path the store was opened at. Returns `:memory:` for in-memory.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Read the `PRAGMA user_version` from the database. Used by tests
    /// and (later) migration code to decide whether to migrate.
    pub fn schema_version(&self) -> Result<u32, SessionError> {
        let conn = self.lock()?;
        let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))?;
        Ok(version)
    }

    /// Acquire the exclusive connection lock. `pub(crate)` so `CP4b`/c
    /// methods on this struct can share the helper without exposing
    /// `rusqlite` in the public API.
    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, Connection>, SessionError> {
        self.inner
            .conn
            .lock()
            .map_err(|_| SessionError::LockPoisoned)
    }

    fn apply_schema(conn: &Connection) -> Result<(), SessionError> {
        // Foreign keys aren't on by default in `SQLite`. We want CASCADE
        // on `DELETE session` to clean up turns + tool_calls.
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA_SQL)?;

        // Migrations. The CREATE TABLE above is the v2 schema; any
        // pre-existing v1 row set needs the `final_report_notes` column
        // added before we record a report against it.
        let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 2 {
            // `ADD COLUMN` errors if the column exists. A fresh DB just
            // ran the v2 CREATE TABLE above, so the column is already
            // there — swallow the error in that case, surface it if it's
            // something else.
            match conn.execute_batch("ALTER TABLE sessions ADD COLUMN final_report_notes TEXT;") {
                Ok(()) => {}
                Err(e) if e.to_string().contains("duplicate column") => {}
                Err(e) => return Err(e.into()),
            }
        }
        // Always set the canonical version after migrations.
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    // --- write path (set-6 `CP4b`) ---------------------------------------

    /// Insert a new `running` session row. Returns the generated
    /// [`SessionId`]. `stats_json` starts as `"{}"` — the loop fills it
    /// in via [`Self::mark_stopped`] when the session terminates.
    pub fn create_session(
        &self,
        target: impl Into<String>,
        model: impl Into<String>,
        system_prompt_hash: impl Into<String>,
        note: Option<String>,
    ) -> Result<SessionId, SessionError> {
        let id = SessionId::generate();
        let now_ms = to_millis(SystemTime::now());
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO sessions (
                id, created_at_ms, updated_at_ms, target, model, system_prompt_hash,
                status, note, stats_json
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, '{}')",
            params![
                id.as_str(),
                now_ms,
                now_ms,
                target.into(),
                model.into(),
                system_prompt_hash.into(),
                SessionStatus::Running.as_str(),
                note,
            ],
        )?;
        Ok(id)
    }

    /// Append one turn and return its 0-based index. Index assignment
    /// and the INSERT run inside the same `Mutex`-guarded critical
    /// section, so concurrent records are safely serialised — first
    /// caller to enter gets `0`, next gets `1`, and so on.
    ///
    /// Also bumps `sessions.updated_at_ms`.
    #[allow(clippy::too_many_arguments)] // columns are orthogonal, bundling hurts readability
    pub fn record_turn(
        &self,
        session_id: &SessionId,
        role: TurnRole,
        content: &serde_json::Value,
        tokens_in: Option<u32>,
        tokens_out: Option<u32>,
        started_at: SystemTime,
        ended_at: SystemTime,
    ) -> Result<u32, SessionError> {
        let conn = self.lock()?;
        let tx = conn.unchecked_transaction()?;
        let next_index: u32 = tx.query_row(
            "SELECT COALESCE(MAX(turn_index) + 1, 0) FROM turns WHERE session_id = ?",
            params![session_id.as_str()],
            |r| r.get(0),
        )?;
        let content_json = serde_json::to_string(content)?;
        tx.execute(
            "INSERT INTO turns (
                session_id, turn_index, role, content_json, tokens_in, tokens_out,
                started_at_ms, ended_at_ms
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                session_id.as_str(),
                next_index,
                role.as_str(),
                content_json,
                tokens_in,
                tokens_out,
                to_millis(started_at),
                to_millis(ended_at),
            ],
        )?;
        tx.execute(
            "UPDATE sessions SET updated_at_ms = ? WHERE id = ?",
            params![to_millis(SystemTime::now()), session_id.as_str()],
        )?;
        tx.commit()?;
        Ok(next_index)
    }

    /// Record one tool call within a turn. `call_index` is supplied by
    /// the caller — the loop already orders calls within a turn, and
    /// making it explicit lets the CP5 streaming code print "call 2 of
    /// 4" without a round-trip.
    #[allow(clippy::too_many_arguments)]
    pub fn record_tool_call(
        &self,
        session_id: &SessionId,
        turn_index: u32,
        call_index: u32,
        tool_use_id: impl Into<String>,
        tool_name: impl Into<String>,
        input: &serde_json::Value,
        output: Option<&serde_json::Value>,
        is_error: bool,
        duration_ms: u64,
    ) -> Result<(), SessionError> {
        let input_json = serde_json::to_string(input)?;
        let output_json = output.map(serde_json::to_string).transpose()?;
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO tool_calls (
                session_id, turn_index, call_index, tool_use_id, tool_name,
                input_json, output_json, is_error, duration_ms
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                session_id.as_str(),
                turn_index,
                call_index,
                tool_use_id.into(),
                tool_name.into(),
                input_json,
                output_json,
                i64::from(is_error),
                // duration in ms can exceed u32 for long stalls (30 min+); store as i64
                i64::try_from(duration_ms).unwrap_or(i64::MAX),
            ],
        )?;
        Ok(())
    }

    /// Attach the agent's `finalize_report` payload to the session.
    /// Does not change `status` — that's [`Self::mark_stopped`]'s job.
    pub fn record_final_report(
        &self,
        session_id: &SessionId,
        markdown: impl Into<String>,
        confidence: impl Into<String>,
        notes: Option<String>,
    ) -> Result<(), SessionError> {
        let conn = self.lock()?;
        let rows = conn.execute(
            "UPDATE sessions
             SET final_report_markdown = ?,
                 final_confidence = ?,
                 final_report_notes = ?,
                 updated_at_ms = ?
             WHERE id = ?",
            params![
                markdown.into(),
                confidence.into(),
                notes,
                to_millis(SystemTime::now()),
                session_id.as_str(),
            ],
        )?;
        if rows == 0 {
            return Err(SessionError::NotFound(session_id.as_str().to_string()));
        }
        Ok(())
    }

    /// Transition the session out of `running`. `stop_reason` is a
    /// free-form tag the loop picks (`"report_finalized"`,
    /// `"turn_limit"`, `"cost_exhausted"`, …). `stats` serialises
    /// whatever `CP5`'s `AgentStats` struct looks like at call time.
    pub fn mark_stopped(
        &self,
        session_id: &SessionId,
        stop_reason: impl Into<String>,
        status: SessionStatus,
        stats: &serde_json::Value,
    ) -> Result<(), SessionError> {
        if matches!(status, SessionStatus::Running) {
            return Err(SessionError::InvalidEnum {
                column: "status",
                value: "running (use record_turn for live updates)".into(),
            });
        }
        let stats_json = serde_json::to_string(stats)?;
        let conn = self.lock()?;
        let rows = conn.execute(
            "UPDATE sessions
             SET status = ?, stop_reason = ?, stats_json = ?, updated_at_ms = ?
             WHERE id = ?",
            params![
                status.as_str(),
                stop_reason.into(),
                stats_json,
                to_millis(SystemTime::now()),
                session_id.as_str(),
            ],
        )?;
        if rows == 0 {
            return Err(SessionError::NotFound(session_id.as_str().to_string()));
        }
        Ok(())
    }

    /// Flip a session back to `running` ahead of a resume. The caller
    /// (`AgentRunner::resume_with_observer`) uses this to undo the
    /// `Interrupted` / `Failed` status that `mark_stopped` left on the
    /// row, so subsequent `record_turn` calls pass the status guard.
    ///
    /// No-ops if the row is already `running`. Returns
    /// [`SessionError::NotFound`] if the session doesn't exist.
    pub fn mark_resumed(&self, session_id: &SessionId) -> Result<(), SessionError> {
        let conn = self.lock()?;
        let rows = conn.execute(
            "UPDATE sessions
             SET status = ?, stop_reason = NULL, updated_at_ms = ?
             WHERE id = ?",
            params![
                SessionStatus::Running.as_str(),
                to_millis(SystemTime::now()),
                session_id.as_str(),
            ],
        )?;
        if rows == 0 {
            return Err(SessionError::NotFound(session_id.as_str().to_string()));
        }
        Ok(())
    }

    // --- read + maintenance path (set-6 `CP4c`) --------------------------

    /// Reconstruct a full session transcript: the [`SessionRecord`]
    /// itself, every turn in write order, and every tool call keyed by
    /// its `(turn_index, call_index)` position.
    ///
    /// Returns [`SessionError::NotFound`] when no row matches.
    pub fn load_session(&self, session_id: &SessionId) -> Result<LoadedSession, SessionError> {
        let conn = self.lock()?;

        let session = conn
            .query_row(
                "SELECT id, created_at_ms, updated_at_ms, target, model,
                        system_prompt_hash, status, stop_reason,
                        final_report_markdown, final_confidence,
                        final_report_notes, note, stats_json
                 FROM sessions WHERE id = ?",
                params![session_id.as_str()],
                row_to_session,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    SessionError::NotFound(session_id.as_str().to_string())
                }
                other => SessionError::from(other),
            })??;

        let turns: Vec<TurnRecord> = {
            let mut stmt = conn.prepare(
                "SELECT session_id, turn_index, role, content_json, tokens_in, tokens_out,
                        started_at_ms, ended_at_ms
                 FROM turns WHERE session_id = ? ORDER BY turn_index",
            )?;
            let iter = stmt.query_map(params![session_id.as_str()], row_to_turn)?;
            let mut out = Vec::new();
            for row in iter {
                out.push(row??);
            }
            out
        };

        let tool_calls: Vec<ToolCallRecord> = {
            let mut stmt = conn.prepare(
                "SELECT session_id, turn_index, call_index, tool_use_id, tool_name,
                        input_json, output_json, is_error, duration_ms
                 FROM tool_calls WHERE session_id = ?
                 ORDER BY turn_index, call_index",
            )?;
            let iter = stmt.query_map(params![session_id.as_str()], row_to_tool_call)?;
            let mut out = Vec::new();
            for row in iter {
                out.push(row??);
            }
            out
        };

        Ok(LoadedSession {
            session,
            turns,
            tool_calls,
        })
    }

    /// Most-recent-first session listing. `limit` defaults to 50 when
    /// omitted; `status` filters by lifecycle stage when supplied.
    /// Returns the summary projection — no stats blob, no final report
    /// markdown — because the CLI's `audit session list` doesn't need
    /// those.
    pub fn list_sessions(
        &self,
        limit: Option<u32>,
        status: Option<SessionStatus>,
    ) -> Result<Vec<SessionSummary>, SessionError> {
        let limit = i64::from(limit.unwrap_or(50));
        let conn = self.lock()?;

        let (sql, status_tag): (&str, Option<&'static str>) = match status {
            Some(s) => (
                "SELECT id, created_at_ms, target, model, status, final_confidence
                 FROM sessions WHERE status = ?
                 ORDER BY created_at_ms DESC LIMIT ?",
                Some(s.as_str()),
            ),
            None => (
                "SELECT id, created_at_ms, target, model, status, final_confidence
                 FROM sessions ORDER BY created_at_ms DESC LIMIT ?",
                None,
            ),
        };

        let mut stmt = conn.prepare(sql)?;
        let iter = if let Some(tag) = status_tag {
            stmt.query_map(params![tag, limit], row_to_summary)?
                .collect::<Vec<_>>()
        } else {
            stmt.query_map(params![limit], row_to_summary)?
                .collect::<Vec<_>>()
        };

        let mut out = Vec::with_capacity(iter.len());
        for row in iter {
            out.push(row??);
        }
        Ok(out)
    }

    /// Remove a session and every row (turns, tool calls) that
    /// references it. Foreign-key CASCADE makes this a single DELETE.
    /// Returns [`SessionError::NotFound`] when the row didn't exist.
    pub fn delete_session(&self, session_id: &SessionId) -> Result<(), SessionError> {
        let conn = self.lock()?;
        let rows = conn.execute(
            "DELETE FROM sessions WHERE id = ?",
            params![session_id.as_str()],
        )?;
        if rows == 0 {
            return Err(SessionError::NotFound(session_id.as_str().to_string()));
        }
        Ok(())
    }

    /// Sweep every session still tagged `running` into `interrupted`.
    /// Called once at startup so a previous crash doesn't leave stale
    /// rows claiming to be live. Returns the number of rows touched.
    pub fn mark_running_as_interrupted(
        &self,
        reason: impl Into<String>,
    ) -> Result<usize, SessionError> {
        let conn = self.lock()?;
        let rows = conn.execute(
            "UPDATE sessions
             SET status = ?, stop_reason = ?, updated_at_ms = ?
             WHERE status = ?",
            params![
                SessionStatus::Interrupted.as_str(),
                reason.into(),
                to_millis(SystemTime::now()),
                SessionStatus::Running.as_str(),
            ],
        )?;
        Ok(rows)
    }
}

// --- row decoders ------------------------------------------------------
//
// Shared by the read-path methods. Each one takes a `rusqlite::Row` —
// column order follows the SELECT statement in the caller — and
// returns `Result<Record, SessionError>`. The outer
// `Result<Result<…, SessionError>, rusqlite::Error>` pattern from
// `query_row` / `query_map` forces a `.??` unwrap at the call site.

fn row_to_session(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<SessionRecord, SessionError>> {
    let status_s: String = row.get(6)?;
    let Some(status) = SessionStatus::parse(&status_s) else {
        return Ok(Err(SessionError::InvalidEnum {
            column: "status",
            value: status_s,
        }));
    };
    let stats_s: String = row.get(12)?;
    let stats = match serde_json::from_str::<serde_json::Value>(&stats_s) {
        Ok(v) => v,
        Err(e) => return Ok(Err(SessionError::Json(e))),
    };
    Ok(Ok(SessionRecord {
        id: row.get(0)?,
        created_at: crate::session::time_serde::from_millis(row.get(1)?),
        updated_at: crate::session::time_serde::from_millis(row.get(2)?),
        target: row.get(3)?,
        model: row.get(4)?,
        system_prompt_hash: row.get(5)?,
        status,
        stop_reason: row.get(7)?,
        final_report_markdown: row.get(8)?,
        final_confidence: row.get(9)?,
        // final_report_notes (index 10) lives on `SessionRecord` as
        // part of the `note` field's semantic pair — kept separate in
        // the DB but collapsed here for the CP4 surface. CP5/CP7 can
        // split it out if a reason emerges.
        note: match row.get::<_, Option<String>>(11)? {
            Some(n) => Some(match row.get::<_, Option<String>>(10)? {
                Some(report_notes) => format!("{n}\n---\n{report_notes}"),
                None => n,
            }),
            None => row.get::<_, Option<String>>(10)?,
        },
        stats,
    }))
}

fn row_to_turn(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<TurnRecord, SessionError>> {
    let role_s: String = row.get(2)?;
    let Some(role) = TurnRole::parse(&role_s) else {
        return Ok(Err(SessionError::InvalidEnum {
            column: "role",
            value: role_s,
        }));
    };
    let content_s: String = row.get(3)?;
    let content = match serde_json::from_str::<serde_json::Value>(&content_s) {
        Ok(v) => v,
        Err(e) => return Ok(Err(SessionError::Json(e))),
    };
    Ok(Ok(TurnRecord {
        session_id: row.get(0)?,
        turn_index: row.get(1)?,
        role,
        content,
        tokens_in: row.get(4)?,
        tokens_out: row.get(5)?,
        started_at: crate::session::time_serde::from_millis(row.get(6)?),
        ended_at: crate::session::time_serde::from_millis(row.get(7)?),
    }))
}

fn row_to_tool_call(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<ToolCallRecord, SessionError>> {
    let input_s: String = row.get(5)?;
    let input = match serde_json::from_str::<serde_json::Value>(&input_s) {
        Ok(v) => v,
        Err(e) => return Ok(Err(SessionError::Json(e))),
    };
    let output: Option<String> = row.get(6)?;
    let output = match output {
        Some(s) => match serde_json::from_str::<serde_json::Value>(&s) {
            Ok(v) => Some(v),
            Err(e) => return Ok(Err(SessionError::Json(e))),
        },
        None => None,
    };
    let is_error: i64 = row.get(7)?;
    let duration_ms: i64 = row.get(8)?;
    Ok(Ok(ToolCallRecord {
        session_id: row.get(0)?,
        turn_index: row.get(1)?,
        call_index: row.get(2)?,
        tool_use_id: row.get(3)?,
        tool_name: row.get(4)?,
        input,
        output,
        is_error: is_error != 0,
        duration_ms: u64::try_from(duration_ms).unwrap_or(0),
    }))
}

fn row_to_summary(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<SessionSummary, SessionError>> {
    let status_s: String = row.get(4)?;
    let Some(status) = SessionStatus::parse(&status_s) else {
        return Ok(Err(SessionError::InvalidEnum {
            column: "status",
            value: status_s,
        }));
    };
    Ok(Ok(SessionSummary {
        id: row.get(0)?,
        created_at: crate::session::time_serde::from_millis(row.get(1)?),
        target: row.get(2)?,
        model: row.get(3)?,
        status,
        final_confidence: row.get(5)?,
    }))
}

// Bring types into scope for the decoders.
use crate::session::types::{
    LoadedSession, SessionRecord, SessionSummary, ToolCallRecord, TurnRecord,
};

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStore")
            .field("path", &self.inner.path)
            .finish()
    }
}

/// Default path for the session database.
///
/// Resolves to `~/.basilisk/sessions.db` (the spec's fixed location).
/// Falls back to `./.basilisk/sessions.db` only when the home directory
/// cannot be determined. Does not create anything — callers are
/// expected to `create_dir_all` the parent before calling
/// [`SessionStore::open`].
#[must_use]
pub fn default_db_path() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        return home.join(".basilisk").join("sessions.db");
    }
    PathBuf::from(".basilisk").join("sessions.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory_applies_schema() {
        let store = SessionStore::open_in_memory().unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);

        // All three tables exist and are queryable (empty).
        let conn = store.lock().unwrap();
        for table in ["sessions", "turns", "tool_calls"] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0, "table {table} wasn't empty");
        }
    }

    #[test]
    fn open_on_disk_survives_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("sessions.db");

        let store = SessionStore::open(&db_path).unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        drop(store);

        // Re-open: schema is `IF NOT EXISTS`, so a second open is a
        // no-op and the version stays put.
        let store = SessionStore::open(&db_path).unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn schema_creates_expected_indexes() {
        let store = SessionStore::open_in_memory().unwrap();
        let conn = store.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name")
            .unwrap();
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(rows.iter().any(|n| n == "sessions_created_at_idx"));
        assert!(rows.iter().any(|n| n == "sessions_status_idx"));
    }

    #[test]
    fn foreign_keys_are_enabled() {
        let store = SessionStore::open_in_memory().unwrap();
        let conn = store.lock().unwrap();
        let on: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(on, 1);
    }

    #[test]
    fn store_is_clone_and_shares_connection() {
        let store = SessionStore::open_in_memory().unwrap();
        let store2 = store.clone();
        // Write through the second handle, read through the first —
        // same underlying connection.
        {
            let conn = store2.lock().unwrap();
            conn.execute("PRAGMA user_version = 42", []).unwrap();
        }
        assert_eq!(store.schema_version().unwrap(), 42);
    }

    #[test]
    fn path_reports_in_memory_marker() {
        let store = SessionStore::open_in_memory().unwrap();
        assert_eq!(store.path().to_string_lossy(), ":memory:");
    }

    // --- write-path (`CP4b`) tests ---------------------------------------

    fn fresh() -> SessionStore {
        SessionStore::open_in_memory().unwrap()
    }

    fn insert_session(store: &SessionStore) -> SessionId {
        store
            .create_session(
                "github://foundry-rs/forge-template",
                "anthropic/claude-opus-4-7",
                "sha256:0000",
                Some("ola smoke".into()),
            )
            .unwrap()
    }

    #[test]
    fn create_session_populates_required_columns_and_sets_running() {
        let store = fresh();
        let id = store
            .create_session(
                "0xdeadbeef",
                "anthropic/claude-opus-4-7",
                "sha256:abc",
                None,
            )
            .unwrap();

        let conn = store.lock().unwrap();
        let (status, target, model, prompt_hash, note, stats): (
            String,
            String,
            String,
            String,
            Option<String>,
            String,
        ) = conn
            .query_row(
                "SELECT status, target, model, system_prompt_hash, note, stats_json
                 FROM sessions WHERE id = ?",
                params![id.as_str()],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(status, "running");
        assert_eq!(target, "0xdeadbeef");
        assert_eq!(model, "anthropic/claude-opus-4-7");
        assert_eq!(prompt_hash, "sha256:abc");
        assert!(note.is_none());
        assert_eq!(stats, "{}");
    }

    #[test]
    fn create_session_records_provided_note() {
        let store = fresh();
        let id = store
            .create_session("x", "m", "h", Some("with-note".into()))
            .unwrap();
        let conn = store.lock().unwrap();
        let note: Option<String> = conn
            .query_row(
                "SELECT note FROM sessions WHERE id = ?",
                params![id.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(note.as_deref(), Some("with-note"));
    }

    #[test]
    fn record_turn_assigns_sequential_indices_starting_at_zero() {
        let store = fresh();
        let id = insert_session(&store);
        let now = SystemTime::now();
        let idx0 = store
            .record_turn(
                &id,
                TurnRole::User,
                &serde_json::json!([{"type": "text", "text": "hi"}]),
                None,
                None,
                now,
                now,
            )
            .unwrap();
        let idx1 = store
            .record_turn(
                &id,
                TurnRole::Assistant,
                &serde_json::json!([{"type": "text", "text": "ok"}]),
                Some(12),
                Some(3),
                now,
                now,
            )
            .unwrap();
        let idx2 = store
            .record_turn(
                &id,
                TurnRole::User,
                &serde_json::json!([]),
                None,
                None,
                now,
                now,
            )
            .unwrap();
        assert_eq!((idx0, idx1, idx2), (0, 1, 2));

        let conn = store.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM turns WHERE session_id = ?",
                params![id.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn record_turn_bumps_session_updated_at() {
        let store = fresh();
        let id = insert_session(&store);

        let initial_updated: i64 = {
            let conn = store.lock().unwrap();
            conn.query_row(
                "SELECT updated_at_ms FROM sessions WHERE id = ?",
                params![id.as_str()],
                |r| r.get(0),
            )
            .unwrap()
        };

        // Sleep a hair so the second millis reading is distinct.
        std::thread::sleep(std::time::Duration::from_millis(3));
        let now = SystemTime::now();
        store
            .record_turn(
                &id,
                TurnRole::User,
                &serde_json::json!([]),
                None,
                None,
                now,
                now,
            )
            .unwrap();

        let after: i64 = {
            let conn = store.lock().unwrap();
            conn.query_row(
                "SELECT updated_at_ms FROM sessions WHERE id = ?",
                params![id.as_str()],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(
            after > initial_updated,
            "{after} should be > {initial_updated}"
        );
    }

    #[test]
    fn record_turn_rejects_unknown_session_via_foreign_key() {
        let store = fresh();
        let ghost = SessionId::new("does-not-exist");
        let now = SystemTime::now();
        let err = store
            .record_turn(
                &ghost,
                TurnRole::User,
                &serde_json::json!([]),
                None,
                None,
                now,
                now,
            )
            .unwrap_err();
        assert!(matches!(err, SessionError::Sqlite(_)), "got {err:?}",);
    }

    #[test]
    fn record_tool_call_round_trips_input_output_and_is_error_flag() {
        let store = fresh();
        let id = insert_session(&store);
        let now = SystemTime::now();
        store
            .record_turn(
                &id,
                TurnRole::Assistant,
                &serde_json::json!([]),
                None,
                None,
                now,
                now,
            )
            .unwrap();

        store
            .record_tool_call(
                &id,
                0,
                0,
                "tu_1",
                "classify_target",
                &serde_json::json!({ "input": "0xdead" }),
                Some(&serde_json::json!({ "OnChain": { "address": "0xdead" } })),
                false,
                142,
            )
            .unwrap();
        store
            .record_tool_call(
                &id,
                0,
                1,
                "tu_2",
                "grep_project",
                &serde_json::json!({ "pattern": "[bad" }),
                None,
                true,
                3,
            )
            .unwrap();

        let conn = store.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT tool_name, is_error, output_json, duration_ms
                 FROM tool_calls
                 WHERE session_id = ? AND turn_index = 0
                 ORDER BY call_index",
            )
            .unwrap();
        let rows: Vec<(String, i64, Option<String>, i64)> = stmt
            .query_map(params![id.as_str()], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "classify_target");
        assert_eq!(rows[0].1, 0); // is_error false
        assert!(rows[0].2.as_deref().unwrap().contains("OnChain"));
        assert_eq!(rows[0].3, 142);
        assert_eq!(rows[1].0, "grep_project");
        assert_eq!(rows[1].1, 1); // is_error true
        assert!(rows[1].2.is_none());
    }

    #[test]
    fn record_final_report_updates_columns_without_changing_status() {
        let store = fresh();
        let id = insert_session(&store);
        store
            .record_final_report(
                &id,
                "# Summary\n\nAll good.",
                "high",
                Some("watch slot 7".into()),
            )
            .unwrap();

        let conn = store.lock().unwrap();
        let (status, md, conf, notes): (String, Option<String>, Option<String>, Option<String>) =
            conn.query_row(
                "SELECT status, final_report_markdown, final_confidence, final_report_notes
                 FROM sessions WHERE id = ?",
                params![id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(status, "running"); // unchanged
        assert!(md.unwrap().contains("All good"));
        assert_eq!(conf.as_deref(), Some("high"));
        assert_eq!(notes.as_deref(), Some("watch slot 7"));
    }

    #[test]
    fn record_final_report_errors_on_unknown_session() {
        let store = fresh();
        let ghost = SessionId::new("nope");
        let err = store
            .record_final_report(&ghost, "x", "low", None)
            .unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }

    #[test]
    fn mark_stopped_rejects_running_status() {
        let store = fresh();
        let id = insert_session(&store);
        let err = store
            .mark_stopped(&id, "foo", SessionStatus::Running, &serde_json::json!({}))
            .unwrap_err();
        assert!(matches!(err, SessionError::InvalidEnum { .. }));
    }

    #[test]
    fn mark_stopped_completes_session_with_stats_blob() {
        let store = fresh();
        let id = insert_session(&store);
        let stats_payload = serde_json::json!({
            "turns": 5,
            "tool_calls": 9,
            "cost_cents": 87,
        });
        store
            .mark_stopped(
                &id,
                "report_finalized",
                SessionStatus::Completed,
                &stats_payload,
            )
            .unwrap();

        let conn = store.lock().unwrap();
        let (status_col, reason, stats_json): (String, String, String) = conn
            .query_row(
                "SELECT status, stop_reason, stats_json FROM sessions WHERE id = ?",
                params![id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(status_col, "completed");
        assert_eq!(reason, "report_finalized");
        let parsed: serde_json::Value = serde_json::from_str(&stats_json).unwrap();
        assert_eq!(parsed["cost_cents"], 87);
    }

    #[test]
    fn full_lifecycle_writes_coherent_rows() {
        // Exercises every CP4b method end-to-end and checks the final
        // row shape matches what CP4c (read path) will later reconstruct.
        let store = fresh();
        let id = store
            .create_session(
                "./my-project",
                "anthropic/claude-opus-4-7",
                "sha256:42",
                None,
            )
            .unwrap();

        let now = SystemTime::now();
        let t0 = store
            .record_turn(
                &id,
                TurnRole::User,
                &serde_json::json!([{"type": "text", "text": "recon this"}]),
                None,
                None,
                now,
                now,
            )
            .unwrap();
        let t1 = store
            .record_turn(
                &id,
                TurnRole::Assistant,
                &serde_json::json!([{"type": "tool_use", "id": "tu_x", "name": "classify_target", "input": {}}]),
                Some(200),
                Some(50),
                now,
                now,
            )
            .unwrap();
        store
            .record_tool_call(
                &id,
                t1,
                0,
                "tu_x",
                "classify_target",
                &serde_json::json!({ "input": "./my-project" }),
                Some(&serde_json::json!({ "LocalPath": { "root": "./my-project" } })),
                false,
                17,
            )
            .unwrap();
        store
            .record_final_report(&id, "# Brief\n\nReady.", "high", None)
            .unwrap();
        store
            .mark_stopped(
                &id,
                "report_finalized",
                SessionStatus::Completed,
                &serde_json::json!({ "turns": 2, "cost_cents": 4 }),
            )
            .unwrap();

        let conn = store.lock().unwrap();
        let session_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        let turn_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM turns WHERE session_id = ?",
                params![id.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        let call_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tool_calls WHERE session_id = ?",
                params![id.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(session_count, 1);
        assert_eq!(turn_count, 2);
        assert_eq!(call_count, 1);
        assert_eq!(t0, 0);
        assert_eq!(t1, 1);
    }

    // --- read + maintenance (`CP4c`) tests -------------------------------

    #[test]
    fn load_session_round_trips_full_lifecycle() {
        let store = fresh();
        let id = store
            .create_session("./proj", "m", "sha256:1", Some("user-note".into()))
            .unwrap();
        let now = SystemTime::now();
        store
            .record_turn(
                &id,
                TurnRole::User,
                &serde_json::json!([{"type": "text", "text": "go"}]),
                None,
                None,
                now,
                now,
            )
            .unwrap();
        store
            .record_turn(
                &id,
                TurnRole::Assistant,
                &serde_json::json!([{"type": "text", "text": "done"}]),
                Some(40),
                Some(20),
                now,
                now,
            )
            .unwrap();
        store
            .record_tool_call(
                &id,
                1,
                0,
                "tu_a",
                "classify_target",
                &serde_json::json!({ "input": "./proj" }),
                Some(&serde_json::json!({ "LocalPath": true })),
                false,
                42,
            )
            .unwrap();
        store
            .record_final_report(&id, "# Report", "high", Some("agent-notes".into()))
            .unwrap();
        store
            .mark_stopped(
                &id,
                "report_finalized",
                SessionStatus::Completed,
                &serde_json::json!({ "turns": 2 }),
            )
            .unwrap();

        let loaded = store.load_session(&id).unwrap();
        assert_eq!(loaded.session.id, id.as_str());
        assert_eq!(loaded.session.status, SessionStatus::Completed);
        assert_eq!(
            loaded.session.stop_reason.as_deref(),
            Some("report_finalized")
        );
        assert_eq!(loaded.session.target, "./proj");
        assert_eq!(loaded.session.final_confidence.as_deref(), Some("high"));
        assert!(loaded
            .session
            .final_report_markdown
            .as_deref()
            .unwrap()
            .contains("Report"));
        // Session note collapses with finalize notes in CP4c's flat read.
        let note = loaded.session.note.as_deref().unwrap();
        assert!(note.contains("user-note"));
        assert!(note.contains("agent-notes"));
        assert_eq!(loaded.turns.len(), 2);
        assert_eq!(loaded.turns[0].role, TurnRole::User);
        assert_eq!(loaded.turns[1].role, TurnRole::Assistant);
        assert_eq!(loaded.turns[1].tokens_in, Some(40));
        assert_eq!(loaded.tool_calls.len(), 1);
        assert_eq!(loaded.tool_calls[0].tool_name, "classify_target");
        assert!(!loaded.tool_calls[0].is_error);
        assert_eq!(loaded.tool_calls[0].duration_ms, 42);
    }

    #[test]
    fn load_session_reports_not_found_for_missing_id() {
        let store = fresh();
        let err = store.load_session(&SessionId::new("ghost")).unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }

    #[test]
    fn list_sessions_orders_newest_first() {
        let store = fresh();
        let older = store.create_session("a", "m", "h", None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        let newer = store.create_session("b", "m", "h", None).unwrap();

        let list = store.list_sessions(None, None).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, newer.as_str());
        assert_eq!(list[1].id, older.as_str());
    }

    #[test]
    fn list_sessions_respects_limit() {
        let store = fresh();
        for i in 0..5 {
            store
                .create_session(format!("t{i}"), "m", "h", None)
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let list = store.list_sessions(Some(2), None).unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn list_sessions_filters_by_status() {
        let store = fresh();
        let a = store.create_session("t1", "m", "h", None).unwrap();
        let b = store.create_session("t2", "m", "h", None).unwrap();
        store
            .mark_stopped(
                &a,
                "report_finalized",
                SessionStatus::Completed,
                &serde_json::json!({}),
            )
            .unwrap();

        let completed = store
            .list_sessions(None, Some(SessionStatus::Completed))
            .unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].id, a.as_str());

        let running = store
            .list_sessions(None, Some(SessionStatus::Running))
            .unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].id, b.as_str());
    }

    #[test]
    fn delete_session_cascades_to_turns_and_tool_calls() {
        let store = fresh();
        let id = insert_session(&store);
        let now = SystemTime::now();
        store
            .record_turn(
                &id,
                TurnRole::User,
                &serde_json::json!([]),
                None,
                None,
                now,
                now,
            )
            .unwrap();
        store
            .record_tool_call(
                &id,
                0,
                0,
                "tu",
                "classify_target",
                &serde_json::json!({}),
                None,
                false,
                1,
            )
            .unwrap();

        store.delete_session(&id).unwrap();

        let conn = store.lock().unwrap();
        let session_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE id = ?",
                params![id.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        let turn_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM turns WHERE session_id = ?",
                params![id.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        let call_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tool_calls WHERE session_id = ?",
                params![id.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(session_count, 0);
        assert_eq!(turn_count, 0);
        assert_eq!(call_count, 0);
    }

    #[test]
    fn delete_session_reports_not_found_when_missing() {
        let store = fresh();
        let err = store.delete_session(&SessionId::new("nope")).unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }

    #[test]
    fn mark_running_as_interrupted_sweeps_stale_rows() {
        let store = fresh();
        // Two running, one already completed.
        let a = store.create_session("t1", "m", "h", None).unwrap();
        let b = store.create_session("t2", "m", "h", None).unwrap();
        let done = store.create_session("t3", "m", "h", None).unwrap();
        store
            .mark_stopped(
                &done,
                "report_finalized",
                SessionStatus::Completed,
                &serde_json::json!({}),
            )
            .unwrap();

        let touched = store
            .mark_running_as_interrupted("process_crashed")
            .unwrap();
        assert_eq!(touched, 2);

        // a + b now interrupted; `done` untouched.
        let loaded_a = store.load_session(&a).unwrap();
        let loaded_b = store.load_session(&b).unwrap();
        let loaded_done = store.load_session(&done).unwrap();
        assert_eq!(loaded_a.session.status, SessionStatus::Interrupted);
        assert_eq!(
            loaded_a.session.stop_reason.as_deref(),
            Some("process_crashed")
        );
        assert_eq!(loaded_b.session.status, SessionStatus::Interrupted);
        assert_eq!(loaded_done.session.status, SessionStatus::Completed);
    }

    #[test]
    fn mark_running_as_interrupted_is_a_noop_when_nothing_running() {
        let store = fresh();
        let touched = store
            .mark_running_as_interrupted("process_crashed")
            .unwrap();
        assert_eq!(touched, 0);
    }

    #[test]
    fn load_session_returns_turns_in_index_order_even_if_inserted_out_of_order() {
        let store = fresh();
        let id = insert_session(&store);
        let now = SystemTime::now();
        // Sequential API guarantees 0, 1, 2 — exercise that read order
        // matches regardless.
        for role in [TurnRole::User, TurnRole::Assistant, TurnRole::User] {
            store
                .record_turn(&id, role, &serde_json::json!([]), None, None, now, now)
                .unwrap();
        }
        let loaded = store.load_session(&id).unwrap();
        let indices: Vec<u32> = loaded.turns.iter().map(|t| t.turn_index).collect();
        assert_eq!(indices, vec![0, 1, 2]);
    }
}
