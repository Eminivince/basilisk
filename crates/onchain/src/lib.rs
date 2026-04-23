//! On-chain ingestion for Basilisk.
//!
//! [`OnchainIngester`] is the entry point. Its `resolve(address)` returns a
//! single enriched [`ResolvedContract`]; `resolve_system(address, limits)`
//! (checkpoint 8) expands from a root into a [`ResolvedSystem`] with a
//! typed [`basilisk_graph::ContractGraph`].

pub mod constructor;
pub mod display;
pub mod enrichment;
pub mod error;
pub mod history;
pub mod ingester;
pub mod proxy;
pub mod references;
pub mod resolved;
pub mod storage_layout;
pub mod system;
pub(crate) mod time_serde;

pub use constructor::recover_constructor_args;
pub use enrichment::{
    AddressReference, ConstructorArgs, DecodedArg, ReferenceSource, StorageLayout,
    StorageLayoutSource, StorageSlot,
};
pub use error::IngestError;
pub use history::fetch_upgrade_history;
pub use ingester::{OnchainIngester, DEFAULT_TIMEOUT_SECS};
pub use proxy::{detect_proxy, DiamondFacet, ProxyEvidence, ProxyInfo, ProxyKind, UpgradeEvent};
pub use references::{
    extract_immutable_addresses, scan_bytecode_for_addresses, scan_storage_for_addresses,
    verify_bytecode_address_references,
};
pub use resolved::{ResolutionSources, ResolvedContract};
pub use storage_layout::recover_storage_layout;
pub use system::{
    ExpansionLimits, FailedResolution, ResolvedSystem, SystemResolutionStats, TruncationReason,
};
