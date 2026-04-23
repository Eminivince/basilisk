//! Git clone + persistent repo cache for Basilisk.
//!
//! Layered by responsibility:
//!   * [`types`] — public data types ([`FetchOptions`], [`FetchedRepo`],
//!     [`RepoMetadata`], …) and the on-disk metadata format.
//!   * [`error`] — the [`GitError`] surface.
//!   * [`cache`] — pure-filesystem cache ([`RepoCache`], metadata I/O,
//!     `list` / `stats` / `clear`). No network, no libgit2.
//!   * [`fetch`] — the git2-backed [`RepoCache::fetch`] entry point
//!     (shallow-first clone, content-addressed by HEAD commit SHA,
//!     [`basilisk_github`] used for ref/short-SHA resolution).
//!
//! Typical use:
//! ```no_run
//! # async fn demo() -> Result<(), basilisk_git::GitError> {
//! use basilisk_core::GitRef;
//! use basilisk_git::{FetchOptions, RepoCache};
//!
//! let cache = RepoCache::open()?;
//! let fetched = cache
//!     .fetch(
//!         "foundry-rs",
//!         "forge-template",
//!         Some(GitRef::Branch("main".into())),
//!         FetchOptions::default(),
//!     )
//!     .await?;
//! println!("{} @ {}", fetched.working_tree.display(), fetched.commit_sha);
//! # Ok(()) }
//! ```

pub mod cache;
pub mod error;
pub mod fetch;
pub(crate) mod time_serde;
pub mod types;

pub use cache::{default_cache_root, RepoCache, DEFAULT_CACHE_SUBDIR};
pub use error::GitError;
pub use types::{
    CloneDepth, CloneStrategy, FetchDuration, FetchOptions, FetchedRepo, RepoCacheStats,
    RepoMetadata, METADATA_FILENAME,
};
