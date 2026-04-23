//! Error type for git operations.

use thiserror::Error;

/// Errors surfaced by [`crate::RepoCache::fetch`] and friends.
#[derive(Debug, Error)]
pub enum GitError {
    /// Upstream repo doesn't exist or isn't accessible with current auth.
    #[error("repo not found: {owner}/{repo}")]
    RepoNotFound { owner: String, repo: String },

    /// Private repo, no token or token lacks scope.
    #[error("authentication required (set GITHUB_TOKEN)")]
    AuthenticationRequired,

    /// Ref doesn't exist on the remote.
    #[error("ref not found: {reference}")]
    RefNotFound { reference: String },

    /// Clone failed — wraps network, disk, or libgit2 errors.
    #[error("clone failed: {detail}")]
    CloneFailed { detail: String },

    /// Cache directory contains a `.basilisk-meta.json` we can't parse.
    #[error("cache corrupt at {path}: {detail}")]
    CacheCorrupt { path: String, detail: String },

    /// Filesystem I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Anything else.
    #[error("{0}")]
    Other(String),
}
