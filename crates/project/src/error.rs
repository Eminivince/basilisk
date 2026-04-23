//! Error surface for the project crate.

use std::{io, path::PathBuf};

use thiserror::Error;

/// Errors returned by project detection and layout inspection.
#[derive(Debug, Error)]
pub enum ProjectError {
    /// Caller pointed at a path that doesn't exist on disk.
    #[error("path does not exist: {0}")]
    DoesNotExist(PathBuf),
    /// Caller pointed at a path that exists but isn't a directory.
    #[error("not a directory: {0}")]
    NotADirectory(PathBuf),
    /// Filesystem read failure while walking a project root.
    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// A config file existed but couldn't be parsed.
    #[error("failed to parse {path}: {detail}")]
    ParseFailed { path: PathBuf, detail: String },
}

impl ProjectError {
    /// Convenience for tagging a stray `io::Error` with the path it came from.
    pub fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
