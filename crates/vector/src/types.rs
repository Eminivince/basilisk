//! Shared vocabulary for [`VectorStore`]. Provider-neutral: the
//! same types appear in every implementation (`LanceDbStore`
//! today, possibly `SqliteVecStore` or others later).
//!
//! [`VectorStore`]: crate::store::VectorStore

use serde::{Deserialize, Serialize};
use std::time::SystemTime;

/// Declarative description of a collection. Passed to
/// [`VectorStore::create_collection`].
///
/// Schema version and embedding dimension are stored as
/// collection-level metadata so a subsequent `open()` can detect
/// drift (embedding provider swap → dimension mismatch) and
/// surface the `reembed` migration path instead of silently
/// corrupting queries.
///
/// [`VectorStore::create_collection`]: crate::store::VectorStore::create_collection
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionSpec {
    pub name: String,
    pub embedding_dim: usize,
    /// Identifier of the embedding provider + model that produced
    /// vectors in this collection, e.g. `"voyage/voyage-code-3"`.
    /// Stamped on create; compared on open to detect drift.
    pub embedding_provider: String,
    pub index: IndexKind,
    pub distance: DistanceMetric,
    /// Bumped when the in-code schema evolves. Open returns an
    /// error if the stored version is unknown to this build.
    pub schema_version: u32,
}

/// Index type. Defaults to `Flat` (exact, slow-but-correct).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexKind {
    /// Exact brute-force search. Correct; fine for small collections.
    #[default]
    Flat,
    /// HNSW graph — low-latency, approximate. Best for moderate
    /// (<1M) collections.
    Hnsw,
    /// IVF-PQ. Best for large corpora (Solodit-scale 100k+).
    /// `LanceDB` takes care of training the quantiser.
    IvfPq,
}

/// Vector-similarity metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DistanceMetric {
    /// Cosine similarity. Default for embeddings — dense vectors'
    /// magnitudes carry little semantic signal.
    #[default]
    Cosine,
    /// Squared L2 distance.
    L2,
    /// Dot product (inner product).
    Dot,
}

/// One row in a collection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// Deterministic id — typically `sha256(source + source_id + chunk_idx)`.
    pub id: String,
    pub vector: Vec<f32>,
    pub text: String,
    pub metadata: Metadata,
}

/// Structured metadata carried with every record.
///
/// The "rich-filter" subset (`source`, `kind`, `engagement_id`)
/// is promoted to its own fields for efficient filtering; the
/// long tail lives in `extra` as an arbitrary JSON value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    pub source: String,
    pub source_id: String,
    pub kind: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Engagement-scoped records (protocol docs) set this so
    /// retrieval can filter to the active engagement.
    #[serde(default)]
    pub engagement_id: Option<String>,
    /// Arbitrary source-specific fields. The common filterable
    /// ones (severity, category) live here under conventional
    /// keys; callers query via `Filter::JsonEquals`.
    #[serde(default)]
    pub extra: serde_json::Value,
    #[serde(default = "default_now", with = "system_time_serde")]
    pub indexed_at: SystemTime,
}

fn default_now() -> SystemTime {
    SystemTime::now()
}

mod system_time_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    pub fn serialize<S: Serializer>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        let secs = t
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        secs.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SystemTime, D::Error> {
        let secs = u64::deserialize(d)?;
        Ok(UNIX_EPOCH + Duration::from_secs(secs))
    }
}

/// Query input for similarity search.
#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub vector: Vec<f32>,
    pub limit: usize,
    #[allow(clippy::struct_field_names)]
    pub filters: Vec<Filter>,
    /// Score floor below which results are dropped. `None` = no floor.
    pub min_score: Option<f32>,
    /// Whether to populate `SearchHit::text`. Skipping saves bytes
    /// on large-payload collections when the caller only wants ids.
    pub include_text: bool,
}

impl SearchQuery {
    #[must_use]
    pub fn new(vector: Vec<f32>, limit: usize) -> Self {
        Self {
            vector,
            limit,
            filters: Vec::new(),
            min_score: None,
            include_text: true,
        }
    }

    #[must_use]
    pub fn with_filter(mut self, filter: Filter) -> Self {
        self.filters.push(filter);
        self
    }

    #[must_use]
    pub fn with_min_score(mut self, score: f32) -> Self {
        self.min_score = Some(score);
        self
    }
}

/// Pre-filter conditions applied before (or alongside) similarity.
///
/// `LanceDB`'s filter pushdown handles the simpler variants (`Equals`,
/// `In`); the others we evaluate in Rust after fetching candidates.
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    /// `metadata.<field> == value`. `field` uses dotted-path syntax
    /// within the `extra` JSON object (e.g. `"severity"` matches
    /// `metadata.extra.severity`); top-level fields use bare names
    /// (e.g. `"source"`).
    Equals {
        field: String,
        value: serde_json::Value,
    },
    /// `metadata.<field>` in `values`.
    In {
        field: String,
        values: Vec<serde_json::Value>,
    },
    /// Substring match on a string field.
    Contains { field: String, substring: String },
    /// Any tag in the list is present on the record.
    TagsAny(Vec<String>),
    /// All tags in the list are present on the record.
    TagsAll(Vec<String>),
    /// `indexed_at` within `[after, before)`.
    TimeRange {
        after: Option<SystemTime>,
        before: Option<SystemTime>,
    },
}

/// One search result row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub id: String,
    /// Distance-adjusted similarity. Higher is better for `Cosine`
    /// and `Dot`; lower for `L2` (we invert L2 internally so a
    /// single "higher is better" convention holds at this layer).
    pub score: f32,
    /// Populated when the query had `include_text = true`.
    pub text: String,
    pub metadata: Metadata,
}

/// Stats reported by [`VectorStore::upsert`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpsertStats {
    pub inserted: usize,
    pub updated: usize,
}

/// Stats reported by [`VectorStore::stats`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CollectionStats {
    pub name: String,
    pub record_count: usize,
    pub embedding_dim: usize,
    pub embedding_provider: String,
    pub schema_version: u32,
    /// When the collection was last modified (write or delete).
    /// `None` for read-only or empty collections.
    #[serde(default, with = "option_system_time_serde")]
    pub last_modified: Option<SystemTime>,
}

mod option_system_time_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    // serde demands `&Option<T>` on the serializer signature; clippy
    // prefers `Option<&T>` as idiomatic API but it's not our API.
    #[allow(clippy::ref_option)]
    pub fn serialize<S: Serializer>(t: &Option<SystemTime>, s: S) -> Result<S::Ok, S::Error> {
        let secs = t
            .as_ref()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs()));
        secs.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<SystemTime>, D::Error> {
        let opt = Option::<u64>::deserialize(d)?;
        Ok(opt.map(|secs| UNIX_EPOCH + Duration::from_secs(secs)))
    }
}

/// Minimal summary row for [`VectorStore::list_collections`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CollectionInfo {
    pub name: String,
    pub record_count: usize,
    pub embedding_dim: usize,
    pub embedding_provider: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_kind_defaults_to_flat() {
        assert_eq!(IndexKind::default(), IndexKind::Flat);
    }

    #[test]
    fn distance_defaults_to_cosine() {
        assert_eq!(DistanceMetric::default(), DistanceMetric::Cosine);
    }

    #[test]
    fn search_query_builder_threads_filters_and_score() {
        let q = SearchQuery::new(vec![0.0; 4], 10)
            .with_filter(Filter::Equals {
                field: "source".into(),
                value: serde_json::json!("solodit"),
            })
            .with_min_score(0.2);
        assert_eq!(q.filters.len(), 1);
        assert_eq!(q.min_score, Some(0.2));
        assert!(q.include_text);
    }

    #[test]
    fn metadata_round_trips_through_json() {
        let m = Metadata {
            source: "solodit".into(),
            source_id: "sol-1".into(),
            kind: "finding".into(),
            tags: vec!["severity:high".into()],
            engagement_id: None,
            extra: serde_json::json!({ "project": "aave" }),
            indexed_at: SystemTime::UNIX_EPOCH,
        };
        let j = serde_json::to_string(&m).unwrap();
        let back: Metadata = serde_json::from_str(&j).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn filter_variants_are_distinct_under_partial_eq() {
        let a = Filter::Equals {
            field: "source".into(),
            value: serde_json::json!("solodit"),
        };
        let b = Filter::In {
            field: "source".into(),
            values: vec![serde_json::json!("solodit")],
        };
        assert_ne!(a, b);
    }
}
