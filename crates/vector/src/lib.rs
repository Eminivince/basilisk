//! Vector database abstraction for Basilisk.
//!
//! One interface ([`VectorStore`]); concrete backends slot in
//! beneath. CP7.3 (this commit) ships:
//!
//!  - the trait,
//!  - [`types`] vocabulary (record, metadata, query, filter),
//!  - [`schema`] — the five canonical collection specs,
//!  - [`memory_store::MemoryVectorStore`] — in-memory
//!    implementation for unit tests and offline smoke-checks.
//!
//! CP7.3b adds `LanceDbStore` — the persistent, production
//! implementation on top of `LanceDB` / Apache Arrow / Parquet. The
//! trait is identical; downstream code written against
//! `dyn VectorStore` doesn't branch.
//!
//! Storage root for the production backend:
//! `~/.basilisk/knowledge/`. Mirrors the `~/.basilisk/repos/`
//! layout pattern established in earlier sets.

pub mod error;
pub mod file_store;
pub mod memory_store;
pub mod schema;
pub mod store;
pub mod types;

pub use error::VectorError;
pub use file_store::FileVectorStore;
pub use memory_store::MemoryVectorStore;
pub use store::VectorStore;
pub use types::{
    CollectionInfo, CollectionSpec, CollectionStats, DistanceMetric, Filter, IndexKind, Metadata,
    Record, SearchHit, SearchQuery, UpsertStats,
};
