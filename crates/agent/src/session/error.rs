//! Error surface for the session store.

use thiserror::Error;

/// Anything the [`crate::session::SessionStore`] can fail with.
#[derive(Debug, Error)]
pub enum SessionError {
    /// `SQLite` rejected a statement, the file couldn't be opened, or a
    /// migration failed.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A row's JSON column couldn't be (de)serialised. Usually means a
    /// `CP4`-era session was written with a schema field we've since
    /// tightened; bump the schema version and migrate if so.
    #[error("malformed JSON in database: {0}")]
    Json(#[from] serde_json::Error),

    /// Caller asked for a session id that isn't in the database.
    #[error("session {0:?} not found")]
    NotFound(String),

    /// A row had an unknown `status` / `role` string. Same cause as
    /// [`Self::Json`] — a schema drift.
    #[error("invalid enum value for {column:?}: {value:?}")]
    InvalidEnum { column: &'static str, value: String },

    /// The mutex guarding the connection was poisoned — some other
    /// thread panicked mid-write. Recoverable on next open.
    #[error("session store lock poisoned")]
    LockPoisoned,

    /// A required schema migration couldn't be applied — surfaces
    /// the underlying scratchpad / scratchpad-revisions table
    /// creation failure.
    #[error("schema migration failed: {0}")]
    SchemaMigration(String),
}
