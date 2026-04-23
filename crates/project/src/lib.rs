//! Source-side project detection and resolution.
//!
//! `basilisk-project` is the counterpart to `basilisk-onchain`: it turns a
//! local filesystem tree (a freshly cloned repo, a user-supplied path, or
//! a subpath narrowed out of a monorepo) into a typed `ResolvedProject`
//! the rest of the auditor can reason about.
//!
//! This is the CP4 entry point: [`ProjectLayout`] + [`detect_layout`]. It
//! classifies the project flavour and records the concrete config /
//! source / test / lib paths we found at the root. No config parsing, no
//! source walking — later checkpoints layer those on.

pub mod error;
pub mod foundry;
pub mod hardhat;
pub(crate) mod js_text;
pub mod layout;
pub mod truffle;

pub use error::ProjectError;
pub use foundry::{
    parse_foundry_config, parse_foundry_toml, parse_remappings_str, parse_remappings_txt,
    FoundryConfig, FoundryProfile, Remapping, DEFAULT_PROFILE,
};
pub use hardhat::{parse_hardhat_config, parse_hardhat_source, HardhatConfig, HardhatStyle};
pub use layout::{detect_layout, ConfigFile, ProjectLayout};
pub use truffle::{parse_truffle_config, parse_truffle_source, TruffleConfig};

// Convenience re-export so callers don't need a direct `basilisk-core`
// dependency just to pattern-match on the layout kind.
pub use basilisk_core::ProjectKind;
