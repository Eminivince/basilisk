//! RPC error type shared across providers.

use thiserror::Error;

/// Errors returned by [`crate::RpcProvider`] implementations.
#[derive(Debug, Error)]
pub enum RpcError {
    /// No RPC URL could be resolved for the requested chain.
    #[error("no RPC provider configured for chain {chain:?}: {suggestion}")]
    NoProviderConfigured { chain: String, suggestion: String },

    /// The configured RPC URL failed to parse.
    #[error("invalid RPC URL {url:?}: {detail}")]
    InvalidUrl { url: String, detail: String },

    /// Transient network / server error — callers may retry.
    #[error("transient RPC error: {0}")]
    Transient(String),

    /// Rate limited by the upstream provider.
    #[error("rate limited by RPC provider")]
    RateLimited,

    /// Request timed out.
    #[error("RPC request timed out after {secs}s")]
    Timeout { secs: u64 },

    /// The server returned an unrecoverable error.
    #[error("RPC server error: {0}")]
    Server(String),

    /// The cache layer surfaced an error while reading or writing bytecode.
    #[error("bytecode cache error: {0}")]
    Cache(String),

    /// Any other failure — carries a free-form message.
    #[error("{0}")]
    Other(String),
}

impl RpcError {
    /// Whether the error is worth retrying.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Transient(_) | Self::RateLimited | Self::Timeout { .. }
        )
    }
}
