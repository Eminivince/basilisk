//! `ResolvedContract` and its supporting types.

use std::time::SystemTime;

use alloy_primitives::{Address, Bytes, B256};
use basilisk_core::Chain;
use basilisk_explorers::{ExplorerAttempt, VerifiedSource};
use serde::{Deserialize, Serialize};

use crate::proxy::ProxyInfo;

/// A single contract fully resolved from on-chain data plus explorer lookups.
///
/// `implementation` carries the one-hop recursion result: if this contract
/// is a proxy, the referenced implementation is itself resolved once (same
/// shape, no further recursion).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedContract {
    pub address: Address,
    pub chain: Chain,
    pub bytecode: Bytes,
    pub bytecode_hash: B256,
    pub is_contract: bool,
    pub source: Option<VerifiedSource>,
    pub proxy: Option<ProxyInfo>,
    /// One-hop implementation; only populated when this contract is a proxy
    /// whose implementation address we could determine.
    pub implementation: Option<Box<ResolvedContract>>,
    #[serde(with = "crate::time_serde")]
    pub fetched_at: SystemTime,
    pub resolution: ResolutionSources,
}

/// Audit trail: which components returned what during resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolutionSources {
    /// Which RPC endpoint served bytecode (redacted).
    pub bytecode_rpc: String,
    /// Every explorer we tried, with its outcome and timing.
    pub source_attempts: Vec<ExplorerAttempt>,
    /// Name of the explorer that won, if any.
    pub source_winner: Option<String>,
    /// Free-form notes about proxy detection and partial-timeout handling.
    pub proxy_detection_notes: Vec<String>,
}

impl ResolutionSources {
    pub fn new(bytecode_rpc: impl Into<String>) -> Self {
        Self {
            bytecode_rpc: bytecode_rpc.into(),
            source_attempts: Vec::new(),
            source_winner: None,
            proxy_detection_notes: Vec::new(),
        }
    }
}
