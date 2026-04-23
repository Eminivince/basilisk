//! Source-verification explorer clients for Basilisk.
//!
//! Each implementation of [`SourceExplorer`] talks to one verification
//! service (Sourcify / Etherscan / Blockscout) and returns a uniform
//! [`VerifiedSource`] on success. [`ExplorerChain`] composes explorers
//! into a fallback chain with per-attempt audit trail and on-disk caching.

pub mod chain;
pub mod error;
pub mod source_explorer;
pub mod sourcify;
pub mod types;

pub use chain::ExplorerChain;
pub use error::ExplorerError;
pub use source_explorer::SourceExplorer;
pub use sourcify::Sourcify;
pub use types::{
    ExplorerAttempt, ExplorerOutcome, MatchQuality, OptimizerSettings, ResolutionAttempt,
    VerifiedSource,
};
