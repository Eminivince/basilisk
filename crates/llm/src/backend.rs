//! The `LlmBackend` trait and supporting abstractions.
//!
//! Streaming comes in CP2. For now the trait exposes the synchronous
//! `complete` entry point — request goes in, response comes out. The
//! agent loop in set-6 CP5 will initially build on `complete`; streaming
//! is a drop-in replacement.

use async_trait::async_trait;

use crate::{
    error::LlmError,
    types::{CompletionRequest, CompletionResponse},
};

/// A model-agnostic LLM provider.
///
/// Implementations wrap one provider's HTTP surface. Callers (the agent
/// loop, tests) hold a `&dyn LlmBackend` or `Arc<dyn LlmBackend>` and
/// don't care which backend they're talking to.
#[async_trait]
pub trait LlmBackend: Send + Sync {
    /// A stable, human-readable identifier for this backend + model —
    /// e.g. `"anthropic/claude-opus-4-7"`. The agent records this on
    /// the session so resumes can detect model drift.
    fn identifier(&self) -> &str;

    /// Send a completion request and wait for the full response.
    ///
    /// Implementations are expected to be cancel-safe: if the future is
    /// dropped mid-flight, no partial state leaks.
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError>;
}
