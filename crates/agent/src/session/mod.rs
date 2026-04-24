//! `SQLite`-backed agent-session persistence.
//!
//! `CP4` ships in three slices:
//!
//!  - **`CP4a`** (this commit): plumbing. [`SessionStore`] opens a DB,
//!    applies the schema, exposes a shared handle. Typed record enums
//!    ([`SessionStatus`], [`TurnRole`], `*Record`) are defined so
//!    later slices have something to write.
//!  - **`CP4b`**: write path — `create_session`, `record_turn`,
//!    `record_tool_call`, `record_final_report`, `mark_stopped`.
//!  - **`CP4c`**: read + maintenance path — `load_session`,
//!    `list_sessions`, `delete_session`,
//!    `mark_running_as_interrupted`.

pub mod error;
pub mod store;
pub(crate) mod time_serde;
pub mod types;

pub use error::SessionError;
pub use store::{SessionStore, SCHEMA_VERSION};
pub use types::{
    SessionRecord, SessionStatus, SessionSummary, ToolCallRecord, TurnRecord, TurnRole,
};
