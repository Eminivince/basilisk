//! Errors returned by explorer clients.

use thiserror::Error;

/// Failure modes for a single explorer call.
#[derive(Debug, Error)]
pub enum ExplorerError {
    /// Transport or connection-level failure.
    #[error("network error: {0}")]
    Network(String),

    /// Upstream signaled rate limiting.
    #[error("rate limited")]
    RateLimited,

    /// The explorer doesn't support this chain.
    #[error("chain not supported by this explorer")]
    ChainUnsupported,

    /// Required API key missing from the config.
    #[error("API key missing")]
    NoApiKey,

    /// Explorer returned a structurally invalid response.
    #[error("malformed explorer response: {0}")]
    MalformedResponse(String),

    /// Upstream surfaced an error we can't classify further.
    #[error("explorer error: {0}")]
    Other(String),
}
