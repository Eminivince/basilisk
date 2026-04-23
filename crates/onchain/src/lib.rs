//! On-chain ingestion for Basilisk.
//!
//! Checkpoint 5 ships the proxy-detection module: pure-ish logic (bytecode
//! pattern matching + storage-slot reads + one `eth_call`) that classifies
//! a deployed contract into one of the canonical proxy patterns. The
//! orchestrator that composes this with bytecode fetching and explorer
//! lookups lands in checkpoint 6.

pub mod proxy;

pub use proxy::{detect_proxy, DiamondFacet, ProxyEvidence, ProxyInfo, ProxyKind};
