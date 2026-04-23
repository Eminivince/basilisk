//! Shared types for explorer results and audit trails.

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use alloy_primitives::{Address, Bytes};
use serde::{Deserialize, Serialize};

/// A single verified source payload, normalized across explorers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedSource {
    /// Source files keyed by their declared path. Paths are sanitized to
    /// prevent `..`/absolute traversal (see [`crate::source_explorer::sanitize_path`]).
    pub source_files: BTreeMap<PathBuf, String>,
    /// Primary contract name (the one targeted for verification).
    pub contract_name: String,
    /// Compiler version, e.g. `0.8.20+commit.a1b79de6`.
    pub compiler_version: String,
    /// Optimizer settings if reported.
    pub optimizer: Option<OptimizerSettings>,
    /// EVM version target if reported.
    pub evm_version: Option<String>,
    /// Contract ABI as JSON (array of entries).
    pub abi: serde_json::Value,
    /// Constructor arguments if the explorer surfaces them.
    pub constructor_args: Option<Bytes>,
    /// SPDX license string if present in metadata.
    pub license: Option<String>,
    /// Some explorers flag the fetched contract as a proxy.
    pub proxy_hint: Option<Address>,
    /// ...and some also surface the implementation address.
    pub implementation_hint: Option<Address>,
    /// Raw explorer response for debugging/audit purposes.
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptimizerSettings {
    pub enabled: bool,
    pub runs: u32,
}

/// How closely a verification match fits — Sourcify-specific distinction
/// but useful to surface generally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchQuality {
    /// Bytecode + metadata both match exactly.
    Full,
    /// Bytecode matches, metadata hash may differ (source likely identical).
    Partial,
}

/// One attempt against one explorer, win or lose.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplorerAttempt {
    pub explorer: String,
    pub outcome: ExplorerOutcome,
    #[serde(with = "duration_millis")]
    pub duration: Duration,
}

/// The outcome of a single explorer attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExplorerOutcome {
    Found { match_quality: MatchQuality },
    NotVerified,
    ChainUnsupported,
    NoApiKey,
    NetworkError(String),
    RateLimited,
    Other(String),
}

/// Result of an [`crate::ExplorerChain::resolve`] call: the winning source
/// (if any) plus the full attempt trail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolutionAttempt {
    /// `(explorer_name, source)` if any explorer returned a match.
    pub result: Option<(String, VerifiedSource)>,
    /// Every explorer we tried, in order, including those that succeeded or were skipped.
    pub attempts: Vec<ExplorerAttempt>,
}

mod duration_millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        // `as_millis` returns u128; we keep it as u64 since nobody cares about > 500 years.
        let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        s.serialize_u64(ms)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let ms = u64::deserialize(d)?;
        Ok(Duration::from_millis(ms))
    }
}
