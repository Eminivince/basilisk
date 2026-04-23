//! Orchestrator error type.

use thiserror::Error;

/// Errors surfaced by [`crate::OnchainIngester::new`] and related paths.
#[derive(Debug, Error)]
pub enum IngestError {
    /// RPC layer couldn't be constructed or was misconfigured.
    #[error("RPC configuration error: {0}")]
    Rpc(#[from] basilisk_rpc::RpcError),

    /// Bytecode fetch never completed within the allotted window. Distinct
    /// from the softer "source/proxy timed out" case — those are recorded
    /// in [`crate::ResolutionSources`] without failing the call.
    #[error("bytecode fetch timed out after {0}s")]
    BytecodeTimeout(u64),

    /// A generic ingestion failure with a human-readable message.
    #[error("{0}")]
    Other(String),
}
