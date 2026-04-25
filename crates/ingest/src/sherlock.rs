//! Sherlock ingester — pulls audit reports from a single GitHub repo.
//!
//! The Sherlock organization publishes every audit's findings as a
//! markdown report under `sherlock-protocol/sherlock-reports`.
//! Each audit lives in its own top-level directory; the
//! consolidated report is the directory's `README.md`. Format
//! conventions:
//!
//!   - Each finding sits under a level-2 heading like
//!     `## Issue H-1: Reentrancy ...` (severity letter + sequence).
//!   - The body up to the next `## Issue` heading is the finding.
//!   - Front matter at the top is treated as the audit overview.
//!
//! The repo is shallow-cloned via [`basilisk_git::RepoCache`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use basilisk_core::GitRef;
use basilisk_embeddings::{EmbeddingInput, EmbeddingProvider};
use basilisk_git::{CloneStrategy, FetchOptions, RepoCache};
use basilisk_vector::{schema, VectorStore};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{
    error::IngestError,
    ingester::{IngestOptions, IngestProgress, IngestRecord, IngestReport, Ingester},
    normalize::chunk_record,
    state::{IngestState, SourceState},
};

const SHERLOCK_OWNER: &str = "sherlock-protocol";
const SHERLOCK_REPO: &str = "sherlock-reports";

/// One Sherlock finding from a per-audit `README.md`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SherlockFindingRow {
    /// `<audit-id>:<severity>:<num>` — stable across re-ingests.
    pub id: String,
    pub audit: String,
    pub title: String,
    pub body: String,
    pub severity: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub finding_url: Option<String>,
}

impl SherlockFindingRow {
    #[must_use]
    pub fn into_ingest_record(self) -> IngestRecord {
        let mut tags: Vec<String> = vec![format!("severity:{}", self.severity.to_lowercase())];
        if let Some(c) = &self.category {
            tags.push(format!("category:{}", c.to_lowercase()));
        }
        tags.push(format!("audit:{}", self.audit));

        let mut extra = serde_json::Map::new();
        extra.insert("audit".into(), self.audit.into());
        extra.insert("severity".into(), self.severity.into());
        if let Some(c) = self.category {
            extra.insert("category".into(), c.into());
        }
        if let Some(u) = self.finding_url {
            extra.insert("finding_url".into(), u.into());
        }

        IngestRecord {
            source_id: self.id,
            source: "sherlock".into(),
            kind: "finding".into(),
            title: self.title,
            body: self.body,
            tags,
            extra: serde_json::Value::Object(extra),
        }
    }
}

/// Pulls Sherlock audit findings into the `public_findings` collection.
#[derive(Clone, Default)]
pub struct SherlockIngester {
    cache_root: Option<PathBuf>,
}

impl SherlockIngester {
    #[must_use]
    pub fn new() -> Self {
        Self { cache_root: None }
    }

    #[cfg(test)]
    #[must_use]
    pub fn with_cache_root(mut self, root: PathBuf) -> Self {
        self.cache_root = Some(root);
        self
    }
}

#[async_trait]
impl Ingester for SherlockIngester {
    #[allow(clippy::unnecessary_literal_bound)]
    fn source_name(&self) -> &str {
        "sherlock"
    }

    fn target_collection(&self) -> &str {
        schema::PUBLIC_FINDINGS
    }

    #[allow(clippy::too_many_lines)]
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

        let cache = if let Some(root) = &self.cache_root {
            RepoCache::open_at(root.clone()).map_err(|e| IngestError::Other(e.to_string()))?
        } else {
            RepoCache::open().map_err(|e| IngestError::Other(e.to_string()))?
        };

        let opts = FetchOptions {
            strategy: CloneStrategy::Shallow,
            force_refresh: false,
            github: None,
        };
        let fetched = match cache
            .fetch(
                SHERLOCK_OWNER,
                SHERLOCK_REPO,
                Some(GitRef::Branch("main".into())),
                opts.clone(),
            )
            .await
        {
            Ok(r) => r,
            Err(basilisk_git::GitError::RefNotFound { .. }) => cache
                .fetch(
                    SHERLOCK_OWNER,
                    SHERLOCK_REPO,
                    Some(GitRef::Branch("master".into())),
                    opts,
                )
                .await
                .map_err(|e| IngestError::Source(format!("cloning sherlock-reports: {e}")))?,
            Err(e) => {
                return Err(IngestError::Source(format!(
                    "cloning sherlock-reports: {e}"
                )))
            }
        };

        let rows = scan_sherlock_repo(&fetched.working_tree);
        report.records_scanned = rows.len();

        if let Some(cb) = &options.progress {
            cb(IngestProgress {
                records_scanned: report.records_scanned,
                records_upserted: 0,
                records_skipped: 0,
                embedding_tokens_used: 0,
            });
        }

        let spec = schema::public_findings(embeddings.identifier(), embeddings.dimensions());
        vector_store.create_collection(spec).await?;

        let max = options.max_records.unwrap_or(usize::MAX);
        let batch_size = embeddings.max_batch_size().min(32);
        let mut latest_id = prior.cursor.clone();
        let mut flat = Vec::new();

        for row in rows.into_iter().take(max) {
            let id = row.id.clone();
            if options.incremental && prior.cursor.as_deref().is_some_and(|c| id.as_str() <= c) {
                report.records_skipped += 1;
                continue;
            }
            if latest_id.as_deref().is_none_or(|c| id.as_str() > c) {
                latest_id = Some(id);
            }
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
                cursor: latest_id,
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

/// Walk every audit subdirectory; pull each one's `README.md` and
/// extract findings.
fn scan_sherlock_repo(repo_root: &Path) -> Vec<SherlockFindingRow> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(repo_root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(audit_id) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // Skip dotfiles and conventional non-audit dirs.
        if audit_id.starts_with('.') {
            continue;
        }
        let readme = path.join("README.md");
        if !readme.is_file() {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&readme) else {
            continue;
        };
        out.extend(parse_audit_readme(audit_id, &body));
    }
    out
}

/// `## Issue H-1: title` / `## Issue M-7: title` heading shape.
fn parse_audit_readme(audit_id: &str, body: &str) -> Vec<SherlockFindingRow> {
    let header_re =
        Regex::new(r"(?m)^##\s+Issue\s+([HMLI])-?(\d+)\s*[:.\-]?\s*(.+?)\s*$").expect("regex");
    let mut out = Vec::new();
    let mut last: Option<(String, String, String, usize)> = None;
    for caps in header_re.captures_iter(body) {
        let sev_letter = caps.get(1).map_or("", |g| g.as_str()).to_string();
        let num = caps.get(2).map_or("", |g| g.as_str()).to_string();
        let title = caps.get(3).map_or("", |g| g.as_str()).to_string();
        let body_start = caps.get(0).map_or(0, |g| g.end());
        if let Some((p_sev, p_num, p_title, p_start)) = last.take() {
            let header_pos = caps.get(0).map_or(body.len(), |g| g.start());
            let prev_body = body
                .get(p_start..header_pos)
                .unwrap_or("")
                .trim()
                .to_string();
            out.push(make_row(audit_id, &p_sev, &p_num, &p_title, &prev_body));
        }
        last = Some((sev_letter, num, title, body_start));
    }
    if let Some((sev, num, title, start)) = last {
        let prev_body = body.get(start..).unwrap_or("").trim().to_string();
        out.push(make_row(audit_id, &sev, &num, &title, &prev_body));
    }
    out
}

fn make_row(
    audit_id: &str,
    severity_letter: &str,
    num: &str,
    title: &str,
    body: &str,
) -> SherlockFindingRow {
    let severity = match severity_letter {
        "H" => "high",
        "M" => "medium",
        "L" => "low",
        _ => "info",
    };
    SherlockFindingRow {
        id: format!("{audit_id}:{severity_letter}:{num}"),
        audit: audit_id.into(),
        title: title.into(),
        body: body.into(),
        severity: severity.into(),
        category: None,
        finding_url: Some(format!(
            "https://github.com/sherlock-protocol/sherlock-reports/blob/main/{audit_id}/README.md"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn parse_audit_readme_pulls_three_findings() {
        let body = "# Foo Audit Report

Some overview prose at the top.

## Issue H-1: Reentrancy in withdraw

The withdraw function calls...

Multiple paragraphs of body.

## Issue M-2: Precision loss in fee calculation

Body for medium finding.

## Issue L-7: Gas optimisation

Body for low finding.
";
        let rows = parse_audit_readme("2024-01-foo", body);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].severity, "high");
        assert_eq!(rows[0].title, "Reentrancy in withdraw");
        assert!(rows[0].body.contains("Multiple paragraphs"));
        assert_eq!(rows[1].severity, "medium");
        assert_eq!(rows[2].severity, "low");
        assert_eq!(rows[0].id, "2024-01-foo:H:1");
    }

    #[test]
    fn parse_audit_readme_handles_no_findings() {
        let body = "# Foo Audit\n\nNo issues identified.";
        let rows = parse_audit_readme("foo", body);
        assert!(rows.is_empty());
    }

    #[test]
    fn scan_sherlock_repo_pulls_from_each_subdir() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        for audit in ["2024-01-foo", "2024-02-bar"] {
            let sub = root.join(audit);
            fs::create_dir_all(&sub).unwrap();
            fs::write(
                sub.join("README.md"),
                format!(
                    "# {audit} report\n\n## Issue H-1: A finding\n\nbody\n## Issue M-2: Another\n\nbody two"
                ),
            )
            .unwrap();
        }
        // Sanity dotfile that should be skipped.
        fs::create_dir(root.join(".github")).unwrap();
        let rows = scan_sherlock_repo(root);
        assert_eq!(rows.len(), 4);
        let audits: std::collections::HashSet<&str> =
            rows.iter().map(|r| r.audit.as_str()).collect();
        assert!(audits.contains("2024-01-foo"));
        assert!(audits.contains("2024-02-bar"));
        assert!(!audits.contains(".github"));
    }

    #[test]
    fn scan_sherlock_repo_skips_dirs_without_readme() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("empty-audit")).unwrap();
        let rows = scan_sherlock_repo(root);
        assert!(rows.is_empty());
    }

    #[test]
    fn into_ingest_record_carries_metadata() {
        let row = SherlockFindingRow {
            id: "2024-01-foo:H:1".into(),
            audit: "2024-01-foo".into(),
            title: "Reentrancy".into(),
            body: "body".into(),
            severity: "high".into(),
            category: Some("reentrancy".into()),
            finding_url: Some("https://example".into()),
        };
        let ir = row.into_ingest_record();
        assert_eq!(ir.source, "sherlock");
        assert_eq!(ir.kind, "finding");
        assert!(ir.tags.contains(&"severity:high".to_string()));
        assert!(ir.tags.contains(&"audit:2024-01-foo".to_string()));
    }

    #[test]
    fn ingester_source_name_and_target_collection_are_stable() {
        let i = SherlockIngester::new();
        assert_eq!(i.source_name(), "sherlock");
        assert_eq!(i.target_collection(), schema::PUBLIC_FINDINGS);
    }
}
