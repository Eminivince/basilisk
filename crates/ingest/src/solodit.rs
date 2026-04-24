//! Solodit ingester — reads a local JSONL dump.
//!
//! Solodit has no public API and aggressive Cloudflare, so the
//! primary path is a **user-supplied JSONL file** placed at
//! `~/.basilisk/knowledge/solodit_dump.jsonl` (override via
//! constructor). One finding per line; expected fields listed
//! in [`SoloditFindingRow`] below.
//!
//! Live scraping is intentionally deferred — handling Cloudflare
//! requires a headless browser or a third-party service, neither
//! of which earns its complexity in Set 7. If/when Solodit
//! exposes a public API, a [`Ingester`] impl can be added under
//! a `--scrape` flag without changing the ingester trait.
//!
//! [`Ingester`]: crate::ingester::Ingester

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use basilisk_embeddings::{EmbeddingInput, EmbeddingProvider};
use basilisk_vector::{VectorStore, schema};
use serde::{Deserialize, Serialize};

use crate::{
    error::IngestError,
    ingester::{IngestOptions, IngestProgress, IngestRecord, IngestReport, Ingester},
    normalize::chunk_record,
    state::{IngestState, SourceState},
};

/// One row from the Solodit JSONL dump.
///
/// Field names mirror Solodit's internal schema; the dump format
/// is what the community typically exports via scrape-and-share
/// workflows. Extra fields we don't recognise are ignored.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SoloditFindingRow {
    /// Solodit's internal id. Used as `source_id` in our records.
    pub id: String,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub auditor: Option<String>,
    #[serde(default)]
    pub report_url: Option<String>,
    #[serde(default)]
    pub finding_url: Option<String>,
    /// ISO-8601 date (`YYYY-MM-DD`). We keep it as a string
    /// in metadata so downstream can filter alphabetically
    /// (ISO-8601 sort order == chronological).
    #[serde(default)]
    pub date: Option<String>,
}

impl SoloditFindingRow {
    /// Convert to the source-neutral [`IngestRecord`] shape.
    #[must_use]
    pub fn into_ingest_record(self) -> IngestRecord {
        let mut tags: Vec<String> = Vec::new();
        if let Some(s) = &self.severity {
            tags.push(format!("severity:{}", s.to_ascii_lowercase()));
        }
        if let Some(c) = &self.category {
            tags.push(format!("category:{}", c.to_ascii_lowercase()));
        }
        if let Some(a) = &self.auditor {
            tags.push(format!("auditor:{}", a.to_ascii_lowercase()));
        }

        let mut extra = serde_json::Map::new();
        if let Some(v) = self.severity {
            extra.insert("severity".into(), serde_json::Value::String(v));
        }
        if let Some(v) = self.category {
            extra.insert("category".into(), serde_json::Value::String(v));
        }
        if let Some(v) = self.project {
            extra.insert("project".into(), serde_json::Value::String(v));
        }
        if let Some(v) = self.auditor {
            extra.insert("auditor".into(), serde_json::Value::String(v));
        }
        if let Some(v) = self.report_url {
            extra.insert("report_url".into(), serde_json::Value::String(v));
        }
        if let Some(v) = self.finding_url {
            extra.insert("finding_url".into(), serde_json::Value::String(v));
        }
        if let Some(v) = self.date {
            extra.insert("date".into(), serde_json::Value::String(v));
        }

        IngestRecord {
            source: "solodit".into(),
            source_id: self.id,
            kind: "finding".into(),
            title: self.title,
            body: self.body,
            tags,
            extra: serde_json::Value::Object(extra),
        }
    }
}

/// The Solodit [`Ingester`].
///
/// Reads one JSONL file; one line per finding. Malformed lines
/// land in [`IngestReport::errors`] and don't halt the run.
pub struct SoloditIngester {
    dump_path: PathBuf,
}

impl SoloditIngester {
    /// Default path: `~/.basilisk/knowledge/solodit_dump.jsonl`,
    /// with a working-dir fallback when no home is discoverable.
    #[must_use]
    pub fn new() -> Self {
        Self {
            dump_path: default_dump_path(),
        }
    }

    /// Point at a non-default JSONL path (used by tests + operators
    /// who store the dump elsewhere).
    #[must_use]
    pub fn at_path(path: impl Into<PathBuf>) -> Self {
        Self {
            dump_path: path.into(),
        }
    }

    /// Path the ingester will read from. Exposed so the CLI can
    /// report it cleanly in "where's my dump?" diagnostics.
    #[must_use]
    pub fn dump_path(&self) -> &Path {
        &self.dump_path
    }
}

impl Default for SoloditIngester {
    fn default() -> Self {
        Self::new()
    }
}

/// `~/.basilisk/knowledge/solodit_dump.jsonl` when a home
/// directory is discoverable; otherwise a working-dir fallback.
#[must_use]
pub fn default_dump_path() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        return home
            .join(".basilisk")
            .join("knowledge")
            .join("solodit_dump.jsonl");
    }
    PathBuf::from(".basilisk")
        .join("knowledge")
        .join("solodit_dump.jsonl")
}

#[async_trait]
impl Ingester for SoloditIngester {
    #[allow(clippy::unnecessary_literal_bound)]
    fn source_name(&self) -> &str {
        "solodit"
    }

    fn target_collection(&self) -> &str {
        schema::PUBLIC_FINDINGS
    }

    async fn ingest(
        &self,
        vector_store: Arc<dyn VectorStore>,
        embeddings: Arc<dyn EmbeddingProvider>,
        options: IngestOptions,
    ) -> Result<IngestReport, IngestError> {
        let start = Instant::now();
        let mut report = IngestReport::empty(self.source_name());

        // Load prior state so incremental runs skip records already
        // persisted. The cursor is the last successfully-ingested
        // id — we keep every line > cursor (string > on ids is the
        // ingester's convention; Solodit ids sort lexicographically).
        let state_file = crate::state::default_state_path();
        let mut persistent = IngestState::load(&state_file)?;
        let prior = persistent.get(self.source_name());
        let cursor = if options.incremental { prior.cursor.clone() } else { None };

        if !self.dump_path.exists() {
            return Err(IngestError::Source(format!(
                "solodit dump not found at {}. Place a JSONL file there, or use \
                 SoloditIngester::at_path() to point elsewhere.",
                self.dump_path.display(),
            )));
        }

        // Parse the JSONL into IngestRecords. Malformed lines go
        // into report.errors and don't halt the run.
        let rows = read_rows(&self.dump_path, cursor.as_deref(), options.max_records, &mut report)?;
        if rows.is_empty() {
            report.duration = start.elapsed();
            return Ok(report);
        }
        report.records_scanned = rows.len();

        // Ensure the target collection exists with matching
        // provider + dim. A dim mismatch here is the "operator
        // swapped embeddings provider without reembedding" case —
        // we surface it as a typed error rather than silently
        // corrupting vectors.
        let spec = schema::public_findings(embeddings.identifier(), embeddings.dimensions());
        vector_store.create_collection(spec).await?;

        // Embed in batches sized to the provider's cap.
        let batch_size = embeddings.max_batch_size().min(64);
        let mut newest_id: Option<String> = prior.cursor.clone();

        for chunk in rows.chunks(batch_size) {
            let mut chunks_flat = Vec::new();
            for row in chunk {
                let ir = row.clone().into_ingest_record();
                let normalized =
                    chunk_record(&ir, embeddings.max_tokens_per_input());
                chunks_flat.extend(normalized);
            }
            if chunks_flat.is_empty() {
                continue;
            }

            let inputs: Vec<EmbeddingInput> = chunks_flat
                .iter()
                .map(|c| EmbeddingInput::document(&c.text))
                .collect();
            let vectors = embeddings.embed(&inputs).await?;
            if vectors.len() != chunks_flat.len() {
                return Err(IngestError::Other(format!(
                    "embedding provider returned {} vectors for {} inputs",
                    vectors.len(),
                    chunks_flat.len(),
                )));
            }

            report.embedding_tokens_used +=
                vectors.iter().map(|v| u64::from(v.input_tokens)).sum::<u64>();

            let mut records = Vec::with_capacity(chunks_flat.len());
            for (norm, emb) in chunks_flat.into_iter().zip(vectors) {
                records.push(norm.into_record(emb.vector));
            }
            let stats = vector_store
                .upsert(self.target_collection(), records)
                .await?;
            report.records_new += stats.inserted;
            report.records_updated += stats.updated;

            // Advance cursor to the largest source_id seen in this
            // batch. String ordering matches Solodit's id convention.
            for row in chunk {
                if newest_id.as_deref().is_none_or(|c| row.id.as_str() > c) {
                    newest_id = Some(row.id.clone());
                }
            }

            if let Some(cb) = &options.progress {
                cb(IngestProgress {
                    records_scanned: report.records_scanned,
                    records_upserted: report.records_new + report.records_updated,
                    records_skipped: report.records_skipped,
                    embedding_tokens_used: report.embedding_tokens_used,
                });
            }
        }

        // Persist updated state.
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .ok();
        persistent.set(
            self.source_name(),
            SourceState {
                cursor: newest_id,
                records_ingested: prior.records_ingested
                    + u64::try_from(report.records_new + report.records_updated)
                        .unwrap_or(u64::MAX),
                last_run_unix: now_secs,
            },
        );
        persistent.save(&state_file)?;

        report.duration = start.elapsed();
        Ok(report)
    }
}

/// Read the JSONL dump, optionally skipping rows at or below
/// `cursor`. Malformed lines are appended to `report.errors` and
/// skipped, not fatal.
fn read_rows(
    path: &Path,
    cursor: Option<&str>,
    max_records: Option<usize>,
    report: &mut IngestReport,
) -> Result<Vec<SoloditFindingRow>, IngestError> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut out = Vec::new();
    for (line_no, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<SoloditFindingRow>(trimmed) {
            Ok(row) => {
                if cursor.is_some_and(|c| row.id.as_str() <= c) {
                    report.records_skipped += 1;
                    continue;
                }
                out.push(row);
                if max_records.is_some_and(|m| out.len() >= m) {
                    break;
                }
            }
            Err(e) => {
                report
                    .errors
                    .push((format!("line:{}", line_no + 1), e.to_string()));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_json(id: &str) -> String {
        format!(
            r#"{{"id":"{id}","title":"Reentrancy in withdraw","body":"A reentrancy vulnerability allows attackers to drain funds.","severity":"high","category":"reentrancy","project":"aave","auditor":"Trail of Bits","report_url":"https://example.com/r","finding_url":"https://example.com/r#1","date":"2024-06-01"}}"#
        )
    }

    #[test]
    fn row_deserialises_full_shape() {
        let row: SoloditFindingRow = serde_json::from_str(&row_json("sol-1")).unwrap();
        assert_eq!(row.id, "sol-1");
        assert_eq!(row.severity.as_deref(), Some("high"));
        assert_eq!(row.auditor.as_deref(), Some("Trail of Bits"));
    }

    #[test]
    fn row_deserialises_minimal_shape() {
        // Only id+title+body are required; everything else optional.
        let minimal = r#"{"id":"sol-2","title":"T","body":"B"}"#;
        let row: SoloditFindingRow = serde_json::from_str(minimal).unwrap();
        assert_eq!(row.id, "sol-2");
        assert!(row.severity.is_none());
    }

    #[test]
    fn into_ingest_record_tags_and_extras_match_schema() {
        let row: SoloditFindingRow = serde_json::from_str(&row_json("sol-x")).unwrap();
        let ir = row.into_ingest_record();
        assert_eq!(ir.source, "solodit");
        assert_eq!(ir.source_id, "sol-x");
        assert_eq!(ir.kind, "finding");
        // Tags are lowercase-normalised and prefixed.
        assert!(ir.tags.contains(&"severity:high".to_string()));
        assert!(ir.tags.contains(&"category:reentrancy".to_string()));
        assert!(ir.tags.contains(&"auditor:trail of bits".to_string()));
        // Extras preserve original casing for display.
        assert_eq!(ir.extra.get("severity").and_then(|v| v.as_str()), Some("high"));
        assert_eq!(ir.extra.get("project").and_then(|v| v.as_str()), Some("aave"));
        assert_eq!(
            ir.extra.get("date").and_then(|v| v.as_str()),
            Some("2024-06-01"),
        );
    }

    #[test]
    fn into_ingest_record_minimal_produces_empty_tags() {
        let minimal = r#"{"id":"sol-3","title":"T","body":"B"}"#;
        let row: SoloditFindingRow = serde_json::from_str(minimal).unwrap();
        let ir = row.into_ingest_record();
        assert!(ir.tags.is_empty());
    }

    #[test]
    fn read_rows_skips_malformed_lines_and_reports_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dump.jsonl");
        let body = format!(
            "{}\n<<not json>>\n{}\n",
            row_json("sol-1"),
            row_json("sol-2"),
        );
        std::fs::write(&path, body).unwrap();
        let mut report = IngestReport::empty("solodit");
        let rows = read_rows(&path, None, None, &mut report).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].0.contains("line:2"));
    }

    #[test]
    fn read_rows_honours_cursor_for_incremental_resume() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dump.jsonl");
        let body = format!(
            "{}\n{}\n{}\n",
            row_json("sol-a"),
            row_json("sol-b"),
            row_json("sol-c"),
        );
        std::fs::write(&path, body).unwrap();
        let mut report = IngestReport::empty("solodit");
        let rows = read_rows(&path, Some("sol-a"), None, &mut report).unwrap();
        // Only sol-b and sol-c are strictly greater than the cursor.
        let ids: Vec<_> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["sol-b", "sol-c"]);
        assert_eq!(report.records_skipped, 1);
    }

    #[test]
    fn read_rows_honours_max_records_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dump.jsonl");
        let body = format!(
            "{}\n{}\n{}\n{}\n",
            row_json("sol-a"),
            row_json("sol-b"),
            row_json("sol-c"),
            row_json("sol-d"),
        );
        std::fs::write(&path, body).unwrap();
        let mut report = IngestReport::empty("solodit");
        let rows = read_rows(&path, None, Some(2), &mut report).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn read_rows_on_empty_file_returns_empty_without_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dump.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut report = IngestReport::empty("solodit");
        let rows = read_rows(&path, None, None, &mut report).unwrap();
        assert!(rows.is_empty());
        assert!(report.errors.is_empty());
    }

    #[test]
    fn default_dump_path_is_under_basilisk_knowledge() {
        let p = default_dump_path();
        let s = p.to_string_lossy();
        assert!(s.contains(".basilisk"));
        assert!(s.contains("knowledge"));
        assert!(s.ends_with("solodit_dump.jsonl"));
    }

    #[test]
    fn source_name_and_target_collection_are_stable() {
        let ingester = SoloditIngester::at_path("/tmp/nonexistent");
        assert_eq!(ingester.source_name(), "solodit");
        assert_eq!(ingester.target_collection(), schema::PUBLIC_FINDINGS);
    }

    #[tokio::test]
    async fn ingest_surfaces_source_error_when_dump_missing() {
        use basilisk_embeddings::{Embedding, EmbeddingProvider};
        // Tiny hand-rolled mock embedding provider — reused below.
        struct MockEmbed;
        #[async_trait]
        impl EmbeddingProvider for MockEmbed {
            #[allow(clippy::unnecessary_literal_bound)]
            fn identifier(&self) -> &str { "mock/embed" }
            fn dimensions(&self) -> usize { 4 }
            fn max_tokens_per_input(&self) -> usize { 1000 }
            fn max_batch_size(&self) -> usize { 32 }
            async fn embed(&self, inputs: &[EmbeddingInput]) -> Result<Vec<Embedding>, basilisk_embeddings::EmbeddingError> {
                Ok(inputs.iter().map(|_| Embedding { vector: vec![0.0; 4], input_tokens: 1 }).collect())
            }
        }

        let ingester = SoloditIngester::at_path("/tmp/definitely-missing-path.jsonl");
        let store: Arc<dyn VectorStore> =
            Arc::new(basilisk_vector::MemoryVectorStore::new());
        let embed: Arc<dyn EmbeddingProvider> = Arc::new(MockEmbed);
        let err = ingester
            .ingest(store, embed, IngestOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, IngestError::Source(_)));
    }
}
