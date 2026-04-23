//! The `Target` enum describes what Basilisk has been asked to audit.
//!
//! Phase 1 only defines the shape. Detection — turning an arbitrary input
//! string into a populated `Target` — lands in a later instruction set.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A thing to audit.
///
/// Variants are intentionally data-bearing so later detection logic can
/// populate them with parsed metadata (repo coordinates, chain id, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    /// A GitHub repository, optionally pinned to a ref (branch, tag, or commit).
    Github {
        owner: String,
        repo: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        git_ref: Option<String>,
    },
    /// A deployed contract identified by address on a specific chain.
    OnChain {
        address: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chain: Option<String>,
    },
    /// A local filesystem path to a project or file.
    LocalPath { path: String },
    /// The detector could not classify the input; `reason` explains why.
    Unknown {
        raw: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

impl Target {
    /// Short label suitable for logs and status output.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Github { .. } => "github",
            Self::OnChain { .. } => "on_chain",
            Self::LocalPath { .. } => "local_path",
            Self::Unknown { .. } => "unknown",
        }
    }
}
