//! Code4rena ingester — pulls public contest reports from GitHub.
//!
//! Code4rena ships every contest's findings as a separate
//! `code-423n4/<contest-id>-findings` repository on GitHub. The
//! findings appear in two shapes that this ingester handles:
//!
//!   - **Per-finding markdown** under `data/<auditor-handle>-<severity>.md`,
//!     `data/<contest>-H-<n>.md`, etc.
//!   - **Consolidated `report.md`** at the repo root, with each
//!     finding sectioned under `## [H-NN] ...` / `## [M-NN] ...` /
//!     `## [Q-NN] ...` headers.
//!
//! Both shapes flow into the same [`IngestRecord`] via
//! [`Code4renaFindingRow::into_ingest_record`].
//!
//! Source acquisition uses [`basilisk_git::RepoCache`] so re-ingests
//! reuse already-cached clones. Operators set the contest list
//! either via [`Code4renaIngester::with_contests`] (explicit list)
//! or by relying on the bundled default list (curated, slow-moving
//! — periodic refresh via the operator's manual list update).

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
use sha2::{Digest, Sha256};

use crate::{
    error::IngestError,
    ingester::{IngestOptions, IngestProgress, IngestRecord, IngestReport, Ingester},
    normalize::chunk_record,
    state::{IngestState, SourceState},
};

/// One Code4rena finding, normalised across the per-finding-file
/// and consolidated-report shapes.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Code4renaFindingRow {
    /// Stable id derived from `(contest, severity, number)` or a
    /// content-hash when the structured id isn't recoverable.
    pub id: String,
    pub contest: String,
    pub title: String,
    pub body: String,
    pub severity: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub auditor: Option<String>,
    #[serde(default)]
    pub finding_url: Option<String>,
}

impl Code4renaFindingRow {
    /// Convert into the source-neutral [`IngestRecord`] shape.
    #[must_use]
    pub fn into_ingest_record(self) -> IngestRecord {
        let mut tags: Vec<String> = vec![format!("severity:{}", self.severity.to_lowercase())];
        if let Some(c) = &self.category {
            tags.push(format!("category:{}", c.to_lowercase()));
        }
        if let Some(a) = &self.auditor {
            tags.push(format!("auditor:{a}"));
        }
        tags.push(format!("contest:{}", self.contest));

        let mut extra = serde_json::Map::new();
        extra.insert("contest".into(), self.contest.into());
        extra.insert("severity".into(), self.severity.into());
        if let Some(c) = self.category {
            extra.insert("category".into(), c.into());
        }
        if let Some(a) = self.auditor {
            extra.insert("auditor".into(), a.into());
        }
        if let Some(u) = self.finding_url {
            extra.insert("finding_url".into(), u.into());
        }

        IngestRecord {
            source_id: self.id,
            source: "code4rena".into(),
            kind: "finding".into(),
            title: self.title,
            body: self.body,
            tags,
            extra: serde_json::Value::Object(extra),
        }
    }
}

/// Bundled list of Code4rena contests. Curated; operators override
/// via `Code4renaIngester::with_contests`. The pattern is just the
/// repo name suffix (`<id>-findings`) — the prefix `code-423n4/`
/// is constant.
///
/// Kept short and focused on high-impact / well-documented contests
/// rather than enumerating all ~500+ Code4rena contests; expanding
/// this list is operator work guided by where retrieval-quality
/// matters.
pub const DEFAULT_CONTESTS: &[&str] = &[
    "2026-04-monetrix",
    "2023-05-ajna",
    "2023-08-shell",
    "2023-10-ethena",
    "2024-01-salty",
    "2024-02-spectra",
    "2024-04-renzo",
];

/// Pulls public Code4rena contest findings into the
/// `public_findings` collection.
#[derive(Clone)]
pub struct Code4renaIngester {
    contests: Vec<String>,
    /// Optional override for the cache root (defaults to the global
    /// `RepoCache` location). Set by tests.
    cache_root: Option<PathBuf>,
}

impl Code4renaIngester {
    #[must_use]
    pub fn new() -> Self {
        Self {
            contests: DEFAULT_CONTESTS.iter().map(|s| (*s).to_string()).collect(),
            cache_root: None,
        }
    }

    /// Override the contest list. Each entry is the slug before
    /// `-findings` (e.g. `2024-04-renzo`).
    #[must_use]
    pub fn with_contests(mut self, contests: Vec<String>) -> Self {
        self.contests = contests;
        self
    }

    #[cfg(test)]
    #[must_use]
    pub fn with_cache_root(mut self, root: PathBuf) -> Self {
        self.cache_root = Some(root);
        self
    }
}

impl Default for Code4renaIngester {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Ingester for Code4renaIngester {
    #[allow(clippy::unnecessary_literal_bound)]
    fn source_name(&self) -> &str {
        "code4rena"
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

        let max = options.max_records.unwrap_or(usize::MAX);
        let spec = schema::public_findings(embeddings.identifier(), embeddings.dimensions());
        vector_store.create_collection(spec).await?;

        let mut all_rows: Vec<Code4renaFindingRow> = Vec::new();
        let max_contests_to_visit = if options.incremental {
            // Incremental: visit contests not yet seen plus the
            // newest one (in case it changed). Operator-supplied
            // contest list is short enough to scan fully cheaply.
            self.contests.len()
        } else {
            self.contests.len()
        };
        for contest_slug in self.contests.iter().take(max_contests_to_visit) {
            if all_rows.len() >= max {
                break;
            }
            let repo_name = format!("{contest_slug}-findings");
            let opts = FetchOptions {
                strategy: CloneStrategy::Shallow,
                force_refresh: false,
                github: None,
            };
            // Try `main` first, fall back to `master`. Both
            // appear across the contest-archive history.
            let fetched = match cache
                .fetch(
                    "code-423n4",
                    &repo_name,
                    Some(GitRef::Branch("main".into())),
                    opts.clone(),
                )
                .await
            {
                Ok(r) => r,
                Err(basilisk_git::GitError::RefNotFound { .. }) => {
                    match cache
                        .fetch(
                            "code-423n4",
                            &repo_name,
                            Some(GitRef::Branch("master".into())),
                            opts,
                        )
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            report.errors.push((repo_name, e.to_string()));
                            continue;
                        }
                    }
                }
                Err(e) => {
                    report.errors.push((repo_name, e.to_string()));
                    continue;
                }
            };
            let rows = parse_contest_repo(&fetched.working_tree, contest_slug);
            report.records_scanned += rows.len();
            for r in rows {
                if all_rows.len() >= max {
                    break;
                }
                all_rows.push(r);
            }
        }

        // Early progress tick so operators see scanning finished
        // before embedding starts.
        if let Some(cb) = &options.progress {
            cb(IngestProgress {
                records_scanned: report.records_scanned,
                records_upserted: 0,
                records_skipped: 0,
                embedding_tokens_used: 0,
            });
        }

        // Cursor: largest source_id seen. Code4rena ids are
        // structured (`<contest>:<severity>:<num>`) — lex sort is
        // stable enough for "have we seen this one."
        let mut latest_id = prior.cursor.clone();
        let batch_size = embeddings.max_batch_size().min(32);
        let mut flat = Vec::new();

        for row in all_rows {
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

/// Walk a cloned `code-423n4/<contest>-findings` repo and pull every
/// finding it contains, regardless of which of the two shapes is used.
fn parse_contest_repo(repo_root: &Path, contest_slug: &str) -> Vec<Code4renaFindingRow> {
    let mut out = Vec::new();
    // Shape A: per-finding markdown under `data/`.
    let data_dir = repo_root.join("data");
    if data_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&data_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                if let Ok(body) = std::fs::read_to_string(&path) {
                    if let Some(row) = parse_per_file_finding(&path, contest_slug, &body) {
                        out.push(row);
                    }
                }
            }
        }
    }
    // Shape B: consolidated `report.md` at root.
    let report = repo_root.join("report.md");
    if report.is_file() {
        if let Ok(body) = std::fs::read_to_string(&report) {
            out.extend(parse_consolidated_report(contest_slug, &body));
        }
    }
    out
}

/// Per-file shape: filename like `BBB-H-001.md` (auditor handle +
/// severity + number) or `c4-2023-04-ondo-H-01.md`. The first
/// `# ` heading inside is the title.
fn parse_per_file_finding(
    path: &Path,
    contest_slug: &str,
    body: &str,
) -> Option<Code4renaFindingRow> {
    let stem = path.file_stem()?.to_str()?;
    let (auditor, severity, num) = parse_filename(stem);
    let title = first_h1(body).unwrap_or_else(|| stem.to_string());
    let id = if let Some(num) = num {
        format!("{contest_slug}:{severity}:{num}")
    } else {
        content_hash_id(contest_slug, body)
    };
    Some(Code4renaFindingRow {
        id,
        contest: contest_slug.into(),
        title,
        body: body.to_string(),
        severity: severity.to_string(),
        category: None,
        auditor,
        finding_url: Some(format!(
            "https://github.com/code-423n4/{contest_slug}-findings/blob/main/data/{stem}.md"
        )),
    })
}

/// Consolidated report.md shape: each finding is a level-2 heading
/// like `## [H-01] Reentrancy in withdraw()`. Walk headings, pair
/// each with its body up to the next heading.
fn parse_consolidated_report(contest_slug: &str, body: &str) -> Vec<Code4renaFindingRow> {
    let header_re = Regex::new(r"(?m)^##\s+\[([HMQGI])-(\d+)\]\s+(.+?)\s*$").expect("regex");
    let mut out = Vec::new();
    let mut last: Option<(String, String, String, usize)> = None; // (severity, num, title, body_start)
    for m in header_re.captures_iter(body) {
        let sev = m.get(1).map_or("", |g| g.as_str()).to_string();
        let num = m.get(2).map_or("", |g| g.as_str()).to_string();
        let title = m.get(3).map_or("", |g| g.as_str()).to_string();
        let body_start = m.get(0).map_or(0, |g| g.end());

        if let Some((prev_sev, prev_num, prev_title, prev_start)) = last.take() {
            let header_pos = m.get(0).map_or(body.len(), |g| g.start());
            let prev_body = body
                .get(prev_start..header_pos)
                .unwrap_or("")
                .trim()
                .to_string();
            out.push(make_consolidated_row(
                contest_slug,
                &prev_sev,
                &prev_num,
                &prev_title,
                &prev_body,
            ));
        }
        last = Some((sev, num, title, body_start));
    }
    if let Some((sev, num, title, start)) = last {
        let body_text = body.get(start..).unwrap_or("").trim().to_string();
        out.push(make_consolidated_row(
            contest_slug,
            &sev,
            &num,
            &title,
            &body_text,
        ));
    }
    out
}

fn make_consolidated_row(
    contest_slug: &str,
    severity_letter: &str,
    num: &str,
    title: &str,
    body: &str,
) -> Code4renaFindingRow {
    let severity = match severity_letter {
        "H" => "high",
        "M" => "medium",
        "Q" | "G" => "low",
        _ => "info",
    }
    .to_string();
    Code4renaFindingRow {
        id: format!("{contest_slug}:{severity_letter}:{num}"),
        contest: contest_slug.into(),
        title: title.into(),
        body: body.into(),
        severity,
        category: None,
        auditor: None,
        finding_url: Some(format!(
            "https://github.com/code-423n4/{contest_slug}-findings/blob/main/report.md"
        )),
    }
}

/// Extract `(auditor_handle, severity_word, number_string)` from a
/// per-finding filename stem like `BBB-H-001` or `0xfoo-M-12`.
/// Severity letter mapping: H→high, M→medium, L→low, Q/G→low,
/// I→info. When the filename doesn't match, severity defaults to
/// "info" and auditor/number become None.
fn parse_filename(stem: &str) -> (Option<String>, &'static str, Option<String>) {
    let re = Regex::new(r"^(.*?)-([HMLQGI])-(\d+)$").expect("regex");
    if let Some(caps) = re.captures(stem) {
        let auditor = caps
            .get(1)
            .map(|g| g.as_str().to_string())
            .filter(|s| !s.is_empty());
        let sev = match caps.get(2).map_or("", |g| g.as_str()) {
            "H" => "high",
            "M" => "medium",
            "L" | "Q" | "G" => "low",
            _ => "info",
        };
        let num = caps.get(3).map(|g| g.as_str().to_string());
        (auditor, sev, num)
    } else {
        (None, "info", None)
    }
}

fn first_h1(body: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("# ") {
            return Some(rest.to_string());
        }
    }
    None
}

fn content_hash_id(contest_slug: &str, body: &str) -> String {
    let mut h = Sha256::new();
    h.update(contest_slug.as_bytes());
    h.update(b"\0");
    h.update(body.as_bytes());
    let digest = h.finalize();
    format!("{contest_slug}:hash:{}", hex::encode(&digest[..8]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn parse_filename_handles_per_finding_shape() {
        let (a, s, n) = parse_filename("BBB-H-001");
        assert_eq!(a.as_deref(), Some("BBB"));
        assert_eq!(s, "high");
        assert_eq!(n.as_deref(), Some("001"));
    }

    #[test]
    fn parse_filename_handles_dashed_handle() {
        let (a, s, n) = parse_filename("0xfoo-bar-M-12");
        assert_eq!(a.as_deref(), Some("0xfoo-bar"));
        assert_eq!(s, "medium");
        assert_eq!(n.as_deref(), Some("12"));
    }

    #[test]
    fn parse_filename_falls_back_when_no_match() {
        let (a, s, n) = parse_filename("README");
        assert!(a.is_none());
        assert_eq!(s, "info");
        assert!(n.is_none());
    }

    #[test]
    fn first_h1_extracts_title() {
        let body = "# My finding title\n\nSome body.";
        assert_eq!(first_h1(body).as_deref(), Some("My finding title"));
    }

    #[test]
    fn first_h1_skips_h2() {
        let body = "## not me\n# title\n";
        assert_eq!(first_h1(body).as_deref(), Some("title"));
    }

    #[test]
    fn parse_consolidated_report_pulls_findings() {
        let body = "# Contest report

## [H-01] First high finding

This is the first finding.

It has multiple paragraphs.

## [M-02] Second medium finding

Medium severity body.

## [Q-03] Quality issue

Low quality issue.
";
        let rows = parse_consolidated_report("2024-04-test", body);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].severity, "high");
        assert_eq!(rows[0].title, "First high finding");
        assert!(rows[0].body.contains("multiple paragraphs"));
        assert_eq!(rows[1].severity, "medium");
        assert_eq!(rows[2].severity, "low");
        assert_eq!(rows[0].id, "2024-04-test:H:01");
    }

    #[test]
    fn parse_per_file_finding_extracts_metadata() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("BBB-H-007.md");
        fs::write(
            &path,
            "# Reentrancy in withdraw\n\nThe withdraw function calls...",
        )
        .unwrap();
        let body = fs::read_to_string(&path).unwrap();
        let row = parse_per_file_finding(&path, "2024-04-test", &body).unwrap();
        assert_eq!(row.title, "Reentrancy in withdraw");
        assert_eq!(row.severity, "high");
        assert_eq!(row.auditor.as_deref(), Some("BBB"));
        assert_eq!(row.id, "2024-04-test:high:007");
    }

    #[test]
    fn parse_contest_repo_handles_both_shapes() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        // Shape A: per-finding files in data/
        let data = root.join("data");
        fs::create_dir(&data).unwrap();
        fs::write(data.join("alice-H-001.md"), "# A bug\n\nbody one").unwrap();
        fs::write(data.join("bob-M-002.md"), "# Another bug\n\nbody two").unwrap();
        // Shape B: report.md
        fs::write(
            root.join("report.md"),
            "# Report\n\n## [H-99] Consolidated\n\nbody three\n",
        )
        .unwrap();

        let rows = parse_contest_repo(root, "2024-test");
        assert_eq!(rows.len(), 3);
        let titles: Vec<_> = rows.iter().map(|r| r.title.as_str()).collect();
        assert!(titles.contains(&"A bug"));
        assert!(titles.contains(&"Another bug"));
        assert!(titles.contains(&"Consolidated"));
    }

    #[test]
    fn into_ingest_record_carries_metadata() {
        let row = Code4renaFindingRow {
            id: "2024-04-test:high:001".into(),
            contest: "2024-04-test".into(),
            title: "Reentrancy".into(),
            body: "body".into(),
            severity: "high".into(),
            category: Some("reentrancy".into()),
            auditor: Some("alice".into()),
            finding_url: Some("https://example".into()),
        };
        let ir = row.into_ingest_record();
        assert_eq!(ir.source, "code4rena");
        assert_eq!(ir.kind, "finding");
        assert!(ir.tags.contains(&"severity:high".to_string()));
        assert!(ir.tags.contains(&"category:reentrancy".to_string()));
        assert!(ir.tags.contains(&"auditor:alice".to_string()));
        assert!(ir.tags.contains(&"contest:2024-04-test".to_string()));
        assert_eq!(ir.extra["contest"], "2024-04-test");
    }

    #[test]
    fn empty_repo_yields_zero_rows() {
        let dir = TempDir::new().unwrap();
        let rows = parse_contest_repo(dir.path(), "test");
        assert!(rows.is_empty());
    }

    #[test]
    fn ingester_source_name_and_target_collection_are_stable() {
        let i = Code4renaIngester::new();
        assert_eq!(i.source_name(), "code4rena");
        assert_eq!(i.target_collection(), schema::PUBLIC_FINDINGS);
    }

    #[test]
    fn default_contests_list_is_non_empty() {
        assert!(!DEFAULT_CONTESTS.is_empty());
        for slug in DEFAULT_CONTESTS {
            // sanity: each slug looks like YYYY-MM-name
            assert!(
                slug.starts_with("20"),
                "contest slug {slug} doesn't start with year"
            );
        }
    }

    #[test]
    fn content_hash_id_is_stable_for_same_input() {
        let a = content_hash_id("contest", "body");
        let b = content_hash_id("contest", "body");
        assert_eq!(a, b);
    }

    #[test]
    fn content_hash_id_differs_with_body() {
        let a = content_hash_id("contest", "body1");
        let b = content_hash_id("contest", "body2");
        assert_ne!(a, b);
    }
}
