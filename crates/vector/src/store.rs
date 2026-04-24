//! The [`VectorStore`] trait.
//!
//! One interface; concrete backends slot in below. `LanceDB` is the
//! shipped implementation; an in-memory mock for tests lives
//! alongside.
//!
//! Async methods so backends that talk to disk or the network fit
//! naturally. Even the in-memory mock is async-shaped — callers
//! don't branch on impl type.

use async_trait::async_trait;

use crate::{
    error::VectorError,
    types::{
        CollectionInfo, CollectionSpec, CollectionStats, Record, SearchHit, SearchQuery,
        UpsertStats,
    },
};

/// A vector database.
///
/// Collections are independent — schemas, dimensions, and indices
/// don't have to match. `create_collection` is idempotent when the
/// spec exactly matches; different embedding dims or providers on
/// an existing collection are an error (callers should `reembed`
/// explicitly instead of blowing the store up).
#[async_trait]
pub trait VectorStore: Send + Sync {
    async fn create_collection(&self, spec: CollectionSpec) -> Result<(), VectorError>;

    async fn delete_collection(&self, name: &str) -> Result<(), VectorError>;

    async fn list_collections(&self) -> Result<Vec<CollectionInfo>, VectorError>;

    /// Upsert (insert or replace) records by id. Returns a count
    /// split of new vs. updated rows.
    async fn upsert(
        &self,
        collection: &str,
        records: Vec<Record>,
    ) -> Result<UpsertStats, VectorError>;

    /// Nearest-neighbour search. Filters are applied via the backend
    /// where possible (`LanceDB`'s pushdown) and in Rust otherwise.
    async fn search(
        &self,
        collection: &str,
        query: SearchQuery,
    ) -> Result<Vec<SearchHit>, VectorError>;

    /// Delete records by id. Returns the count actually removed.
    async fn delete(&self, collection: &str, ids: Vec<String>) -> Result<usize, VectorError>;

    /// Fetch a single record by id. `Ok(None)` when the id doesn't
    /// exist — not an error.
    async fn get(&self, collection: &str, id: &str) -> Result<Option<Record>, VectorError>;

    async fn stats(&self, collection: &str) -> Result<CollectionStats, VectorError>;
}
