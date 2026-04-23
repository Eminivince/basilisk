//! Public types surfaced from [`crate::RepoCache`].

use std::{
    path::PathBuf,
    time::{Duration, SystemTime},
};

use basilisk_core::GitRef;
use basilisk_github::GithubClient;
use serde::{Deserialize, Serialize};

/// Knobs for a single [`crate::RepoCache::fetch`] call.
#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    /// `Shallow` (default) for `--depth 1`; `Full` when we need history.
    pub strategy: CloneStrategy,
    /// Bypass the on-disk cache for this run. Still writes on success.
    pub force_refresh: bool,
    /// Optional GitHub client for ref / short-SHA disambiguation.
    pub github: Option<GithubClient>,
}

/// Depth of the clone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CloneStrategy {
    /// `git clone --depth 1 --branch <ref>`. Fast, minimal disk use.
    #[default]
    Shallow,
    /// Complete history. Required for arbitrary-commit checkouts that
    /// shallow clones can't reach.
    Full,
}

/// Result of a successful fetch.
#[derive(Debug, Clone)]
pub struct FetchedRepo {
    pub owner: String,
    pub repo: String,
    /// The full 40-character commit SHA this checkout points at.
    pub commit_sha: String,
    /// The resolved ref (Branch / Tag / Commit) we were asked for.
    pub reference: GitRef,
    /// Absolute path to the checkout.
    pub working_tree: PathBuf,
    /// `true` if we reused an existing cache entry without cloning.
    pub cached: bool,
    pub cloned_at: SystemTime,
}

/// Summary returned by [`crate::RepoCache::stats`].
#[derive(Debug, Clone, Default)]
pub struct RepoCacheStats {
    pub repos_count: usize,
    pub total_bytes: u64,
    pub oldest_clone: Option<SystemTime>,
    pub newest_clone: Option<SystemTime>,
}

/// What we write next to each working tree so later reads can tell where it
/// came from and how fresh it is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoMetadata {
    pub owner: String,
    pub repo: String,
    pub commit_sha: String,
    pub original_ref: GitRef,
    pub clone_depth: CloneDepth,
    #[serde(with = "crate::time_serde")]
    pub cloned_at: SystemTime,
}

/// Serializable depth record for the metadata file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CloneDepth {
    Shallow,
    Full,
}

impl From<CloneStrategy> for CloneDepth {
    fn from(s: CloneStrategy) -> Self {
        match s {
            CloneStrategy::Shallow => Self::Shallow,
            CloneStrategy::Full => Self::Full,
        }
    }
}

/// Name of the metadata sidecar file dropped into each cache entry.
pub const METADATA_FILENAME: &str = ".basilisk-meta.json";

/// Duration helper re-exported for consumers that build `FetchOptions`
/// programmatically (they can pass `Duration::from_secs(...)` for timeouts
/// we may add in a later checkpoint).
pub type FetchDuration = Duration;

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_meta() -> RepoMetadata {
        RepoMetadata {
            owner: "foundry-rs".into(),
            repo: "forge-template".into(),
            commit_sha: "a".repeat(40),
            original_ref: GitRef::Branch("main".into()),
            clone_depth: CloneDepth::Shallow,
            cloned_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        }
    }

    #[test]
    fn metadata_round_trips_through_json() {
        let meta = sample_meta();
        let json = serde_json::to_string(&meta).unwrap();
        let back: RepoMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back.owner, meta.owner);
        assert_eq!(back.repo, meta.repo);
        assert_eq!(back.commit_sha, meta.commit_sha);
        assert_eq!(back.original_ref, meta.original_ref);
        assert_eq!(back.clone_depth, meta.clone_depth);
        assert_eq!(back.cloned_at, meta.cloned_at);
    }

    #[test]
    fn clone_strategy_defaults_to_shallow() {
        assert_eq!(CloneStrategy::default(), CloneStrategy::Shallow);
    }

    #[test]
    fn fetch_options_default_is_shallow_no_refresh_no_github() {
        let opts = FetchOptions::default();
        assert_eq!(opts.strategy, CloneStrategy::Shallow);
        assert!(!opts.force_refresh);
        assert!(opts.github.is_none());
    }

    #[test]
    fn clone_strategy_maps_into_depth() {
        assert_eq!(
            CloneDepth::from(CloneStrategy::Shallow),
            CloneDepth::Shallow
        );
        assert_eq!(CloneDepth::from(CloneStrategy::Full), CloneDepth::Full);
    }

    #[test]
    fn metadata_filename_is_hidden() {
        assert!(METADATA_FILENAME.starts_with('.'));
    }

    #[test]
    fn repo_cache_stats_default_is_empty() {
        let s = RepoCacheStats::default();
        assert_eq!(s.repos_count, 0);
        assert_eq!(s.total_bytes, 0);
        assert!(s.oldest_clone.is_none());
        assert!(s.newest_clone.is_none());
    }
}
