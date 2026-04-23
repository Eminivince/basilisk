//! Shared foundation for the Basilisk workspace.
//!
//! Holds the types and plumbing every other crate needs: configuration
//! loading, the workspace-wide error type, the `Target` enum and its
//! siblings, and the [`detect`] entry point that classifies an input string
//! into a `Target`.

pub mod chain;
pub mod config;
pub mod detect;
pub mod error;
pub mod target;

pub use chain::{Chain, ChainParseError};
pub use config::Config;
pub use detect::detect;
pub use error::{Error, Result};
pub use target::{
    address_hex, parse_address, AddressParseError, GitRef, ProjectKind, Target, UnknownReason,
};
