//! On-chain ingestion for Basilisk.
//!
//! [`OnchainIngester`] is the entry point. Its `resolve(address)` returns a
//! single enriched [`ResolvedContract`]; `resolve_system(address, limits)`
//! (checkpoint 8) expands from a root into a [`ResolvedSystem`] with a
//! typed [`basilisk_graph::ContractGraph`].

pub mod display;
pub mod enrichment;
pub mod error;
pub mod ingester;
pub mod proxy;
pub mod resolved;
pub mod system;
pub(crate) mod time_serde;

pub use enrichment::{
    AddressReference, ConstructorArgs, DecodedArg, ReferenceSource, StorageLayout,
    StorageLayoutSource, StorageSlot,
};
pub use error::IngestError;
pub use ingester::{OnchainIngester, DEFAULT_TIMEOUT_SECS};
pub use proxy::{detect_proxy, DiamondFacet, ProxyEvidence, ProxyInfo, ProxyKind, UpgradeEvent};
pub use resolved::{ResolutionSources, ResolvedContract};
pub use system::{
    ExpansionLimits, FailedResolution, ResolvedSystem, SystemResolutionStats, TruncationReason,
};
