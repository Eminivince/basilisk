//! Error surface for the LLM backend.

use std::time::Duration;

use thiserror::Error;

/// Every failure mode any `LlmBackend` implementation can produce.
///
/// Callers (the agent loop) match on these: `RateLimited` is retryable
/// with backoff, `AuthError` / `BadRequest` are not, `ServerError` may
/// be depending on status code.
#[derive(Debug, Error)]
pub enum LlmError {
    /// Backend returned 429. `retry_after` is populated from the
    /// `retry-after` header when present.
    #[error("rate limited (retry after {retry_after:?})")]
    RateLimited { retry_after: Option<Duration> },

    /// API key missing, invalid, or revoked. Fail loudly.
    #[error("authentication failed: {0}")]
    AuthError(String),

    /// Transport-layer error (DNS, TLS, socket). Retryable in principle
    /// but the caller decides.
    #[error("network error: {0}")]
    NetworkError(String),

    /// 4xx other than 401/429 — request shape was wrong. Not retryable.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// 5xx. Retryable with backoff; include the body for diagnostics.
    #[error("server error (status {status}): {body}")]
    ServerError { status: u16, body: String },

    /// Body came back but we couldn't deserialize it.
    #[error("failed to parse response: {0}")]
    ParseError(String),

    /// Request-level timeout (as distinct from socket-level network errors).
    #[error("request timed out")]
    Timeout,

    /// Anything else.
    #[error("{0}")]
    Other(String),
}

impl LlmError {
    /// `true` when retrying the same request after backoff has a
    /// reasonable chance of succeeding.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. }
                | Self::ServerError { .. }
                | Self::NetworkError(_)
                | Self::Timeout
        )
    }
}
