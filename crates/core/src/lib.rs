//! Shared foundation for the Basilisk workspace.
//!
//! This crate is intentionally small during Phase 1. It holds the types and
//! plumbing that every other crate needs: configuration loading, the
//! workspace-wide error type, and the `Target` enum that downstream detectors
//! and analyzers will grow into.

pub mod config;
pub mod error;
pub mod target;

pub use config::Config;
pub use error::{Error, Result};
pub use target::Target;
