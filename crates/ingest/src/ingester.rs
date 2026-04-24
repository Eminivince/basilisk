//! The [`Ingester`] trait + option/report types.
//!
//! Each concrete ingester implements this once. Options and reports
//! are shape-uniform across sources so the CLI's `ingest` command
//! can loop over multiple ingesters with one codepath.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use basilisk_embeddings::EmbeddingProvider;
use basilisk_vector::VectorStore;
use serde::{Deserialize, Serialize};

use crate::error::IngestError;

/// An ingestion pipeline for one external corpus (Solodit,
/// Code4rena, SWC, …) or one protocol-context source type (URL,
/// PDF, file, GitHub dir).
#[async_trait]
pub trait Ingester: Send + Sync {
    /// Stable identifier — used in logs and state files.
    fn source_name(&self) -> &str;

    /// Which [`basilisk_vector`] collection this ingester writes to
    /// (`public_findings`, `advisories`, `post_mortems`,
    /// `protocols`). `user_findings` is the agent's write path and
    /// never an ingest target.
    fn target_collection(&self) -> &str;

    async fn ingest(
        &self,
        vector_store: Arc<dyn VectorStore>,
        embeddings: Arc<dyn EmbeddingProvider>,
        options: IngestOptions,
    ) -> Result<IngestReport, IngestError>;
}

/// Categorise ingesters for CLI dispatch.
///
/// External corpora and protocol context diverge enough
/// (scheduling, credentials, TOS) that the CLI branches on the
/// kind at the dispatch layer. `External` ingesters run by source
/// name; `Protocol` ingesters require an engagement id + source
/// pointer (`--url`, `--pdf`, `--file`, `--github`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngesterKind {
    External,
    Protocol,
}

/// Runtime options passed into every [`Ingester::ingest`] call.
#[derive(Clone)]
pub struct IngestOptions {
    /// When `true` (default), only records newer than the last
    /// successful run are processed. When `false`, every record is
    /// re-ingested and the state file's cursor is reset.
    pub incremental: bool,
    /// Hard cap on records ingested in this run. Used for testing
    /// or partial dumps. `None` = unlimited.
    pub max_records: Option<usize>,
    /// How many source-read futures may run in parallel. Defaults to
    /// 4, respecting typical GitHub rate limits and polite-scraping
    /// conventions.
    pub concurrency: usize,
    /// Optional progress callback. Fired after every batch upsert.
    pub progress: Option<Arc<dyn Fn(IngestProgress) + Send + Sync>>,
}

impl std::fmt::Debug for IngestOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IngestOptions")
            .field("incremental", &self.incremental)
            .field("max_records", &self.max_records)
            .field("concurrency", &self.concurrency)
            .field("progress", &self.progress.as_ref().map(|_| "<callback>"))
            .finish()
    }
}

impl Default for IngestOptions {
    fn default() -> Self {
        Self {
            incremental: true,
            max_records: None,
            concurrency: 4,
            progress: None,
        }
    }
}

impl IngestOptions {
    /// Builder: override `max_records`.
    #[must_use]
    pub fn with_max_records(mut self, n: usize) -> Self {
        self.max_records = Some(n);
        self
    }

    /// Builder: force full re-ingest.
    #[must_use]
    pub fn non_incremental(mut self) -> Self {
        self.incremental = false;
        self
    }
}

/// Partial-progress snapshot. Fired by the ingester at a
/// granularity of its choosing (typically one event per batch).
#[derive(Debug, Clone, Copy, Default)]
pub struct IngestProgress {
    pub records_scanned: usize,
    pub records_upserted: usize,
    pub records_skipped: usize,
    pub embedding_tokens_used: u64,
}

/// Final report returned when [`Ingester::ingest`] completes
/// successfully.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestReport {
    pub source: String,
    pub records_scanned: usize,
    pub records_new: usize,
    pub records_updated: usize,
    pub records_skipped: usize,
    pub embedding_tokens_used: u64,
    #[serde(with = "duration_ms")]
    pub duration: Duration,
    /// Per-record errors that didn't halt the run. Format:
    /// `(record_id, human-readable reason)`. Surfaced so operators
    /// can see what was dropped without scanning logs.
    pub errors: Vec<(String, String)>,
}

impl IngestReport {
    /// Convenience: zero everything, set source + duration. Ingesters
    /// accumulate state into an `IngestReport` as they go.
    #[must_use]
    pub fn empty(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            records_scanned: 0,
            records_new: 0,
            records_updated: 0,
            records_skipped: 0,
            embedding_tokens_used: 0,
            duration: Duration::ZERO,
            errors: Vec::new(),
        }
    }
}

/// Source-agnostic intermediate record. Every ingester normalises
/// its native shape into this before chunking + embedding.
///
/// Chunking: if `body` exceeds the embedding model's per-input
/// token limit, [`crate::normalize::chunk_record`] splits along
/// semantic boundaries, linking the chunks via `parent_id` and
/// `chunk_index`/`total_chunks` in the resulting metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngestRecord {
    /// Source-stable id. Combined with the source name to produce
    /// the final record id via sha256, so different sources with
    /// colliding ids stay distinct.
    pub source_id: String,
    pub source: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub extra: serde_json::Value,
}

mod duration_ms {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u128(d.as_millis())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let ms = u64::deserialize(d)?;
        Ok(Duration::from_millis(ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_defaults_are_sensible() {
        let o = IngestOptions::default();
        assert!(o.incremental);
        assert_eq!(o.concurrency, 4);
        assert!(o.max_records.is_none());
    }

    #[test]
    fn options_builders_update_fields() {
        let o = IngestOptions::default()
            .with_max_records(5)
            .non_incremental();
        assert_eq!(o.max_records, Some(5));
        assert!(!o.incremental);
    }

    #[test]
    fn empty_report_zeroes_counters_and_preserves_source() {
        let r = IngestReport::empty("solodit");
        assert_eq!(r.source, "solodit");
        assert_eq!(r.records_scanned, 0);
        assert!(r.errors.is_empty());
    }

    #[test]
    fn report_round_trips_through_json_including_duration() {
        let mut r = IngestReport::empty("swc");
        r.duration = Duration::from_millis(2500);
        r.records_new = 42;
        r.errors.push(("SWC-131".into(), "parse failed".into()));
        let j = serde_json::to_string(&r).unwrap();
        let back: IngestReport = serde_json::from_str(&j).unwrap();
        assert_eq!(r.source, back.source);
        assert_eq!(r.duration, back.duration);
        assert_eq!(r.errors, back.errors);
    }
}
