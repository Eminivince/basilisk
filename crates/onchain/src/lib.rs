//! On-chain ingestion for Basilisk.
//!
//! [`OnchainIngester`] is the entry point: given a chain + config, call
//! [`OnchainIngester::resolve`] with an address to get a
//! [`ResolvedContract`] — bytecode + verified source (if found) + proxy
//! classification + one-hop implementation resolution, with a full
//! audit trail in [`ResolutionSources`].

pub mod display;
pub mod error;
pub mod ingester;
pub mod proxy;
pub mod resolved;
pub(crate) mod time_serde;

pub use error::IngestError;
pub use ingester::{OnchainIngester, DEFAULT_TIMEOUT_SECS};
pub use proxy::{detect_proxy, DiamondFacet, ProxyEvidence, ProxyInfo, ProxyKind};
pub use resolved::{ResolutionSources, ResolvedContract};
