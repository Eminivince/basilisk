//! Workspace-wide error type.
//!
//! Library crates return [`Error`] via [`Result`]. The CLI binary converts
//! these into `anyhow::Error` at its boundary so user-facing errors carry
//! full context.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }
}
