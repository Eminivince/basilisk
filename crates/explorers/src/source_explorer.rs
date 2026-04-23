//! The [`SourceExplorer`] trait and shared helpers.

use std::path::{Component, Path, PathBuf};

use alloy_primitives::Address;
use async_trait::async_trait;
use basilisk_core::Chain;

use crate::{
    error::ExplorerError,
    types::{CreationInfo, VerifiedSource},
};

/// A single source-verification explorer.
///
/// Implementations are expected to be cheap to clone or share (`Arc<Self>`).
#[async_trait]
pub trait SourceExplorer: Send + Sync {
    /// Short, stable name for the explorer (`"sourcify"`, `"etherscan"`, ...).
    /// Used in audit trails and logs.
    fn name(&self) -> &'static str;

    /// Look up verified source for `address` on `chain`.
    /// - `Ok(Some(_))` — verified source located.
    /// - `Ok(None)` — contract is not verified by this explorer.
    /// - `Err(_)` — a failure that a fallback chain may skip past.
    async fn fetch_source(
        &self,
        chain: &Chain,
        address: Address,
    ) -> Result<Option<VerifiedSource>, ExplorerError>;

    /// Look up the creation transaction for `address` on `chain`.
    ///
    /// Default impl returns [`ExplorerError::Unsupported`] so explorers that
    /// don't have this endpoint (e.g. Sourcify) are transparent to callers.
    async fn fetch_creation(
        &self,
        _chain: &Chain,
        _address: Address,
    ) -> Result<Option<CreationInfo>, ExplorerError> {
        Err(ExplorerError::Unsupported)
    }
}

/// Normalize an explorer-supplied source path into something safe to use as
/// a map key.
///
/// We reject absolute paths and collapse `..` components so explorer
/// responses can't direct later code to walk outside the intended source tree.
pub fn sanitize_path(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(os) => out.push(os),
            // Skip empty "." segments.
            Component::CurDir => {}
            // Reject parent-directory climbs and anything that isn't plain.
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_path;

    #[test]
    fn accepts_plain_relative_paths() {
        assert_eq!(
            sanitize_path("contracts/Token.sol")
                .unwrap()
                .to_str()
                .unwrap(),
            "contracts/Token.sol"
        );
        assert_eq!(
            sanitize_path("Token.sol").unwrap().to_str().unwrap(),
            "Token.sol"
        );
    }

    #[test]
    fn rejects_absolute_paths() {
        assert!(sanitize_path("/etc/passwd").is_none());
    }

    #[test]
    fn rejects_parent_dir_traversal() {
        assert!(sanitize_path("../Token.sol").is_none());
        assert!(sanitize_path("contracts/../../Token.sol").is_none());
    }

    #[test]
    fn strips_curdir_components() {
        let p = sanitize_path("./contracts/./Token.sol").unwrap();
        assert_eq!(p.to_str().unwrap(), "contracts/Token.sol");
    }

    #[test]
    fn rejects_empty_and_whitespace() {
        assert!(sanitize_path("").is_none());
        assert!(sanitize_path("   ").is_none());
    }
}
