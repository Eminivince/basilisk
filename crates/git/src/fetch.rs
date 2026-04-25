//! `RepoCache::fetch` — the clone-and-cache entry point.
//!
//! Flow:
//!   1. Resolve the user's [`GitRef`] as far as we can without a clone
//!      (default-branch lookup, ambiguous disambiguation, short-SHA
//!      expansion — all via the supplied [`GithubClient`]).
//!   2. If the resolved ref is a full commit SHA, check the cache. Hit →
//!      return `cached: true` without touching git.
//!   3. Clone into a temp dir under the cache root (so `rename` is
//!      same-filesystem and therefore atomic). Shallow by default; if
//!      shallow fails we retry Full and log the fallback.
//!   4. Read HEAD's commit SHA from the clone. This is the
//!      content-addressed cache key.
//!   5. If that SHA is already cached (can happen for Branch/Tag refs
//!      whose tip we couldn't predict), discard the temp and return the
//!      existing entry as `cached: true`.
//!   6. Otherwise rename the temp into `<root>/<owner>/<repo>/<sha>/`
//!      and write the `.basilisk-meta.json` sidecar.
//!
//! Auth: if `GITHUB_TOKEN` is set, we embed it in the clone URL as
//! `https://<token>@github.com/...`. The token never appears in tracing
//! output — we log only the bare `github.com/owner/repo.git`.

use std::{path::Path, time::SystemTime};

use basilisk_core::GitRef;
use basilisk_github::GithubError;
use git2::{build::RepoBuilder, FetchOptions as Git2FetchOptions};

use crate::{
    cache::RepoCache,
    error::GitError,
    types::{
        CloneDepth, CloneStrategy, FetchOptions, FetchedRepo, RepoMetadata, METADATA_FILENAME,
    },
};

impl RepoCache {
    /// Fetch a repo into the cache. See module docs for the flow.
    #[allow(clippy::too_many_lines)] // 7-step flow — splitting hurts readability
    pub async fn fetch(
        &self,
        owner: &str,
        repo: &str,
        reference: Option<GitRef>,
        options: FetchOptions,
    ) -> Result<FetchedRepo, GitError> {
        // 1. Resolve as much as we can without a clone.
        let (resolved_ref, mut known_sha, requires_full) = self
            .resolve_reference(owner, repo, reference, &options)
            .await?;

        // 1b. For Branch/Tag refs without a known SHA yet, try
        // `git ls-remote`-style advertisement. This is a single
        // lightweight HTTP round-trip; hitting it means a cached
        // checkout can return immediately without re-cloning.
        // Silent fallback on any error — we'll fall through to the
        // regular clone path.
        if known_sha.is_none() && !options.force_refresh {
            if let Some(sha) = ls_remote_sha(&clone_url(owner, repo), &resolved_ref).await {
                known_sha = Some(sha);
            }
        }

        // 2. Fast path: known full SHA + cache hit.
        if let Some(sha) = &known_sha {
            if !options.force_refresh && self.is_cached(owner, repo, sha) {
                let meta = self.read_metadata(owner, repo, sha)?.ok_or_else(|| {
                    GitError::CacheCorrupt {
                        path: self.entry_path(owner, repo, sha).display().to_string(),
                        detail: "metadata disappeared between check and read".into(),
                    }
                })?;
                return Ok(FetchedRepo {
                    owner: owner.to_string(),
                    repo: repo.to_string(),
                    commit_sha: sha.clone(),
                    reference: resolved_ref,
                    working_tree: self.entry_path(owner, repo, sha),
                    cached: true,
                    cloned_at: meta.cloned_at,
                });
            }
        }

        // 3. Clone into a temp dir within the cache root (same-fs rename).
        std::fs::create_dir_all(self.root())?;
        let strategy = if requires_full {
            CloneStrategy::Full
        } else {
            options.strategy
        };

        let (clone_dir, actual_strategy) = self
            .clone_with_fallback(owner, repo, &resolved_ref, strategy)
            .await?;

        // 4. Resolve HEAD -> full SHA.
        let head_sha = {
            let path = clone_dir.path().to_path_buf();
            tokio::task::spawn_blocking(move || read_head_sha(&path))
                .await
                .map_err(|e| GitError::Other(format!("join: {e}")))??
        };

        // 5. Second cache check for the Branch/Tag case where we only learn
        //    the SHA after cloning.
        if !options.force_refresh && self.is_cached(owner, repo, &head_sha) {
            let meta = self.read_metadata(owner, repo, &head_sha)?.ok_or_else(|| {
                GitError::CacheCorrupt {
                    path: self
                        .entry_path(owner, repo, &head_sha)
                        .display()
                        .to_string(),
                    detail: "metadata disappeared between check and read".into(),
                }
            })?;
            tracing::info!(
                owner,
                repo,
                sha = %head_sha,
                "cache hit after clone; discarding temp",
            );
            // `clone_dir` (TempDir) drops and cleans up automatically.
            return Ok(FetchedRepo {
                owner: owner.to_string(),
                repo: repo.to_string(),
                commit_sha: head_sha,
                reference: resolved_ref,
                working_tree: self.entry_path(owner, repo, &meta.commit_sha),
                cached: true,
                cloned_at: meta.cloned_at,
            });
        }

        // 6. Atomic-ish move: rename temp → content-addressed cache entry.
        let final_dir = self.entry_path(owner, repo, &head_sha);
        if final_dir.exists() {
            std::fs::remove_dir_all(&final_dir)?;
        }
        if let Some(parent) = final_dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Consume the TempDir so its Drop doesn't try to delete what we just moved.
        let temp_path = clone_dir.keep();
        std::fs::rename(&temp_path, &final_dir).map_err(|e| GitError::CloneFailed {
            detail: format!(
                "rename {} → {}: {e}",
                temp_path.display(),
                final_dir.display()
            ),
        })?;

        // 7. Write metadata sidecar.
        let cloned_at = SystemTime::now();
        let meta = RepoMetadata {
            owner: owner.to_string(),
            repo: repo.to_string(),
            commit_sha: head_sha.clone(),
            original_ref: resolved_ref.clone(),
            clone_depth: CloneDepth::from(actual_strategy),
            cloned_at,
        };
        self.write_metadata(&meta)?;

        tracing::info!(
            owner,
            repo,
            sha = %head_sha,
            strategy = ?actual_strategy,
            "cloned into cache",
        );

        Ok(FetchedRepo {
            owner: owner.to_string(),
            repo: repo.to_string(),
            commit_sha: head_sha,
            reference: resolved_ref,
            working_tree: final_dir,
            cached: false,
            cloned_at,
        })
    }

    /// Resolve the user's `GitRef` (or `None`) into:
    ///   * a concrete ref (Branch/Tag/Commit),
    ///   * the full 40-char commit SHA when we can determine it without cloning,
    ///   * a hint that the caller must use a Full clone (for short SHAs we
    ///     failed to expand).
    async fn resolve_reference(
        &self,
        owner: &str,
        repo: &str,
        reference: Option<GitRef>,
        options: &FetchOptions,
    ) -> Result<(GitRef, Option<String>, bool), GitError> {
        match reference {
            None => {
                // Need default branch; requires a GithubClient.
                if let Some(gh) = &options.github {
                    match gh.default_branch(owner, repo).await {
                        Ok(branch) => Ok((GitRef::Branch(branch), None, false)),
                        Err(e) => Err(map_github_err(owner, repo, e)),
                    }
                } else {
                    Err(GitError::Other(
                        "no ref supplied and no GithubClient configured for \
                         default-branch lookup"
                            .into(),
                    ))
                }
            }
            Some(GitRef::Commit(sha)) if sha.len() == 40 => {
                Ok((GitRef::Commit(sha.clone()), Some(sha), false))
            }
            Some(GitRef::Commit(short)) => {
                // Try to expand via GitHub; otherwise mark as requiring Full.
                if let Some(gh) = &options.github {
                    match gh.resolve_short_sha(owner, repo, &short).await {
                        Ok(full) => Ok((GitRef::Commit(full.clone()), Some(full), false)),
                        Err(GithubError::NotFound { .. }) => {
                            Err(GitError::RefNotFound { reference: short })
                        }
                        Err(_) => Ok((GitRef::Commit(short), None, true)),
                    }
                } else {
                    // Short SHA + no GitHub → must Full-clone and resolve locally.
                    Ok((GitRef::Commit(short), None, true))
                }
            }
            Some(GitRef::Ambiguous(name)) => {
                if let Some(gh) = &options.github {
                    match gh.resolve_ref(owner, repo, &name).await {
                        Ok(resolved) => Ok((resolved, None, false)),
                        Err(GithubError::NotFound { .. }) => {
                            Err(GitError::RefNotFound { reference: name })
                        }
                        Err(_) => {
                            // Fall back to cloning with the raw name; git will
                            // accept both branch and tag names.
                            Ok((GitRef::Branch(name), None, false))
                        }
                    }
                } else {
                    Ok((GitRef::Branch(name), None, false))
                }
            }
            Some(other) => Ok((other, None, false)),
        }
    }

    async fn clone_with_fallback(
        &self,
        owner: &str,
        repo: &str,
        reference: &GitRef,
        strategy: CloneStrategy,
    ) -> Result<(tempfile::TempDir, CloneStrategy), GitError> {
        let url = clone_url(owner, repo);
        let ref_arg = ref_to_clone_arg(reference);

        // First attempt with the requested strategy.
        let temp = tempfile::TempDir::new_in(self.root()).map_err(GitError::Io)?;
        let path = temp.path().to_path_buf();
        let url_clone = url.clone();
        let ref_clone = ref_arg.clone();
        let first = tokio::task::spawn_blocking(move || {
            run_clone(&url_clone, &path, ref_clone.as_deref(), strategy)
        })
        .await
        .map_err(|e| GitError::Other(format!("join: {e}")))?;

        match first {
            Ok(()) => Ok((temp, strategy)),
            // RepoNotFound is terminal — no point retrying with a
            // full clone; the repo simply doesn't exist (or auth
            // didn't let us see it). Surface the error directly.
            Err(e @ GitError::RepoNotFound { .. }) => Err(e),
            Err(e) if strategy == CloneStrategy::Shallow => {
                tracing::warn!(
                    owner,
                    repo,
                    error = %e,
                    "shallow clone failed; retrying with full history",
                );
                // Drop the failed temp and make a fresh one.
                drop(temp);
                let temp2 = tempfile::TempDir::new_in(self.root()).map_err(GitError::Io)?;
                let path2 = temp2.path().to_path_buf();
                let url2 = url.clone();
                let ref2 = ref_arg.clone();
                let second = tokio::task::spawn_blocking(move || {
                    run_clone(&url2, &path2, ref2.as_deref(), CloneStrategy::Full)
                })
                .await
                .map_err(|e| GitError::Other(format!("join: {e}")))?;
                second.map(|()| (temp2, CloneStrategy::Full))
            }
            Err(e) => Err(e),
        }
    }
}

/// Ask the remote for the SHA of a Branch/Tag ref without cloning.
/// Single HTTP round-trip via `git ls-remote`-style ref
/// advertisement. Returns `None` for Commit/Ambiguous refs (where
/// the advertisement doesn't help) or on any error (so the caller
/// falls through to the regular clone path).
async fn ls_remote_sha(url: &str, reference: &GitRef) -> Option<String> {
    let ref_name = match reference {
        GitRef::Branch(b) => format!("refs/heads/{b}"),
        GitRef::Tag(t) => format!("refs/tags/{t}"),
        GitRef::Commit(_) | GitRef::Ambiguous(_) => return None,
    };
    let url = url.to_string();
    tokio::task::spawn_blocking(move || -> Option<String> {
        let mut remote = git2::Remote::create_detached(url.as_str()).ok()?;
        remote.connect(git2::Direction::Fetch).ok()?;
        let list = remote.list().ok()?;
        let head = list.iter().find(|h| h.name() == ref_name)?;
        Some(head.oid().to_string())
    })
    .await
    .ok()
    .flatten()
}

/// Build the clone URL. If `GITHUB_TOKEN` is set, embed it; otherwise plain HTTPS.
fn clone_url(owner: &str, repo: &str) -> String {
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        let token = token.trim();
        if !token.is_empty() {
            return format!("https://{token}@github.com/{owner}/{repo}.git");
        }
    }
    format!("https://github.com/{owner}/{repo}.git")
}

/// What string do we pass as libgit2's `checkout_branch`?
fn ref_to_clone_arg(r: &GitRef) -> Option<String> {
    match r {
        GitRef::Branch(b) => Some(b.clone()),
        GitRef::Tag(t) => Some(t.clone()),
        GitRef::Commit(_) | GitRef::Ambiguous(_) => None,
    }
}

/// Blocking clone via git2. Caller must run this on `spawn_blocking`.
fn run_clone(
    url: &str,
    target: &Path,
    branch: Option<&str>,
    strategy: CloneStrategy,
) -> Result<(), GitError> {
    let mut fetch = Git2FetchOptions::new();
    if strategy == CloneStrategy::Shallow {
        fetch.depth(1);
    }

    let mut builder = RepoBuilder::new();
    builder.fetch_options(fetch);
    if let Some(b) = branch {
        builder.branch(b);
    }

    // Ensure the parent directory exists; git2 errors out if it doesn't.
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }

    builder
        .clone(url, target)
        .map_err(|e| classify_git2_error(url, e))?;
    Ok(())
}

/// Map a libgit2 error into our `GitError` variants, scrubbing any token
/// embedded in the URL.
fn classify_git2_error(url: &str, err: git2::Error) -> GitError {
    let msg = err.message().to_string();
    let lower = msg.to_ascii_lowercase();
    let scrubbed = redact_token(url);
    if lower.contains("authentication") || lower.contains("authorize") {
        GitError::AuthenticationRequired
    } else if lower.contains("not found")
        || lower.contains("repository not found")
        || lower.contains("404")
    {
        // URL shape: https://github.com/<owner>/<repo>.git — extract via regex-lite.
        if let Some((owner, repo)) = owner_repo_from_url(&scrubbed) {
            GitError::RepoNotFound { owner, repo }
        } else {
            GitError::CloneFailed {
                detail: format!("not found: {scrubbed}"),
            }
        }
    } else if lower.contains("reference")
        || lower.contains("unknown revision")
        || lower.contains("no such ref")
    {
        GitError::RefNotFound {
            reference: scrubbed,
        }
    } else {
        GitError::CloneFailed {
            detail: format!("{scrubbed}: {msg}"),
        }
    }
}

fn redact_token(url: &str) -> String {
    // https://<token>@github.com/... → https://***@github.com/...
    if let Some(at) = url.find('@') {
        if let Some(scheme_end) = url.find("://") {
            let head = &url[..scheme_end + 3];
            let tail = &url[at..];
            return format!("{head}***{tail}");
        }
    }
    url.to_string()
}

fn owner_repo_from_url(url: &str) -> Option<(String, String)> {
    let trimmed = url.trim_end_matches(".git");
    let after_host = trimmed.split("github.com/").nth(1)?;
    let mut parts = after_host.split('/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    Some((owner, repo))
}

/// Resolve HEAD of the repo at `path` to its full 40-char commit SHA.
fn read_head_sha(path: &Path) -> Result<String, GitError> {
    let repo = git2::Repository::open(path).map_err(|e| GitError::CloneFailed {
        detail: format!("opening {}: {e}", path.display()),
    })?;
    let head = repo.head().map_err(|e| GitError::CloneFailed {
        detail: format!("reading HEAD: {e}"),
    })?;
    let commit = head.peel_to_commit().map_err(|e| GitError::CloneFailed {
        detail: format!("peeling HEAD: {e}"),
    })?;
    Ok(commit.id().to_string())
}

fn map_github_err(owner: &str, repo: &str, err: GithubError) -> GitError {
    match err {
        GithubError::NotFound { .. } => GitError::RepoNotFound {
            owner: owner.to_string(),
            repo: repo.to_string(),
        },
        GithubError::Unauthorized => GitError::AuthenticationRequired,
        other => GitError::Other(other.to_string()),
    }
}

/// Guard: the `METADATA_FILENAME` constant is referenced here so the module
/// remains consistent with the broader public surface.
#[allow(dead_code)]
const _META_LINK: &str = METADATA_FILENAME;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use basilisk_core::GitRef;
    use tempfile::TempDir;

    use super::*;
    use crate::types::CloneDepth;

    fn ok_meta(cache: &RepoCache, owner: &str, repo: &str, sha: &str) {
        let meta = RepoMetadata {
            owner: owner.into(),
            repo: repo.into(),
            commit_sha: sha.into(),
            original_ref: GitRef::Branch("main".into()),
            clone_depth: CloneDepth::Shallow,
            cloned_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        };
        cache.write_metadata(&meta).unwrap();
        // Create a stand-in working tree so entry_path isn't obviously bogus.
        std::fs::write(cache.entry_path(owner, repo, sha).join("placeholder"), b"x").unwrap();
    }

    #[tokio::test]
    async fn fetch_returns_cached_for_known_full_sha_hit() {
        let tmp = TempDir::new().unwrap();
        let cache = RepoCache::open_at(tmp.path().to_path_buf()).unwrap();
        let sha = "a".repeat(40);
        ok_meta(&cache, "foo", "bar", &sha);
        let fetched = cache
            .fetch(
                "foo",
                "bar",
                Some(GitRef::Commit(sha.clone())),
                FetchOptions::default(),
            )
            .await
            .unwrap();
        assert!(fetched.cached);
        assert_eq!(fetched.commit_sha, sha);
        assert_eq!(fetched.working_tree, cache.entry_path("foo", "bar", &sha));
    }

    #[tokio::test]
    async fn fetch_cache_hit_bypassed_when_force_refresh() {
        let tmp = TempDir::new().unwrap();
        let cache = RepoCache::open_at(tmp.path().to_path_buf()).unwrap();
        let sha = "b".repeat(40);
        ok_meta(&cache, "foo", "bar", &sha);
        // force_refresh skips the cache; with no network this attempts a real
        // clone and fails — we just assert it *didn't* return the cached hit.
        let opts = FetchOptions {
            force_refresh: true,
            ..FetchOptions::default()
        };
        let res = cache
            .fetch("foo", "bar", Some(GitRef::Commit(sha)), opts)
            .await;
        // Either it fails to clone (most sandbox environments) or succeeds
        // with cached: false. We only care that cached != true.
        if let Ok(fetched) = res {
            assert!(!fetched.cached);
        }
    }

    #[tokio::test]
    async fn fetch_with_no_ref_and_no_github_errors() {
        let tmp = TempDir::new().unwrap();
        let cache = RepoCache::open_at(tmp.path().to_path_buf()).unwrap();
        let err = cache
            .fetch("foo", "bar", None, FetchOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, GitError::Other(_)));
    }

    #[test]
    fn ref_to_clone_arg_maps_branch_and_tag() {
        assert_eq!(
            ref_to_clone_arg(&GitRef::Branch("main".into())),
            Some("main".into())
        );
        assert_eq!(
            ref_to_clone_arg(&GitRef::Tag("v1".into())),
            Some("v1".into())
        );
        assert_eq!(ref_to_clone_arg(&GitRef::Commit("abc".into())), None);
        assert_eq!(ref_to_clone_arg(&GitRef::Ambiguous("main".into())), None);
    }

    /// All three `clone_url` cases run in one test to serialise the
    /// `GITHUB_TOKEN` env-var mutations — Rust runs unit tests in
    /// parallel by default, and interleaving set/remove from separate
    /// tests makes these race.
    #[test]
    fn clone_url_honours_github_token_env_var() {
        // Set → token embedded.
        std::env::set_var("GITHUB_TOKEN", "ghp_test_token_XYZ");
        assert!(clone_url("foo", "bar").starts_with("https://ghp_test_token_XYZ@github.com/"));

        // Whitespace-only → ignored, plain HTTPS.
        std::env::set_var("GITHUB_TOKEN", "   ");
        assert_eq!(clone_url("foo", "bar"), "https://github.com/foo/bar.git");

        // Unset → plain HTTPS.
        std::env::remove_var("GITHUB_TOKEN");
        assert_eq!(clone_url("foo", "bar"), "https://github.com/foo/bar.git");
    }

    #[test]
    fn redact_token_scrubs_embedded_secret() {
        let url = "https://ghp_secret@github.com/foo/bar.git";
        assert_eq!(redact_token(url), "https://***@github.com/foo/bar.git");
        // No-op for unauthenticated URLs.
        assert_eq!(
            redact_token("https://github.com/foo/bar.git"),
            "https://github.com/foo/bar.git"
        );
    }

    #[test]
    fn owner_repo_from_url_extracts_path_segments() {
        assert_eq!(
            owner_repo_from_url("https://github.com/foo/bar.git"),
            Some(("foo".into(), "bar".into())),
        );
        assert_eq!(
            owner_repo_from_url("https://github.com/foundry-rs/forge-template.git"),
            Some(("foundry-rs".into(), "forge-template".into())),
        );
        assert!(owner_repo_from_url("https://not-github/foo/bar.git").is_none());
    }

    /// Real-network live clone. Mark ignored so it only runs on demand.
    #[tokio::test]
    #[ignore = "requires network access to github.com"]
    async fn live_clone_forge_template_shallow() {
        let tmp = TempDir::new().unwrap();
        let cache = RepoCache::open_at(tmp.path().to_path_buf()).unwrap();
        let t0 = std::time::Instant::now();
        let first = cache
            .fetch(
                "foundry-rs",
                "forge-template",
                Some(GitRef::Branch("main".into())),
                FetchOptions::default(),
            )
            .await
            .expect("live clone");
        let first_dur = t0.elapsed();
        assert!(!first.cached);
        assert!(first.working_tree.exists());
        assert_eq!(first.commit_sha.len(), 40);

        // Second fetch at the same ref must hit the cache.
        let t1 = std::time::Instant::now();
        let second = cache
            .fetch(
                "foundry-rs",
                "forge-template",
                Some(GitRef::Commit(first.commit_sha.clone())),
                FetchOptions::default(),
            )
            .await
            .unwrap();
        assert!(second.cached);
        assert!(t1.elapsed().as_millis() < 200);
        assert!(first_dur > t1.elapsed());
    }
}
