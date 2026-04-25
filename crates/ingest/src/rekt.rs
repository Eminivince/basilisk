//! rekt.news ingester — reads a local JSONL dump.
//!
//! rekt.news has no public API and changes layout occasionally,
//! so the primary path is a **user-supplied JSONL file** placed at
//! `~/.basilisk/knowledge/rekt_dump.jsonl` (override via
//! constructor). Live HTML scraping is intentionally deferred —
//! per the spec, parse failures should log+skip rather than
//! crash the ingest run, and that's easiest to enforce when the
//! input is operator-curated.
//!
//! One post-mortem per JSONL line. Loss-amount is **bucketed**
//! (`<$1M`, `$1M-$10M`, `$10M-$100M`, `>$100M`) into a tag for
//! filtering — the raw USD figure is preserved in `extra.loss_usd`.

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

/// One row from the rekt.news JSONL dump.
///
/// `id` should be a stable slug (rekt.news's URL slug works well —
/// e.g. `euler-rekt`). `loss_usd` is optional but strongly
/// recommended; when provided, it's bucketed into a filter tag.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RektPostMortemRow {
    pub id: String,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub chain: Option<String>,
    #[serde(default)]
    pub loss_usd: Option<u64>,
    #[serde(default)]
    pub attack_vector: Option<String>,
    #[serde(default)]
    pub post_url: Option<String>,
}

impl RektPostMortemRow {
    #[must_use]
    pub fn into_ingest_record(self) -> IngestRecord {
        let mut tags: Vec<String> = Vec::new();
        if let Some(c) = &self.chain {
            tags.push(format!("chain:{}", c.to_lowercase()));
        }
        if let Some(loss) = self.loss_usd {
            tags.push(format!("loss_amount:{}", bucket_loss(loss)));
        }
        if let Some(p) = &self.protocol {
            tags.push(format!("protocol:{}", p.to_lowercase()));
        }
        if let Some(av) = &self.attack_vector {
            tags.push(format!("attack:{}", av.to_lowercase()));
        }

        let mut extra = serde_json::Map::new();
        if let Some(p) = self.protocol {
            extra.insert("protocol".into(), p.into());
        }
        if let Some(d) = self.date {
            extra.insert("date".into(), d.into());
        }
        if let Some(loss) = self.loss_usd {
            extra.insert("loss_usd".into(), loss.into());
        }
        if let Some(av) = self.attack_vector {
            extra.insert("attack_vector".into(), av.into());
        }
        if let Some(u) = self.post_url {
            extra.insert("post_url".into(), u.into());
        }
        if let Some(c) = self.chain {
            extra.insert("chain".into(), c.into());
        }

        IngestRecord {
            source_id: self.id,
            source: "rekt".into(),
            kind: "post_mortem".into(),
            title: self.title,
            body: self.body,
            tags,
            extra: serde_json::Value::Object(extra),
        }
    }
}

/// Map a USD loss figure into a filterable bucket.
///
/// Buckets follow the spec: `<$1M`, `$1M-$10M`, `$10M-$100M`,
/// `>$100M`. The exact dollar figure stays in `extra.loss_usd`.
pub fn bucket_loss(loss_usd: u64) -> &'static str {
    if loss_usd < 1_000_000 {
        "lt_1m"
    } else if loss_usd < 10_000_000 {
        "1m_10m"
    } else if loss_usd < 100_000_000 {
        "10m_100m"
    } else {
        "gt_100m"
    }
}

const DEFAULT_DUMP: &str = "rekt_dump.jsonl";

/// Reads `~/.basilisk/knowledge/rekt_dump.jsonl` (or override).
#[derive(Clone)]
pub struct RektIngester {
    dump_path: Option<PathBuf>,
}

impl RektIngester {
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

impl Default for RektIngester {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Ingester for RektIngester {
    #[allow(clippy::unnecessary_literal_bound)]
    fn source_name(&self) -> &str {
        "rekt"
    }

    fn target_collection(&self) -> &str {
        schema::POST_MORTEMS
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
                "rekt dump not found at {}: place a JSONL dump there or pass --dump <path>",
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

        let spec = schema::post_mortems(embeddings.identifier(), embeddings.dimensions());
        vector_store.create_collection(spec).await?;

        let max = options.max_records.unwrap_or(usize::MAX);
        let batch_size = embeddings.max_batch_size().min(32);
        // File-position cursor (line number) — same pattern as Solodit.
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

fn read_jsonl(
    path: &Path,
    report: &mut IngestReport,
) -> Result<Vec<RektPostMortemRow>, IngestError> {
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
        match serde_json::from_str::<RektPostMortemRow>(&line) {
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
    fn loss_bucketing() {
        assert_eq!(bucket_loss(500_000), "lt_1m");
        assert_eq!(bucket_loss(1_000_000), "1m_10m");
        assert_eq!(bucket_loss(9_999_999), "1m_10m");
        assert_eq!(bucket_loss(10_000_000), "10m_100m");
        assert_eq!(bucket_loss(99_999_999), "10m_100m");
        assert_eq!(bucket_loss(100_000_000), "gt_100m");
        assert_eq!(bucket_loss(2_000_000_000), "gt_100m");
    }

    #[test]
    fn into_ingest_record_emits_loss_bucket_and_chain_tag() {
        let row = RektPostMortemRow {
            id: "euler-rekt".into(),
            title: "Euler Finance — donation attack".into(),
            body: "post-mortem body".into(),
            date: Some("2023-03-13".into()),
            protocol: Some("Euler".into()),
            chain: Some("Ethereum".into()),
            loss_usd: Some(197_000_000),
            attack_vector: Some("donation_attack".into()),
            post_url: Some("https://rekt.news/euler-rekt/".into()),
        };
        let ir = row.into_ingest_record();
        assert_eq!(ir.source, "rekt");
        assert_eq!(ir.kind, "post_mortem");
        assert!(ir.tags.contains(&"loss_amount:gt_100m".to_string()));
        assert!(ir.tags.contains(&"chain:ethereum".to_string()));
        assert!(ir.tags.contains(&"protocol:euler".to_string()));
        assert!(ir.tags.contains(&"attack:donation_attack".to_string()));
        assert_eq!(ir.extra["loss_usd"], 197_000_000);
        assert_eq!(ir.extra["date"], "2023-03-13");
    }

    #[test]
    fn read_jsonl_handles_partial_failures() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rekt.jsonl");
        fs::write(
            &path,
            r#"{"id":"a","title":"A","body":"a","loss_usd":500000}
not valid json
{"id":"b","title":"B","body":"b","loss_usd":50000000}
"#,
        )
        .unwrap();
        let mut report = IngestReport::empty("rekt");
        let rows = read_jsonl(&path, &mut report).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(rows[0].id, "a");
        assert_eq!(rows[1].id, "b");
    }

    #[test]
    fn read_jsonl_skips_blank_lines() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rekt.jsonl");
        fs::write(
            &path,
            r#"{"id":"a","title":"A","body":"a"}

{"id":"b","title":"B","body":"b"}
"#,
        )
        .unwrap();
        let mut report = IngestReport::empty("rekt");
        let rows = read_jsonl(&path, &mut report).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(report.errors.is_empty());
    }

    #[test]
    fn ingester_source_name_and_target_collection_are_stable() {
        let i = RektIngester::new();
        assert_eq!(i.source_name(), "rekt");
        assert_eq!(i.target_collection(), schema::POST_MORTEMS);
    }
}
