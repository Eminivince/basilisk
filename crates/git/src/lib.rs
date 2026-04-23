//! Git clone + persistent repo cache for Basilisk.
//!
//! Checkpoint 3a: public types + error surface. No cache or clone logic
//! yet — later CP3 sub-commits fill those in. Every type here is
//! deliberately small and serializable so the cache layer (`CP3b`) and
//! clone logic (`CP3c`) can slot in without touching the public surface.

pub mod cache;
pub mod error;
pub(crate) mod time_serde;
pub mod types;

pub use cache::{default_cache_root, RepoCache, DEFAULT_CACHE_SUBDIR};
pub use error::GitError;
pub use types::{
    CloneDepth, CloneStrategy, FetchOptions, FetchedRepo, RepoCacheStats, RepoMetadata,
    METADATA_FILENAME,
};
