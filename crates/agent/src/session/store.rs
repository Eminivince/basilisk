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
}

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStore")
            .field("path", &self.inner.path)
            .finish()
    }
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
}
