//! Proxy-detection result types.

use alloy_primitives::{Address, B256};
use serde::{Deserialize, Serialize};

/// Structured proxy classification. Returned by [`crate::detect_proxy`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyInfo {
    pub kind: ProxyKind,
    pub implementation_address: Option<Address>,
    pub admin_address: Option<Address>,
    pub beacon_address: Option<Address>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facets: Vec<DiamondFacet>,
    pub detection_evidence: Vec<ProxyEvidence>,
    /// Past implementations surfaced via `Upgraded(address)` event scans.
    /// Empty if history wasn't fetched or the proxy isn't upgradeable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upgrade_history: Vec<UpgradeEvent>,
}

/// Canonical proxy patterns Basilisk recognizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProxyKind {
    /// EIP-1967 impl slot populated AND admin slot populated.
    Eip1967Transparent,
    /// EIP-1967 impl slot populated WITHOUT admin slot (admin logic in impl).
    Eip1967Uups,
    /// EIP-1967 beacon slot populated.
    Eip1967Beacon,
    /// EIP-1167 minimal proxy (bytecode signature match).
    Eip1167Minimal,
    /// EIP-2535 Diamond — `facets()` returned a non-empty list.
    Eip2535Diamond,
    /// Storage signals suggest a proxy but don't fit any known shape.
    UnknownProxyPattern,
}

/// One facet of an EIP-2535 diamond.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiamondFacet {
    pub facet_address: Address,
    pub selectors: Vec<[u8; 4]>,
}

/// A single human-readable signal observed during detection. Carries the
/// raw value (hex string) so a reader can independently verify the call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProxyEvidence {
    pub signal: String,
    pub value: String,
}

impl ProxyEvidence {
    pub fn new(signal: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            signal: signal.into(),
            value: value.into(),
        }
    }
}

/// One entry in an upgradeable proxy's historical impl list.
///
/// `old_implementation` is walked after sort-by-block: the previous event's
/// `new_implementation` becomes the next event's `old_implementation`. The
/// first event in the history has `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeEvent {
    pub block_number: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_timestamp: Option<u64>,
    pub tx_hash: B256,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_implementation: Option<Address>,
    pub new_implementation: Address,
    /// Human-readable event signature we matched (`Upgraded(address)`,
    /// `BeaconUpgraded(address)`, etc.).
    pub event_signature: String,
}
