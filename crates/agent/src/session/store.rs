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
};

use rusqlite::Connection;

use crate::session::error::SessionError;

/// Bundled schema DDL — applied on every open, `IF NOT EXISTS` guarded
/// so it's a no-op on subsequent opens.
const SCHEMA_SQL: &str = include_str!("schema.sql");

/// Current schema version. Bump when the schema changes; a later
/// checkpoint adds migration-on-open.
pub const SCHEMA_VERSION: u32 = 1;

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
}
