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

/// Detect the "block range too wide" / "too many results" family of errors
/// that free-tier RPC providers return for large `eth_getLogs` queries.
///
/// We substring-match on the error message payload across the variants that
/// carry free-form strings. Matched on:
///   - `"eth_getLogs requests with up to a 10 block range"` (Alchemy free)
///   - `"eth_getLogs is limited to"`                        (Alchemy variant)
///   - `"block range is too wide"`                          (Infura)
///   - `"exceed maximum block range"`                       (`QuickNode` & others)
///   - `"query returned more than"`                         (result-size limit)
///
/// Callers that walk log histories use this to degrade gracefully — a
/// missing range-limited history is a warning, not a hard failure.
#[must_use]
pub fn is_rpc_range_limited(err: &RpcError) -> bool {
    let msg = match err {
        RpcError::Server(s) | RpcError::Transient(s) | RpcError::Other(s) | RpcError::Cache(s) => {
            s.as_str()
        }
        RpcError::InvalidUrl { detail, .. } => detail.as_str(),
        RpcError::NoProviderConfigured { .. }
        | RpcError::RateLimited
        | RpcError::Timeout { .. } => return false,
    };
    let lower = msg.to_ascii_lowercase();
    lower.contains("eth_getlogs requests with up to")
        || lower.contains("eth_getlogs is limited to")
        || lower.contains("block range is too wide")
        || lower.contains("exceed maximum block range")
        || lower.contains("query returned more than")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_alchemy_free_tier_phrasing() {
        let e =
            RpcError::Server("eth_getLogs requests with up to a 10 block range are allowed".into());
        assert!(is_rpc_range_limited(&e));
    }

    #[test]
    fn detects_alchemy_alt_phrasing() {
        let e = RpcError::Server("eth_getLogs is limited to 10000 blocks".into());
        assert!(is_rpc_range_limited(&e));
    }

    #[test]
    fn detects_infura_phrasing() {
        let e = RpcError::Other("block range is too wide".into());
        assert!(is_rpc_range_limited(&e));
    }

    #[test]
    fn detects_quicknode_phrasing() {
        let e = RpcError::Server("exceed maximum block range of 5000".into());
        assert!(is_rpc_range_limited(&e));
    }

    #[test]
    fn detects_result_size_limit() {
        let e = RpcError::Server("query returned more than 10000 results".into());
        assert!(is_rpc_range_limited(&e));
    }

    #[test]
    fn ignores_unrelated_errors() {
        assert!(!is_rpc_range_limited(&RpcError::RateLimited));
        assert!(!is_rpc_range_limited(&RpcError::Timeout { secs: 5 }));
        assert!(!is_rpc_range_limited(&RpcError::Server(
            "unknown method".into()
        )));
        assert!(!is_rpc_range_limited(&RpcError::Transient(
            "conn reset".into()
        )));
    }

    #[test]
    fn case_insensitive_match() {
        let e = RpcError::Server("Block Range Is Too Wide".into());
        assert!(is_rpc_range_limited(&e));
    }
}
