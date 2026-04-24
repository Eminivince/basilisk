//! Error type for the public knowledge-base API.

use basilisk_embeddings::EmbeddingError;
use basilisk_vector::VectorError;

/// Failure modes for [`KnowledgeBase`] operations.
///
/// [`KnowledgeBase`]: crate::knowledge_base::KnowledgeBase
#[derive(Debug, thiserror::Error)]
pub enum KnowledgeError {
    /// Embedding provider rejected a request.
    #[error("embedding error: {0}")]
    Embedding(#[from] EmbeddingError),

    /// Vector store rejected an operation.
    #[error("vector store error: {0}")]
    Vector(#[from] VectorError),

    /// Caller asked to correct a finding that doesn't exist.
    #[error("finding not found: {0}")]
    FindingNotFound(String),

    /// Caller supplied invalid input (empty query, missing
    /// required fields).
    #[error("bad input: {0}")]
    BadInput(String),

    /// Serialisation of a record's metadata failed.
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
}
