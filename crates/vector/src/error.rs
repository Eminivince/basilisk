//! Error types for [`VectorStore`] operations.

use std::path::PathBuf;

/// Failure modes for vector-store operations.
///
/// [`VectorStore`]: crate::store::VectorStore
#[derive(Debug, thiserror::Error)]
pub enum VectorError {
    /// A collection with the given name doesn't exist.
    #[error("collection not found: {0}")]
    CollectionNotFound(String),

    /// A collection with this name already exists with a
    /// different spec. The caller should `delete + recreate` or
    /// `reembed` — we won't silently alter.
    #[error(
        "collection '{name}' exists with incompatible spec: \
         stored provider={stored_provider} dim={stored_dim}, \
         requested provider={requested_provider} dim={requested_dim}"
    )]
    IncompatibleSpec {
        name: String,
        stored_provider: String,
        stored_dim: usize,
        requested_provider: String,
        requested_dim: usize,
    },

    /// An operation on `collection` expected vectors of the
    /// collection's dimension but received something different.
    /// Usually signals the caller's embedding provider changed
    /// without a `reembed`.
    #[error(
        "vector dimension mismatch in '{collection}': expected {expected}, got {actual}"
    )]
    DimensionMismatch {
        collection: String,
        expected: usize,
        actual: usize,
    },

    /// Schema version in storage is newer than this build knows.
    /// Caller should upgrade the binary before opening.
    #[error("collection '{name}' schema version {stored} is newer than supported {max_known}")]
    UnknownSchemaVersion {
        name: String,
        stored: u32,
        max_known: u32,
    },

    /// Low-level storage I/O failed.
    #[error("storage error at {path}: {message}")]
    Storage {
        path: PathBuf,
        message: String,
    },

    /// Backend-specific error we couldn't classify more precisely.
    #[error("backend error: {0}")]
    Backend(String),

    /// Caller supplied input that violated invariants we can't
    /// recover from.
    #[error("bad input: {0}")]
    BadInput(String),

    /// Serialisation / deserialisation of a record's metadata
    /// failed.
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_formats_incompatible_spec_with_all_context() {
        let err = VectorError::IncompatibleSpec {
            name: "public_findings".into(),
            stored_provider: "voyage/voyage-code-3".into(),
            stored_dim: 1024,
            requested_provider: "openai/text-embedding-3-large".into(),
            requested_dim: 3072,
        };
        let s = err.to_string();
        assert!(s.contains("voyage"));
        assert!(s.contains("openai"));
        assert!(s.contains("1024"));
        assert!(s.contains("3072"));
    }

    #[test]
    fn display_formats_dimension_mismatch_with_numbers() {
        let err = VectorError::DimensionMismatch {
            collection: "x".into(),
            expected: 1024,
            actual: 768,
        };
        let s = err.to_string();
        assert!(s.contains("expected 1024"));
        assert!(s.contains("got 768"));
    }
}
