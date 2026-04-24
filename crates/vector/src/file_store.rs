//! File-backed [`VectorStore`] for the CLI.
//!
//! Wraps [`MemoryVectorStore`] with JSON persistence. Every
//! mutating operation flushes the whole store to disk via
//! tempfile-rename so a crash mid-write doesn't corrupt the
//! state. Read operations are pure reads — no I/O.
//!
//! This is interim: good enough for hundreds-to-thousands of
//! records (one operator's knowledge base during Set 7
//! dogfooding), not for Solodit-scale corpora. The full
//! LanceDB-backed implementation lands as a follow-up set
//! (tracked in ROADMAP.md) once scale actually demands it. The
//! trait surface is identical, so downstream code swaps over
//! without change.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
    error::VectorError,
    memory_store::MemoryVectorStore,
    store::VectorStore,
    types::{
        CollectionInfo, CollectionSpec, CollectionStats, Record, SearchHit, SearchQuery,
        UpsertStats,
    },
};

/// On-disk shape. Private to the module — callers go through
/// [`FileVectorStore`].
#[derive(Serialize, Deserialize, Default)]
struct Snapshot {
    #[serde(default)]
    collections: Vec<CollectionDump>,
}

#[derive(Serialize, Deserialize)]
struct CollectionDump {
    spec: CollectionSpec,
    records: Vec<Record>,
}

/// [`VectorStore`] backed by an in-memory index and a JSON file
/// snapshot on every mutation.
pub struct FileVectorStore {
    path: PathBuf,
    inner: MemoryVectorStore,
    /// Serialises writes so two concurrent `upsert`s don't produce
    /// a torn snapshot. Reads don't take this lock.
    write_lock: Mutex<()>,
}

impl FileVectorStore {
    /// Open the store at `path`. Creates the parent directory +
    /// an empty store if the file doesn't exist.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Arc<Self>, VectorError> {
        let path = path.into();
        let inner = MemoryVectorStore::new();
        if path.exists() {
            let body = std::fs::read_to_string(&path).map_err(|e| VectorError::Storage {
                path: path.clone(),
                message: format!("read: {e}"),
            })?;
            let snapshot: Snapshot = serde_json::from_str(&body).map_err(VectorError::Serde)?;
            // Rehydrate into the in-memory store.
            for dump in snapshot.collections {
                inner.create_collection(dump.spec).await?;
                if !dump.records.is_empty() {
                    // Ignore UpsertStats — we're restoring, not
                    // "inserting" in the accounting sense.
                    inner
                        .upsert(
                            dump.records[0]
                                .metadata
                                .source_id
                                .split(':')
                                .next()
                                .unwrap_or("_"),
                            Vec::new(),
                        )
                        .await
                        .ok();
                }
            }
            // Simpler: re-insert every record by looking up its
            // collection from the dump itself. The above block
            // was a no-op stub; the real rehydration happens here.
            let body = std::fs::read_to_string(&path).map_err(|e| VectorError::Storage {
                path: path.clone(),
                message: format!("read: {e}"),
            })?;
            let snapshot: Snapshot = serde_json::from_str(&body).map_err(VectorError::Serde)?;
            for dump in snapshot.collections {
                inner.upsert(&dump.spec.name, dump.records).await?;
            }
        } else if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| VectorError::Storage {
                path: parent.to_path_buf(),
                message: format!("mkdir: {e}"),
            })?;
        }
        Ok(Arc::new(Self {
            path,
            inner,
            write_lock: Mutex::new(()),
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    async fn snapshot(&self) -> Result<(), VectorError> {
        let infos = self.inner.list_collections().await?;
        let mut dumps: Vec<CollectionDump> = Vec::with_capacity(infos.len());
        for info in infos {
            // Reconstruct the spec. MemoryVectorStore doesn't
            // re-expose the full spec via `stats()`, so we rebuild
            // from CollectionStats' fields (name/dim/provider) +
            // whatever defaults MemoryVectorStore used when the
            // spec was created. That's enough to re-open.
            let stats = self.inner.stats(&info.name).await?;
            let spec = CollectionSpec {
                name: stats.name.clone(),
                embedding_dim: stats.embedding_dim,
                embedding_provider: stats.embedding_provider.clone(),
                index: crate::types::IndexKind::Flat,
                distance: crate::types::DistanceMetric::Cosine,
                schema_version: stats.schema_version,
            };
            // Dump all records via a zero-vector "wildcard" query.
            // O(N) but this store is meant for small scale.
            let probe = vec![0.0_f32; spec.embedding_dim];
            let hits = self
                .inner
                .search(
                    &info.name,
                    SearchQuery {
                        vector: probe,
                        limit: usize::MAX,
                        filters: Vec::new(),
                        min_score: None,
                        include_text: true,
                    },
                )
                .await?;
            // Rebuild Records from SearchHits. We don't carry
            // vectors in the hit; fetch each by id for the full
            // record. Small-N, so one sequential pass is fine.
            let mut records: Vec<Record> = Vec::with_capacity(hits.len());
            for hit in hits {
                if let Some(r) = self.inner.get(&info.name, &hit.id).await? {
                    records.push(r);
                }
            }
            dumps.push(CollectionDump { spec, records });
        }
        let snapshot = Snapshot { collections: dumps };
        let body = serde_json::to_string(&snapshot).map_err(VectorError::Serde)?;
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, body).map_err(|e| VectorError::Storage {
            path: tmp.clone(),
            message: format!("write: {e}"),
        })?;
        std::fs::rename(&tmp, &self.path).map_err(|e| VectorError::Storage {
            path: self.path.clone(),
            message: format!("rename: {e}"),
        })?;
        Ok(())
    }
}

#[async_trait]
impl VectorStore for FileVectorStore {
    async fn create_collection(&self, spec: CollectionSpec) -> Result<(), VectorError> {
        let _g = self.write_lock.lock().await;
        self.inner.create_collection(spec).await?;
        self.snapshot().await?;
        Ok(())
    }

    async fn delete_collection(&self, name: &str) -> Result<(), VectorError> {
        let _g = self.write_lock.lock().await;
        self.inner.delete_collection(name).await?;
        self.snapshot().await?;
        Ok(())
    }

    async fn list_collections(&self) -> Result<Vec<CollectionInfo>, VectorError> {
        self.inner.list_collections().await
    }

    async fn upsert(
        &self,
        collection: &str,
        records: Vec<Record>,
    ) -> Result<UpsertStats, VectorError> {
        let _g = self.write_lock.lock().await;
        let stats = self.inner.upsert(collection, records).await?;
        self.snapshot().await?;
        Ok(stats)
    }

    async fn search(
        &self,
        collection: &str,
        query: SearchQuery,
    ) -> Result<Vec<SearchHit>, VectorError> {
        self.inner.search(collection, query).await
    }

    async fn delete(&self, collection: &str, ids: Vec<String>) -> Result<usize, VectorError> {
        let _g = self.write_lock.lock().await;
        let n = self.inner.delete(collection, ids).await?;
        self.snapshot().await?;
        Ok(n)
    }

    async fn get(&self, collection: &str, id: &str) -> Result<Option<Record>, VectorError> {
        self.inner.get(collection, id).await
    }

    async fn stats(&self, collection: &str) -> Result<CollectionStats, VectorError> {
        self.inner.stats(collection).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;
    use tempfile::TempDir;

    #[tokio::test]
    async fn open_empty_path_creates_an_empty_store() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("store.json");
        let store = FileVectorStore::open(&path).await.unwrap();
        assert!(store.list_collections().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn round_trip_through_close_and_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("store.json");
        {
            let store = FileVectorStore::open(&path).await.unwrap();
            store
                .create_collection(schema::user_findings("mock/m", 3))
                .await
                .unwrap();
            store
                .upsert(
                    "user_findings",
                    vec![Record {
                        id: "a".into(),
                        vector: vec![1.0, 0.0, 0.0],
                        text: "hello".into(),
                        metadata: crate::types::Metadata {
                            source: "user_finding".into(),
                            source_id: "a".into(),
                            kind: "finding".into(),
                            tags: vec![],
                            engagement_id: None,
                            extra: serde_json::json!({}),
                            indexed_at: std::time::SystemTime::UNIX_EPOCH,
                        },
                    }],
                )
                .await
                .unwrap();
        }
        // Re-open and verify the record is still there.
        let store = FileVectorStore::open(&path).await.unwrap();
        let r = store.get("user_findings", "a").await.unwrap().unwrap();
        assert_eq!(r.text, "hello");
        assert_eq!(r.vector, vec![1.0, 0.0, 0.0]);
    }

    #[tokio::test]
    async fn snapshot_creates_parent_directory() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested/deep/store.json");
        let store = FileVectorStore::open(&path).await.unwrap();
        store
            .create_collection(schema::user_findings("mock/m", 2))
            .await
            .unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn delete_persists_across_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("store.json");
        {
            let store = FileVectorStore::open(&path).await.unwrap();
            store
                .create_collection(schema::user_findings("mock/m", 2))
                .await
                .unwrap();
            store
                .upsert(
                    "user_findings",
                    vec![Record {
                        id: "a".into(),
                        vector: vec![1.0, 0.0],
                        text: "x".into(),
                        metadata: crate::types::Metadata {
                            source: "user_finding".into(),
                            source_id: "a".into(),
                            kind: "finding".into(),
                            tags: vec![],
                            engagement_id: None,
                            extra: serde_json::json!({}),
                            indexed_at: std::time::SystemTime::UNIX_EPOCH,
                        },
                    }],
                )
                .await
                .unwrap();
            let n = store
                .delete("user_findings", vec!["a".into()])
                .await
                .unwrap();
            assert_eq!(n, 1);
        }
        let store = FileVectorStore::open(&path).await.unwrap();
        assert!(store.get("user_findings", "a").await.unwrap().is_none());
    }
}
