//! Error types for ingest pipelines.

use basilisk_embeddings::EmbeddingError;
use basilisk_vector::VectorError;

/// Failure modes for an [`Ingester::ingest`] run.
///
/// Most ingesters handle per-record failures by logging + skipping
/// (surfaced via `IngestReport::errors`). `IngestError` is reserved
/// for run-level failures that halt the whole pipeline.
///
/// [`Ingester::ingest`]: crate::ingester::Ingester::ingest
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    /// Couldn't read the source (HTTP, filesystem, git).
    #[error("source read error: {0}")]
    Source(String),

    /// Source returned something we couldn't parse.
    #[error("parse error: {0}")]
    Parse(String),

    /// Filesystem I/O on `~/.basilisk/knowledge/` or similar.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// The embedding provider rejected a batch we couldn't recover
    /// from.
    #[error("embedding error: {0}")]
    Embedding(#[from] EmbeddingError),

    /// The vector store rejected an upsert we couldn't recover
    /// from.
    #[error("vector store error: {0}")]
    Vector(#[from] VectorError),

    /// Serialisation of state or records failed.
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),

    /// Catch-all for anything else.
    #[error("other: {0}")]
    Other(String),
}
