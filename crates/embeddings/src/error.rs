//! Error types for embedding backends.
//!
//! Mirrors `basilisk-llm`'s `LlmError` (`crates/llm/src/error.rs`) —
//! same classification, same retryability contract. Downstream code
//! can treat either family uniformly when bridging.

use std::time::Duration;

/// Failure modes for an [`EmbeddingProvider::embed`] call.
///
/// [`EmbeddingProvider::embed`]: crate::backend::EmbeddingProvider::embed
#[derive(Debug, thiserror::Error)]
pub enum EmbeddingError {
    /// Provider rejected the request for quota reasons. `retry_after`
    /// carries the `Retry-After` header value if the provider sent
    /// one — retry loops should honour it.
    #[error("rate limited (retry_after={retry_after:?})")]
    RateLimited { retry_after: Option<Duration> },

    /// Authentication failed (401 / 403). Typically the API key is
    /// missing, wrong, or revoked.
    #[error("authentication failed: {0}")]
    AuthError(String),

    /// Low-level network failure: DNS, TCP, TLS, timeouts. Usually
    /// retryable.
    #[error("network error: {0}")]
    NetworkError(String),

    /// The request was malformed or the input violated the provider's
    /// constraints (too many items, too many tokens per item,
    /// unsupported model). Not retryable without changing the input.
    #[error("bad input: {0}")]
    BadInput(String),

    /// Non-success HTTP that doesn't fit the other categories. The
    /// body is retained for forensics.
    #[error("server error: HTTP {status}: {body}")]
    ServerError { status: u16, body: String },

    /// Response arrived but didn't parse into the expected shape.
    #[error("parse error: {0}")]
    ParseError(String),

    /// Overall request exceeded the client-side timeout.
    #[error("timeout")]
    Timeout,

    /// Catch-all for anything else. Try to classify more precisely
    /// before reaching for this.
    #[error("other: {0}")]
    Other(String),
}

impl EmbeddingError {
    /// Whether a caller should retry the same request after a backoff.
    ///
    /// Rate-limit, network, and timeout errors are retryable;
    /// auth-class errors aren't (retrying won't fix a wrong key); bad
    /// input isn't (retrying the same bad input is useless).
    /// Server 5xx is retryable up to a cap. The caller decides the
    /// cap and the backoff shape — we just answer the yes/no.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimited { .. } | Self::NetworkError(_) | Self::Timeout => true,
            Self::ServerError { status, .. } => *status >= 500,
            Self::AuthError(_) | Self::BadInput(_) | Self::ParseError(_) | Self::Other(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limited_and_network_are_retryable() {
        assert!(EmbeddingError::RateLimited { retry_after: None }.is_retryable());
        assert!(EmbeddingError::NetworkError("dns".into()).is_retryable());
        assert!(EmbeddingError::Timeout.is_retryable());
    }

    #[test]
    fn auth_and_bad_input_are_not_retryable() {
        assert!(!EmbeddingError::AuthError("401".into()).is_retryable());
        assert!(!EmbeddingError::BadInput("too long".into()).is_retryable());
    }

    #[test]
    fn server_5xx_retryable_but_4xx_is_not() {
        assert!(EmbeddingError::ServerError {
            status: 502,
            body: String::new()
        }
        .is_retryable());
        assert!(!EmbeddingError::ServerError {
            status: 418,
            body: String::new()
        }
        .is_retryable());
    }
}
