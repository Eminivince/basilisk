//! In-memory [`VectorStore`] for tests. Not for production.
//!
//! Keeps every collection in a `HashMap<String, HashMap<String, Record>>`
//! and runs cosine similarity with a linear scan. Correctness
//! matters (tests rely on it); speed doesn't (corpora are tiny).
//!
//! Why not just rely on `LanceDbStore` for tests? Two reasons:
//!  1. Cold `LanceDB` connect is fast (< 100ms) but each test still
//!     creates a tempdir and writes files. Thousands of fixture-
//!     based unit tests would add minutes.
//!  2. This layer only wants to exercise the *caller* — the
//!     ingester, the knowledge-base code. LanceDB-specific
//!     behaviour is covered in `lancedb_store`'s own tests.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::SystemTime;

use async_trait::async_trait;

use crate::{
    error::VectorError,
    store::VectorStore,
    types::{
        CollectionInfo, CollectionSpec, CollectionStats, Filter, Metadata, Record, SearchHit,
        SearchQuery, UpsertStats,
    },
};

struct Collection {
    spec: CollectionSpec,
    records: HashMap<String, Record>,
    last_modified: Option<SystemTime>,
}

/// In-memory implementation of [`VectorStore`]. Cheap to clone —
/// inner state is `Arc`-shared under the hood.
pub struct MemoryVectorStore {
    collections: Mutex<HashMap<String, Collection>>,
}

impl MemoryVectorStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            collections: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for MemoryVectorStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VectorStore for MemoryVectorStore {
    async fn create_collection(&self, spec: CollectionSpec) -> Result<(), VectorError> {
        let mut guard = self.lock();
        if let Some(existing) = guard.get(&spec.name) {
            // Idempotent when the spec exactly matches; otherwise
            // we refuse to clobber.
            if existing.spec.embedding_dim == spec.embedding_dim
                && existing.spec.embedding_provider == spec.embedding_provider
            {
                return Ok(());
            }
            return Err(VectorError::IncompatibleSpec {
                name: spec.name,
                stored_provider: existing.spec.embedding_provider.clone(),
                stored_dim: existing.spec.embedding_dim,
                requested_provider: spec.embedding_provider,
                requested_dim: spec.embedding_dim,
            });
        }
        guard.insert(
            spec.name.clone(),
            Collection {
                spec,
                records: HashMap::new(),
                last_modified: None,
            },
        );
        Ok(())
    }

    async fn delete_collection(&self, name: &str) -> Result<(), VectorError> {
        let mut guard = self.lock();
        guard
            .remove(name)
            .ok_or_else(|| VectorError::CollectionNotFound(name.to_string()))?;
        Ok(())
    }

    async fn list_collections(&self) -> Result<Vec<CollectionInfo>, VectorError> {
        let guard = self.lock();
        let mut out: Vec<_> = guard
            .values()
            .map(|c| CollectionInfo {
                name: c.spec.name.clone(),
                record_count: c.records.len(),
                embedding_dim: c.spec.embedding_dim,
                embedding_provider: c.spec.embedding_provider.clone(),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn upsert(
        &self,
        collection: &str,
        records: Vec<Record>,
    ) -> Result<UpsertStats, VectorError> {
        let mut guard = self.lock();
        let coll = guard
            .get_mut(collection)
            .ok_or_else(|| VectorError::CollectionNotFound(collection.to_string()))?;
        let mut stats = UpsertStats::default();
        for r in records {
            if r.vector.len() != coll.spec.embedding_dim {
                return Err(VectorError::DimensionMismatch {
                    collection: collection.to_string(),
                    expected: coll.spec.embedding_dim,
                    actual: r.vector.len(),
                });
            }
            if coll.records.insert(r.id.clone(), r).is_some() {
                stats.updated += 1;
            } else {
                stats.inserted += 1;
            }
        }
        coll.last_modified = Some(SystemTime::now());
        Ok(stats)
    }

    async fn search(
        &self,
        collection: &str,
        query: SearchQuery,
    ) -> Result<Vec<SearchHit>, VectorError> {
        let guard = self.lock();
        let coll = guard
            .get(collection)
            .ok_or_else(|| VectorError::CollectionNotFound(collection.to_string()))?;
        if query.vector.len() != coll.spec.embedding_dim {
            return Err(VectorError::DimensionMismatch {
                collection: collection.to_string(),
                expected: coll.spec.embedding_dim,
                actual: query.vector.len(),
            });
        }

        let mut scored: Vec<(f32, &Record)> = coll
            .records
            .values()
            .filter(|r| matches_filters(&query.filters, &r.metadata))
            .map(|r| (cosine(&query.vector, &r.vector), r))
            .filter(|(score, _)| query.min_score.is_none_or(|m| *score >= m))
            .collect();
        // Higher cosine = more similar.
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(query.limit);

        Ok(scored
            .into_iter()
            .map(|(score, r)| SearchHit {
                id: r.id.clone(),
                score,
                text: if query.include_text {
                    r.text.clone()
                } else {
                    String::new()
                },
                metadata: r.metadata.clone(),
            })
            .collect())
    }

    async fn delete(&self, collection: &str, ids: Vec<String>) -> Result<usize, VectorError> {
        let mut guard = self.lock();
        let coll = guard
            .get_mut(collection)
            .ok_or_else(|| VectorError::CollectionNotFound(collection.to_string()))?;
        let before = coll.records.len();
        for id in &ids {
            coll.records.remove(id);
        }
        let removed = before - coll.records.len();
        if removed > 0 {
            coll.last_modified = Some(SystemTime::now());
        }
        Ok(removed)
    }

    async fn get(&self, collection: &str, id: &str) -> Result<Option<Record>, VectorError> {
        let guard = self.lock();
        let coll = guard
            .get(collection)
            .ok_or_else(|| VectorError::CollectionNotFound(collection.to_string()))?;
        Ok(coll.records.get(id).cloned())
    }

    async fn stats(&self, collection: &str) -> Result<CollectionStats, VectorError> {
        let guard = self.lock();
        let coll = guard
            .get(collection)
            .ok_or_else(|| VectorError::CollectionNotFound(collection.to_string()))?;
        Ok(CollectionStats {
            name: coll.spec.name.clone(),
            record_count: coll.records.len(),
            embedding_dim: coll.spec.embedding_dim,
            embedding_provider: coll.spec.embedding_provider.clone(),
            schema_version: coll.spec.schema_version,
            last_modified: coll.last_modified,
        })
    }
}

impl MemoryVectorStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Collection>> {
        self.collections.lock().expect("memory vector store poisoned")
    }
}

/// Cosine similarity between two equal-length vectors. Undefined
/// for zero vectors; callers are expected to normalise beforehand
/// if that's a concern (embeddings providers always return
/// non-zero vectors in practice).
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

/// Apply all filters to a record's metadata. Returns `true` iff
/// every filter passes (AND semantics).
pub(crate) fn matches_filters(filters: &[Filter], metadata: &Metadata) -> bool {
    filters.iter().all(|f| matches_filter(f, metadata))
}

fn matches_filter(filter: &Filter, metadata: &Metadata) -> bool {
    match filter {
        Filter::Equals { field, value } => get_field(metadata, field) == Some(value.clone()),
        Filter::In { field, values } => get_field(metadata, field)
            .is_some_and(|v| values.contains(&v)),
        Filter::Contains { field, substring } => get_field(metadata, field)
            .and_then(|v| v.as_str().map(std::string::ToString::to_string))
            .is_some_and(|s| s.contains(substring)),
        Filter::TagsAny(tags) => tags.iter().any(|t| metadata.tags.contains(t)),
        Filter::TagsAll(tags) => tags.iter().all(|t| metadata.tags.contains(t)),
        Filter::TimeRange { after, before } => {
            if let Some(after) = after {
                if metadata.indexed_at < *after {
                    return false;
                }
            }
            if let Some(before) = before {
                if metadata.indexed_at >= *before {
                    return false;
                }
            }
            true
        }
    }
}

/// Lookup a metadata field by dotted name. Top-level names
/// (`source`, `kind`, `source_id`, `engagement_id`) resolve to
/// their own fields; everything else is looked up in
/// `metadata.extra` as a JSON path.
fn get_field(metadata: &Metadata, field: &str) -> Option<serde_json::Value> {
    match field {
        "source" => Some(serde_json::Value::String(metadata.source.clone())),
        "source_id" => Some(serde_json::Value::String(metadata.source_id.clone())),
        "kind" => Some(serde_json::Value::String(metadata.kind.clone())),
        "engagement_id" => metadata
            .engagement_id
            .as_ref()
            .map(|s| serde_json::Value::String(s.clone())),
        _ => metadata.extra.pointer(&format!("/{field}")).cloned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;

    fn rec(id: &str, vec: Vec<f32>, source: &str, kind: &str) -> Record {
        Record {
            id: id.to_string(),
            vector: vec,
            text: format!("text for {id}"),
            metadata: Metadata {
                source: source.to_string(),
                source_id: id.to_string(),
                kind: kind.to_string(),
                tags: vec![],
                engagement_id: None,
                extra: serde_json::json!({}),
                indexed_at: SystemTime::UNIX_EPOCH,
            },
        }
    }

    async fn seeded() -> MemoryVectorStore {
        let store = MemoryVectorStore::new();
        store
            .create_collection(schema::user_findings("test/mock", 3))
            .await
            .unwrap();
        store
            .upsert(
                "user_findings",
                vec![
                    rec("a", vec![1.0, 0.0, 0.0], "solodit", "finding"),
                    rec("b", vec![0.0, 1.0, 0.0], "solodit", "finding"),
                    rec("c", vec![0.0, 0.0, 1.0], "code4rena", "finding"),
                ],
            )
            .await
            .unwrap();
        store
    }

    #[tokio::test]
    async fn create_collection_is_idempotent_on_matching_spec() {
        let store = MemoryVectorStore::new();
        let spec = schema::user_findings("test/mock", 4);
        store.create_collection(spec.clone()).await.unwrap();
        store.create_collection(spec).await.unwrap(); // second call: no error
    }

    #[tokio::test]
    async fn create_collection_rejects_mismatched_spec() {
        let store = MemoryVectorStore::new();
        store
            .create_collection(schema::user_findings("voyage/voyage-code-3", 1024))
            .await
            .unwrap();
        let err = store
            .create_collection(schema::user_findings(
                "openai/text-embedding-3-large",
                3072,
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, VectorError::IncompatibleSpec { .. }));
    }

    #[tokio::test]
    async fn upsert_rejects_dimension_mismatch() {
        let store = MemoryVectorStore::new();
        store
            .create_collection(schema::user_findings("test/mock", 3))
            .await
            .unwrap();
        let err = store
            .upsert(
                "user_findings",
                vec![rec("x", vec![1.0, 2.0], "s", "k")],
            )
            .await
            .unwrap_err();
        assert!(matches!(err, VectorError::DimensionMismatch { .. }));
    }

    #[tokio::test]
    async fn search_returns_nearest_first() {
        let store = seeded().await;
        let query = SearchQuery::new(vec![1.0, 0.0, 0.0], 3);
        let hits = store.search("user_findings", query).await.unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].id, "a"); // exact direction match
    }

    #[tokio::test]
    async fn search_applies_source_filter() {
        let store = seeded().await;
        let query = SearchQuery::new(vec![1.0, 1.0, 1.0], 10).with_filter(Filter::Equals {
            field: "source".into(),
            value: serde_json::json!("code4rena"),
        });
        let hits = store.search("user_findings", query).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "c");
    }

    #[tokio::test]
    async fn search_respects_limit() {
        let store = seeded().await;
        let query = SearchQuery::new(vec![1.0, 1.0, 1.0], 2);
        let hits = store.search("user_findings", query).await.unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[tokio::test]
    async fn search_respects_min_score() {
        let store = seeded().await;
        let query = SearchQuery::new(vec![1.0, 0.0, 0.0], 10).with_min_score(0.99);
        let hits = store.search("user_findings", query).await.unwrap();
        // Only `a` has cosine ≈ 1 against [1,0,0]; the others are 0.
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "a");
    }

    #[tokio::test]
    async fn search_on_unknown_collection_errors() {
        let store = MemoryVectorStore::new();
        let err = store
            .search("nope", SearchQuery::new(vec![0.0; 3], 1))
            .await
            .unwrap_err();
        assert!(matches!(err, VectorError::CollectionNotFound(_)));
    }

    #[tokio::test]
    async fn upsert_twice_counts_insert_then_update() {
        let store = seeded().await;
        let again = store
            .upsert(
                "user_findings",
                vec![
                    rec("a", vec![0.0, 1.0, 0.0], "solodit", "finding"),
                    rec("new", vec![0.5, 0.5, 0.5], "solodit", "finding"),
                ],
            )
            .await
            .unwrap();
        assert_eq!(again.inserted, 1);
        assert_eq!(again.updated, 1);
    }

    #[tokio::test]
    async fn delete_returns_exact_removed_count() {
        let store = seeded().await;
        let n = store
            .delete("user_findings", vec!["a".into(), "missing".into()])
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn get_none_for_unknown_id() {
        let store = seeded().await;
        assert!(store.get("user_findings", "missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn stats_reflects_inserts_and_provider() {
        let store = seeded().await;
        let s = store.stats("user_findings").await.unwrap();
        assert_eq!(s.record_count, 3);
        assert_eq!(s.embedding_provider, "test/mock");
    }

    #[tokio::test]
    async fn list_collections_alphabetical() {
        let store = MemoryVectorStore::new();
        for spec in [
            schema::user_findings("m", 1),
            schema::advisories("m", 1),
            schema::protocols("m", 1),
        ] {
            store.create_collection(spec).await.unwrap();
        }
        let names: Vec<_> = store
            .list_collections()
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["advisories", "protocols", "user_findings"]);
    }

    #[tokio::test]
    async fn tags_any_filter_matches_if_any_overlap() {
        let store = MemoryVectorStore::new();
        store
            .create_collection(schema::advisories("m", 2))
            .await
            .unwrap();
        let mut a = rec("a", vec![1.0, 0.0], "s", "k");
        a.metadata.tags = vec!["severity:high".into(), "category:reentrancy".into()];
        let mut b = rec("b", vec![0.0, 1.0], "s", "k");
        b.metadata.tags = vec!["severity:low".into()];
        store.upsert("advisories", vec![a, b]).await.unwrap();

        let hits = store
            .search(
                "advisories",
                SearchQuery::new(vec![1.0, 1.0], 10).with_filter(Filter::TagsAny(vec![
                    "severity:high".into(),
                ])),
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "a");
    }
}
