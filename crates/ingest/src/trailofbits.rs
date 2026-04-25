//! Trail of Bits blog ingester — reads a local JSONL dump.
//!
//! Trail of Bits ships a public RSS feed at `blog.trailofbits.com`,
//! but the blog covers a wide range of topics (compilers,
//! cryptography, ML safety, etc.) and only a subset is relevant to
//! smart-contract auditing. To avoid baking topic-classification
//! heuristics into the ingester, the primary path is a
//! **user-supplied JSONL file** at
//! `~/.basilisk/knowledge/tob_dump.jsonl` (override via
//! constructor). The operator curates which posts are
//! security-relevant; one post per JSONL line.
//!
//! RSS scraping is intentionally deferred. When/if a focused
//! security-only feed appears, an alternate `Ingester` impl can
//! land additively without changing the trait or this row format.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use basilisk_embeddings::{EmbeddingInput, EmbeddingProvider};
use basilisk_vector::{schema, VectorStore};
use serde::{Deserialize, Serialize};

use crate::{
    error::IngestError,
    ingester::{IngestOptions, IngestProgress, IngestRecord, IngestReport, Ingester},
    normalize::chunk_record,
    state::{IngestState, SourceState},
};

/// One row from the Trail of Bits JSONL dump.
///
/// `topics` carries the operator-curated tag list — typically
/// `["smart-contracts", "evm", "fuzzing"]` etc. Each entry becomes
/// a `topic:<tag>` filter tag in the indexed record.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TobBlogRow {
    pub id: String,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub topics: Vec<String>,
    #[serde(default)]
    pub post_url: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
}

impl TobBlogRow {
    #[must_use]
    pub fn into_ingest_record(self) -> IngestRecord {
        let mut tags: Vec<String> = self
            .topics
            .iter()
            .map(|t| format!("topic:{}", t.to_lowercase()))
            .collect();

        let mut extra = serde_json::Map::new();
        extra.insert("title".into(), self.title.clone().into());
        if let Some(d) = self.date {
            extra.insert("date".into(), d.into());
        }
        if let Some(u) = self.post_url {
            extra.insert("post_url".into(), u.into());
        }
        if let Some(s) = self.summary {
            extra.insert("summary".into(), s.into());
        }
        if !self.topics.is_empty() {
            extra.insert(
                "topics".into(),
                serde_json::Value::Array(self.topics.into_iter().map(Into::into).collect()),
            );
        }

        // Body fallback: if the operator only supplied summary +
        // metadata, use the summary as the body so embedding has
        // something substantive.
        let body = if self.body.trim().is_empty() {
            extra
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        } else {
            self.body
        };

        // Add an "advisory" marker so retrieval can filter for
        // ToB-style writeups vs. raw post-mortems or audit findings.
        if !tags.iter().any(|t| t.starts_with("topic:")) {
            tags.push("topic:security".into());
        }

        IngestRecord {
            source_id: self.id,
            source: "trailofbits".into(),
            kind: "advisory".into(),
            title: self.title,
            body,
            tags,
            extra: serde_json::Value::Object(extra),
        }
    }
}

const DEFAULT_DUMP: &str = "tob_dump.jsonl";

#[derive(Clone)]
pub struct TobBlogIngester {
    dump_path: Option<PathBuf>,
}

impl TobBlogIngester {
    #[must_use]
    pub fn new() -> Self {
        Self { dump_path: None }
    }

    #[must_use]
    pub fn with_dump_path(mut self, path: PathBuf) -> Self {
        self.dump_path = Some(path);
        self
    }
}

impl Default for TobBlogIngester {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Ingester for TobBlogIngester {
    #[allow(clippy::unnecessary_literal_bound)]
    fn source_name(&self) -> &str {
        "trailofbits"
    }

    fn target_collection(&self) -> &str {
        schema::ADVISORIES
    }

    async fn ingest(
        &self,
        vector_store: Arc<dyn VectorStore>,
        embeddings: Arc<dyn EmbeddingProvider>,
        options: IngestOptions,
    ) -> Result<IngestReport, IngestError> {
        let start = Instant::now();
        let mut report = IngestReport::empty(self.source_name());

        let state_file = crate::state::default_state_path();
        let mut persistent = IngestState::load(&state_file)?;
        let prior = persistent.get(self.source_name());

        let path = self.dump_path.clone().unwrap_or_else(|| {
            crate::state::default_knowledge_root()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(DEFAULT_DUMP)
        });

        if !path.exists() {
            return Err(IngestError::Source(format!(
                "Trail of Bits dump not found at {}: place a JSONL dump there",
                path.display()
            )));
        }

        let rows = read_jsonl(&path, &mut report)?;
        report.records_scanned = rows.len();

        if let Some(cb) = &options.progress {
            cb(IngestProgress {
                records_scanned: report.records_scanned,
                records_upserted: 0,
                records_skipped: 0,
                embedding_tokens_used: 0,
            });
        }

        let spec = schema::advisories(embeddings.identifier(), embeddings.dimensions());
        vector_store.create_collection(spec).await?;

        let max = options.max_records.unwrap_or(usize::MAX);
        let batch_size = embeddings.max_batch_size().min(32);
        let mut latest_line = prior.cursor.clone();
        let prior_line: usize = prior
            .cursor
            .as_ref()
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let mut flat = Vec::new();

        for (idx, row) in rows.into_iter().enumerate() {
            if flat.len() >= max {
                break;
            }
            if options.incremental && idx < prior_line {
                report.records_skipped += 1;
                continue;
            }
            latest_line = Some((idx + 1).to_string());
            let ir = row.into_ingest_record();
            let chunks = chunk_record(&ir, embeddings.max_tokens_per_input());
            flat.extend(chunks);
        }

        for batch in flat.chunks(batch_size) {
            if batch.is_empty() {
                continue;
            }
            let inputs: Vec<EmbeddingInput> = batch
                .iter()
                .map(|c| EmbeddingInput::document(&c.text))
                .collect();
            let vectors = embeddings.embed(&inputs).await?;
            report.embedding_tokens_used += vectors
                .iter()
                .map(|v| u64::from(v.input_tokens))
                .sum::<u64>();
            let records: Vec<_> = batch
                .iter()
                .cloned()
                .zip(vectors)
                .map(|(n, v)| n.into_record(v.vector))
                .collect();
            let stats = vector_store
                .upsert(self.target_collection(), records)
                .await?;
            report.records_new += stats.inserted;
            report.records_updated += stats.updated;

            if let Some(cb) = &options.progress {
                cb(IngestProgress {
                    records_scanned: report.records_scanned,
                    records_upserted: report.records_new + report.records_updated,
                    records_skipped: report.records_skipped,
                    embedding_tokens_used: report.embedding_tokens_used,
                });
            }
        }

        persistent.set(
            self.source_name(),
            SourceState {
                cursor: latest_line,
                records_ingested: prior.records_ingested
                    + u64::try_from(report.records_new + report.records_updated)
                        .unwrap_or(u64::MAX),
                last_run_unix: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .ok(),
            },
        );
        persistent.save(&state_file)?;
        report.duration = start.elapsed();
        Ok(report)
    }
}

fn read_jsonl(path: &Path, report: &mut IngestReport) -> Result<Vec<TobBlogRow>, IngestError> {
    let f = std::fs::File::open(path)
        .map_err(|e| IngestError::Source(format!("open {}: {e}", path.display())))?;
    let reader = std::io::BufReader::new(f);
    let mut out = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                report
                    .errors
                    .push((format!("line {}", i + 1), format!("read: {e}")));
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<TobBlogRow>(&line) {
            Ok(row) => out.push(row),
            Err(e) => {
                report
                    .errors
                    .push((format!("line {}", i + 1), format!("parse: {e}")));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn into_ingest_record_emits_topic_tags() {
        let row = TobBlogRow {
            id: "fuzzing-evm-2023".into(),
            title: "Fuzzing the EVM".into(),
            body: "long body".into(),
            date: Some("2023-08-01".into()),
            topics: vec!["smart-contracts".into(), "fuzzing".into()],
            post_url: Some("https://blog.trailofbits.com/...".into()),
            summary: None,
        };
        let ir = row.into_ingest_record();
        assert_eq!(ir.source, "trailofbits");
        assert_eq!(ir.kind, "advisory");
        assert!(ir.tags.contains(&"topic:smart-contracts".to_string()));
        assert!(ir.tags.contains(&"topic:fuzzing".to_string()));
    }

    #[test]
    fn into_ingest_record_falls_back_to_summary_when_body_empty() {
        let row = TobBlogRow {
            id: "x".into(),
            title: "Title".into(),
            body: String::new(),
            date: None,
            topics: vec!["smart-contracts".into()],
            post_url: None,
            summary: Some("a summary that's not empty".into()),
        };
        let ir = row.into_ingest_record();
        assert!(ir.body.contains("summary that's not empty"));
    }

    #[test]
    fn into_ingest_record_defaults_topic_when_none() {
        let row = TobBlogRow {
            id: "x".into(),
            title: "T".into(),
            body: "b".into(),
            date: None,
            topics: vec![],
            post_url: None,
            summary: None,
        };
        let ir = row.into_ingest_record();
        assert!(ir.tags.iter().any(|t| t == "topic:security"));
    }

    #[test]
    fn read_jsonl_handles_partial_failures() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tob.jsonl");
        fs::write(
            &path,
            r#"{"id":"a","title":"A","body":"a","topics":["smart-contracts"]}
not valid json
{"id":"b","title":"B","body":"b"}
"#,
        )
        .unwrap();
        let mut report = IngestReport::empty("trailofbits");
        let rows = read_jsonl(&path, &mut report).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(report.errors.len(), 1);
    }

    #[test]
    fn ingester_source_name_and_target_collection_are_stable() {
        let i = TobBlogIngester::new();
        assert_eq!(i.source_name(), "trailofbits");
        assert_eq!(i.target_collection(), schema::ADVISORIES);
    }
}
