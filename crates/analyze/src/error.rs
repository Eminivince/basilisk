//! Error types for the analytical tools.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AnalyzeError {
    /// Caller asked about an address that isn't part of the resolved
    /// system. The agent should `resolve_onchain_system` first.
    #[error("address {0:?} not present in resolved system")]
    UnknownAddress(String),

    /// Selector input wasn't a 4-byte hex string.
    #[error("selector must be 4 bytes hex (0x12345678); got {0}")]
    InvalidSelector(String),

    /// Address input wasn't a 20-byte hex string.
    #[error("address must be 20 bytes hex (0x...); got {0}")]
    InvalidAddress(String),

    /// Underlying execution-backend failure (forwarded from
    /// `simulate_call_chain`).
    #[error("execution backend error: {0}")]
    Exec(#[from] basilisk_exec::ExecError),

    /// Catch-all.
    #[error("{0}")]
    Other(String),
}
