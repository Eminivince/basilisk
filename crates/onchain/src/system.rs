//! System-level resolution types: `ResolvedSystem`, `ExpansionLimits`, and
//! stats/truncation bookkeeping.

use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime},
};

use alloy_primitives::Address;
use basilisk_core::Chain;
use basilisk_graph::{ContractGraph, GraphEdge};
use serde::{Deserialize, Serialize};

use crate::resolved::ResolvedContract;

/// Every contract reachable from a root, plus the typed graph describing
/// how they relate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedSystem {
    pub root: Address,
    pub chain: Chain,
    pub contracts: BTreeMap<Address, ResolvedContract>,
    pub graph: ContractGraph,
    pub stats: SystemResolutionStats,
    #[serde(with = "crate::time_serde")]
    pub resolved_at: SystemTime,
}

/// Aggregate metrics for one `resolve_system` run.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SystemResolutionStats {
    pub contracts_resolved: usize,
    pub contracts_failed: Vec<FailedResolution>,
    pub expansion_truncated: Vec<TruncationReason>,
    pub total_rpc_calls: u64,
    pub total_explorer_calls: u64,
    pub duration: Duration,
}

/// A single contract we tried to resolve but couldn't. Carries the edge
/// path that led us to the node, for human follow-up.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailedResolution {
    pub address: Address,
    pub reached_via: Vec<GraphEdge>,
    pub error: String,
}

/// Why expansion stopped short of covering the whole reachable set.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TruncationReason {
    /// An expansion hit `max_depth` before being added to the queue.
    MaxDepthReached { at_address: Address, depth: usize },
    /// The contract budget was exhausted.
    MaxContractsReached { last_attempted: Address },
    /// The duration budget was exhausted.
    MaxTimeReached,
}

/// Tunable limits passed to `OnchainIngester::resolve_system`.
///
/// Defaults are conservative enough to complete a typical mainnet proxy
/// system within a few minutes; override any field independently.
#[allow(clippy::struct_excessive_bools)] // Every bool here is an independent expansion toggle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpansionLimits {
    pub max_depth: usize,
    pub max_contracts: usize,
    pub max_duration: Duration,
    pub expand_storage: bool,
    pub expand_bytecode: bool,
    pub expand_immutables: bool,
    pub fetch_history: bool,
    pub fetch_constructor_args: bool,
    pub fetch_storage_layout: bool,
    pub storage_scan_depth: usize,
    pub history_from_block: u64,
    /// How many contracts may be resolved concurrently (spec default: 4).
    pub parallelism: usize,
}

impl Default for ExpansionLimits {
    fn default() -> Self {
        Self {
            max_depth: 3,
            max_contracts: 50,
            max_duration: Duration::from_secs(5 * 60),
            expand_storage: true,
            expand_bytecode: true,
            expand_immutables: true,
            fetch_history: true,
            fetch_constructor_args: true,
            fetch_storage_layout: true,
            storage_scan_depth: 256,
            history_from_block: 0,
            parallelism: 4,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_limits_match_spec() {
        let d = ExpansionLimits::default();
        assert_eq!(d.max_depth, 3);
        assert_eq!(d.max_contracts, 50);
        assert_eq!(d.max_duration, Duration::from_secs(300));
        assert_eq!(d.storage_scan_depth, 256);
        assert_eq!(d.history_from_block, 0);
        assert_eq!(d.parallelism, 4);
        assert!(d.expand_storage && d.expand_bytecode && d.expand_immutables);
        assert!(d.fetch_history && d.fetch_constructor_args && d.fetch_storage_layout);
    }

    #[test]
    fn truncation_reason_serde_round_trip() {
        let r = TruncationReason::MaxDepthReached {
            at_address: Address::from([1u8; 20]),
            depth: 3,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: TruncationReason = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn stats_default_is_empty() {
        let s = SystemResolutionStats::default();
        assert_eq!(s.contracts_resolved, 0);
        assert_eq!(s.total_rpc_calls, 0);
        assert!(s.contracts_failed.is_empty());
        assert!(s.expansion_truncated.is_empty());
    }
}
