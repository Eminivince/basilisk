//! Error surface for [`crate::Scratchpad`] operations + the
//! [`ScratchpadStore`](crate::ScratchpadStore) persistence layer.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ScratchpadError {
    /// The requested section doesn't exist on this scratchpad.
    /// Fires on updates/reads targeting a `Custom` section that
    /// the agent never created.
    #[error("missing section: {0}")]
    MissingSection(String),

    /// The operation targeted the wrong section kind (e.g.
    /// `append_item` on a `Prose` section, or `set_prose` on an
    /// `Items` section).
    #[error("section '{section}' is {actual}, not {expected}")]
    WrongSectionKind {
        section: String,
        expected: &'static str,
        actual: &'static str,
    },

    /// The requested item id doesn't exist in the target section.
    #[error("item {item_id} not found in section '{section}'")]
    ItemNotFound { section: String, item_id: u64 },

    /// Tried to create a `Custom` section whose name clashes with
    /// one of the built-in keys or fails validation.
    #[error("invalid custom section name '{name}': {reason}")]
    InvalidCustomName { name: String, reason: String },

    /// Persistence / DB layer error.
    #[error("storage: {0}")]
    Storage(String),

    /// JSON (de)serialization error at the persistence boundary.
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),

    /// Raw rusqlite failure.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
}
