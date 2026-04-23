//! Persistent repo cache. Pure-filesystem: the git2-backed `fetch()` that
//! populates these directories lives in `crate::fetch` (`CP3c`). Everything
//! here is testable without a network or libgit2.
//!
//! Layout:
//! ```text
//! <root>/
//!   <owner>/
//!     <repo>/
//!       <full_commit_sha>/
//!         .basilisk-meta.json
//!         .git/
//!         ...working tree
//! ```

use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use crate::{
    error::GitError,
    types::{RepoCacheStats, RepoMetadata, METADATA_FILENAME},
};

/// Default subdirectory under `~/.basilisk/` for the repo cache.
pub const DEFAULT_CACHE_SUBDIR: &str = ".basilisk/repos";

/// Persistent, content-addressed repo cache.
#[derive(Debug, Clone)]
pub struct RepoCache {
    root: PathBuf,
}

impl RepoCache {
    /// Open the user-default cache at `$HOME/.basilisk/repos/`. Falls back
    /// to `./.basilisk-repos/` when `$HOME` can't be resolved.
    pub fn open() -> Result<Self, GitError> {
        let root = default_cache_root()?;
        Self::open_at(root)
    }

    /// Open a cache rooted at an explicit directory. Creates the directory
    /// if it doesn't exist.
    pub fn open_at(root: PathBuf) -> Result<Self, GitError> {
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// The root directory of this cache.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path where a given `(owner, repo, sha)` checkout would live.
    /// Does not create the directory.
    pub fn entry_path(&self, owner: &str, repo: &str, sha: &str) -> PathBuf {
        self.root.join(owner).join(repo).join(sha)
    }

    /// Read the metadata sidecar for an entry, if present.
    pub fn read_metadata(
        &self,
        owner: &str,
        repo: &str,
        sha: &str,
    ) -> Result<Option<RepoMetadata>, GitError> {
        let path = self.entry_path(owner, repo, sha).join(METADATA_FILENAME);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&path)?;
        let meta: RepoMetadata =
            serde_json::from_slice(&bytes).map_err(|e| GitError::CacheCorrupt {
                path: path.display().to_string(),
                detail: e.to_string(),
            })?;
        Ok(Some(meta))
    }

    /// Write the metadata sidecar for an entry. Overwrites any existing
    /// file at the target path.
    pub fn write_metadata(&self, meta: &RepoMetadata) -> Result<(), GitError> {
        let dir = self.entry_path(&meta.owner, &meta.repo, &meta.commit_sha);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(METADATA_FILENAME);
        let bytes = serde_json::to_vec_pretty(meta).map_err(|e| GitError::Other(e.to_string()))?;
        std::fs::write(&path, &bytes)?;
        Ok(())
    }

    /// `true` iff there's a directory at the expected path AND it contains
    /// a readable metadata sidecar pointing at this SHA.
    pub fn is_cached(&self, owner: &str, repo: &str, sha: &str) -> bool {
        match self.read_metadata(owner, repo, sha) {
            Ok(Some(meta)) => meta.commit_sha == sha,
            _ => false,
        }
    }

    /// Remove the entire cache. Returns the number of bytes freed.
    pub fn clear(&self) -> Result<u64, GitError> {
        let bytes = dir_bytes(&self.root).unwrap_or(0);
        if self.root.exists() {
            std::fs::remove_dir_all(&self.root)?;
        }
        std::fs::create_dir_all(&self.root)?;
        Ok(bytes)
    }

    /// Remove every entry whose owner prefix matches `owner` (and, if
    /// supplied, the `repo` subdir under it). Returns bytes freed.
    pub fn clear_scoped(&self, owner: Option<&str>, repo: Option<&str>) -> Result<u64, GitError> {
        let target = match (owner, repo) {
            (Some(o), Some(r)) => self.root.join(o).join(r),
            (Some(o), None) => self.root.join(o),
            _ => return self.clear(),
        };
        let bytes = dir_bytes(&target).unwrap_or(0);
        if target.exists() {
            std::fs::remove_dir_all(&target)?;
        }
        Ok(bytes)
    }

    /// Walk the cache, return `(owner, repo, sha, metadata)` for every
    /// well-formed entry. Corrupt entries are skipped with a log.
    pub fn list(&self) -> Result<Vec<(String, String, String, RepoMetadata)>, GitError> {
        let mut out = Vec::new();
        if !self.root.exists() {
            return Ok(out);
        }
        for owner_entry in std::fs::read_dir(&self.root)? {
            let owner_entry = owner_entry?;
            if !owner_entry.file_type()?.is_dir() {
                continue;
            }
            let owner = owner_entry.file_name().to_string_lossy().to_string();
            for repo_entry in std::fs::read_dir(owner_entry.path())? {
                let repo_entry = repo_entry?;
                if !repo_entry.file_type()?.is_dir() {
                    continue;
                }
                let repo = repo_entry.file_name().to_string_lossy().to_string();
                for sha_entry in std::fs::read_dir(repo_entry.path())? {
                    let sha_entry = sha_entry?;
                    if !sha_entry.file_type()?.is_dir() {
                        continue;
                    }
                    let sha = sha_entry.file_name().to_string_lossy().to_string();
                    match self.read_metadata(&owner, &repo, &sha) {
                        Ok(Some(meta)) => out.push((owner.clone(), repo.clone(), sha, meta)),
                        Ok(None) => {
                            tracing::debug!(
                                path = %sha_entry.path().display(),
                                "cache entry missing metadata; skipped",
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                path = %sha_entry.path().display(),
                                error = %e,
                                "cache entry metadata unreadable; skipped",
                            );
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// Aggregate counts + byte total + oldest/newest clone times.
    pub fn stats(&self) -> Result<RepoCacheStats, GitError> {
        let entries = self.list()?;
        let mut stats = RepoCacheStats {
            repos_count: entries.len(),
            ..RepoCacheStats::default()
        };
        for (owner, repo, sha, meta) in entries {
            let size = dir_bytes(&self.entry_path(&owner, &repo, &sha)).unwrap_or(0);
            stats.total_bytes = stats.total_bytes.saturating_add(size);
            stats.oldest_clone = Some(min_or_new(stats.oldest_clone, meta.cloned_at));
            stats.newest_clone = Some(max_or_new(stats.newest_clone, meta.cloned_at));
        }
        Ok(stats)
    }
}

/// Default cache root: `$HOME/.basilisk/repos/`, falling back to
/// `./.basilisk-repos/` if `$HOME` isn't set.
pub fn default_cache_root() -> Result<PathBuf, GitError> {
    if let Some(home) = dirs::home_dir() {
        Ok(home.join(DEFAULT_CACHE_SUBDIR))
    } else {
        let cwd = std::env::current_dir()?;
        Ok(cwd.join(".basilisk-repos"))
    }
}

fn dir_bytes(path: &Path) -> std::io::Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            total = total.saturating_add(dir_bytes(&entry.path())?);
        } else if ft.is_file() {
            total = total.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(total)
}

fn min_or_new(existing: Option<SystemTime>, candidate: SystemTime) -> SystemTime {
    match existing {
        Some(prev) if prev <= candidate => prev,
        _ => candidate,
    }
}

fn max_or_new(existing: Option<SystemTime>, candidate: SystemTime) -> SystemTime {
    match existing {
        Some(prev) if prev >= candidate => prev,
        _ => candidate,
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use basilisk_core::GitRef;
    use tempfile::TempDir;

    use super::*;
    use crate::types::CloneDepth;

    fn meta(owner: &str, repo: &str, sha: &str, seconds: u64) -> RepoMetadata {
        RepoMetadata {
            owner: owner.into(),
            repo: repo.into(),
            commit_sha: sha.into(),
            original_ref: GitRef::Branch("main".into()),
            clone_depth: CloneDepth::Shallow,
            cloned_at: UNIX_EPOCH + Duration::from_secs(seconds),
        }
    }

    #[test]
    fn open_at_creates_root_if_missing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("fresh/nested/dir");
        RepoCache::open_at(root.clone()).unwrap();
        assert!(root.is_dir());
    }

    #[test]
    fn entry_path_composes_owner_repo_sha() {
        let tmp = TempDir::new().unwrap();
        let cache = RepoCache::open_at(tmp.path().to_path_buf()).unwrap();
        let p = cache.entry_path("foo", "bar", "abc");
        assert!(p.ends_with("foo/bar/abc"));
    }

    #[test]
    fn metadata_round_trips_through_disk() {
        let tmp = TempDir::new().unwrap();
        let cache = RepoCache::open_at(tmp.path().to_path_buf()).unwrap();
        let m = meta(
            "foundry-rs",
            "forge-template",
            &"a".repeat(40),
            1_700_000_000,
        );
        cache.write_metadata(&m).unwrap();
        let back = cache
            .read_metadata("foundry-rs", "forge-template", &"a".repeat(40))
            .unwrap()
            .unwrap();
        assert_eq!(back.owner, m.owner);
        assert_eq!(back.commit_sha, m.commit_sha);
        assert_eq!(back.cloned_at, m.cloned_at);
    }

    #[test]
    fn read_metadata_missing_is_none() {
        let tmp = TempDir::new().unwrap();
        let cache = RepoCache::open_at(tmp.path().to_path_buf()).unwrap();
        assert!(cache.read_metadata("a", "b", "c").unwrap().is_none());
    }

    #[test]
    fn read_metadata_corrupt_errors() {
        let tmp = TempDir::new().unwrap();
        let cache = RepoCache::open_at(tmp.path().to_path_buf()).unwrap();
        let dir = cache.entry_path("a", "b", "c");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(METADATA_FILENAME), b"not json").unwrap();
        let err = cache.read_metadata("a", "b", "c").unwrap_err();
        assert!(matches!(err, GitError::CacheCorrupt { .. }), "got {err:?}");
    }

    #[test]
    fn is_cached_true_only_when_metadata_matches_sha() {
        let tmp = TempDir::new().unwrap();
        let cache = RepoCache::open_at(tmp.path().to_path_buf()).unwrap();
        let sha = "a".repeat(40);
        cache.write_metadata(&meta("foo", "bar", &sha, 1)).unwrap();
        assert!(cache.is_cached("foo", "bar", &sha));
        assert!(!cache.is_cached("foo", "bar", &"b".repeat(40)));
    }

    #[test]
    fn list_returns_every_well_formed_entry() {
        let tmp = TempDir::new().unwrap();
        let cache = RepoCache::open_at(tmp.path().to_path_buf()).unwrap();
        cache
            .write_metadata(&meta("o1", "r1", &"1".repeat(40), 10))
            .unwrap();
        cache
            .write_metadata(&meta("o1", "r2", &"2".repeat(40), 20))
            .unwrap();
        cache
            .write_metadata(&meta("o2", "r3", &"3".repeat(40), 30))
            .unwrap();
        // Orphan dir without metadata — should be skipped silently.
        std::fs::create_dir_all(cache.entry_path("o3", "r4", "no-meta")).unwrap();
        let entries = cache.list().unwrap();
        assert_eq!(entries.len(), 3);
        let owners: Vec<_> = entries.iter().map(|(o, ..)| o.as_str()).collect();
        assert!(owners.contains(&"o1"));
        assert!(owners.contains(&"o2"));
    }

    #[test]
    fn stats_counts_and_times() {
        let tmp = TempDir::new().unwrap();
        let cache = RepoCache::open_at(tmp.path().to_path_buf()).unwrap();
        cache
            .write_metadata(&meta("o", "r", &"1".repeat(40), 100))
            .unwrap();
        cache
            .write_metadata(&meta("o", "r", &"2".repeat(40), 200))
            .unwrap();
        let stats = cache.stats().unwrap();
        assert_eq!(stats.repos_count, 2);
        assert_eq!(
            stats.oldest_clone.unwrap(),
            UNIX_EPOCH + Duration::from_secs(100)
        );
        assert_eq!(
            stats.newest_clone.unwrap(),
            UNIX_EPOCH + Duration::from_secs(200)
        );
        assert!(stats.total_bytes > 0);
    }

    #[test]
    fn clear_removes_everything() {
        let tmp = TempDir::new().unwrap();
        let cache = RepoCache::open_at(tmp.path().to_path_buf()).unwrap();
        cache
            .write_metadata(&meta("o", "r", &"1".repeat(40), 1))
            .unwrap();
        assert_eq!(cache.list().unwrap().len(), 1);
        let freed = cache.clear().unwrap();
        assert!(freed > 0);
        assert_eq!(cache.list().unwrap().len(), 0);
    }

    #[test]
    fn clear_scoped_to_owner() {
        let tmp = TempDir::new().unwrap();
        let cache = RepoCache::open_at(tmp.path().to_path_buf()).unwrap();
        cache
            .write_metadata(&meta("keep", "r", &"1".repeat(40), 1))
            .unwrap();
        cache
            .write_metadata(&meta("drop", "r", &"2".repeat(40), 2))
            .unwrap();
        cache.clear_scoped(Some("drop"), None).unwrap();
        let remaining = cache.list().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].0, "keep");
    }

    #[test]
    fn clear_scoped_to_owner_and_repo() {
        let tmp = TempDir::new().unwrap();
        let cache = RepoCache::open_at(tmp.path().to_path_buf()).unwrap();
        cache
            .write_metadata(&meta("o", "keep", &"1".repeat(40), 1))
            .unwrap();
        cache
            .write_metadata(&meta("o", "drop", &"2".repeat(40), 2))
            .unwrap();
        cache.clear_scoped(Some("o"), Some("drop")).unwrap();
        let remaining = cache.list().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].1, "keep");
    }
}
