//! The [`EmbeddingProvider`] trait.
//!
//! Deliberately minimal: one method, batch-in batch-out. Every
//! backend speaks to the same shape; the batching wrapper in
//! `CP7.2`'s [`crate::batching`] layer sits on top to handle
//! token-aware rate limiting and window accumulation.

use async_trait::async_trait;

use crate::{error::EmbeddingError, types::Embedding, types::EmbeddingInput};

/// A model-agnostic embedding backend.
///
/// Implementations wrap one provider's HTTP surface. Callers
/// (`basilisk-vector`, `basilisk-ingest`, `basilisk-knowledge`)
/// hold a `&dyn EmbeddingProvider` or `Arc<dyn EmbeddingProvider>`
/// and don't care which backend they're talking to.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Stable, human-readable identifier for this backend + model —
    /// e.g. `"voyage/voyage-code-3"`. Stamped on session records so
    /// mixed-provider corpora remain attributable and dimension
    /// mismatches surface diagnostic-ally.
    fn identifier(&self) -> &str;

    /// Vector dimensionality emitted by this provider + model.
    /// Collections are created with this dimension pinned in their
    /// metadata; switching providers requires `audit knowledge
    /// reembed` to drop + recreate.
    fn dimensions(&self) -> usize;

    /// Maximum token length per single input the provider accepts.
    /// Callers that chunk respect this as a hard upper bound.
    fn max_tokens_per_input(&self) -> usize;

    /// Whether the provider supports batched requests. Every
    /// shipped provider supports batching; the method lives for
    /// forward-compat.
    fn supports_batch(&self) -> bool {
        true
    }

    /// Maximum number of inputs the provider accepts in one batched
    /// call. Exceeding this returns [`EmbeddingError::BadInput`].
    fn max_batch_size(&self) -> usize;

    /// Maximum total tokens (summed across all inputs) the provider
    /// accepts in one batched call. Voyage caps at 120k; `OpenAI` at
    /// ~300k; Ollama is unbounded by spec. The default returns the
    /// product of `max_tokens_per_input * max_batch_size` — i.e. no
    /// extra constraint beyond the per-input + count limits.
    /// Backends with explicit per-batch caps override this; ingesters
    /// pack batches up to whichever limit binds first.
    fn max_tokens_per_batch(&self) -> usize {
        self.max_tokens_per_input().saturating_mul(self.max_batch_size())
    }

    /// Embed a batch of inputs.
    ///
    /// Returns one [`Embedding`] per input in the same order. An
    /// empty `inputs` slice is allowed and returns an empty vec
    /// without hitting the network — useful for loops that may end
    /// up with nothing to embed.
    async fn embed(&self, inputs: &[EmbeddingInput]) -> Result<Vec<Embedding>, EmbeddingError>;
}
