//! `OpenZeppelin` security-advisories ingester.
//!
//! Pulls published advisories from GitHub's REST endpoint:
//! `GET /repos/OpenZeppelin/openzeppelin-contracts/security-advisories`.
//! One [`IngestRecord`] per advisory. CVE ids, affected version
//! ranges, and severity land in metadata for downstream filtering.
//!
//! Authentication: optional `GITHUB_TOKEN` read from the
//! environment. Unauthenticated requests are subject to the
//! 60-per-hour IP limit; authenticated requests raise this to
//! 5000/hour. The advisories endpoint returns a manageable number
//! of records (~dozens as of 2026-04) so unauthenticated calls
//! are fine for most operators.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use basilisk_embeddings::{EmbeddingInput, EmbeddingProvider};
use basilisk_vector::{schema, VectorStore};
use serde::Deserialize;

use crate::{
    error::IngestError,
    ingester::{IngestOptions, IngestRecord, IngestReport, Ingester},
    normalize::chunk_record,
    state::{IngestState, SourceState},
};

const DEFAULT_BASE: &str = "https://api.github.com";
const OZ_OWNER: &str = "OpenZeppelin";
const OZ_REPO: &str = "openzeppelin-contracts";

/// Shape of one advisory row from the GitHub REST API.
///
/// Only the fields we actually persist are declared; `#[serde(default)]`
/// on every optional field keeps us forward-compatible.
#[derive(Debug, Clone, Deserialize)]
pub struct OzAdvisoryRow {
    pub ghsa_id: String,
    #[serde(default)]
    pub cve_id: Option<String>,
    pub summary: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub html_url: Option<String>,
    #[serde(default)]
    pub vulnerabilities: Vec<OzVulnerability>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OzVulnerability {
    #[serde(default)]
    pub package: Option<OzPackage>,
    #[serde(default)]
    pub vulnerable_version_range: Option<String>,
    #[serde(default)]
    pub patched_versions: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OzPackage {
    #[serde(default)]
    pub ecosystem: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

impl OzAdvisoryRow {
    /// Convert into the source-neutral [`IngestRecord`] shape.
    #[must_use]
    pub fn into_ingest_record(self) -> IngestRecord {
        let mut tags: Vec<String> = Vec::new();
        if let Some(s) = &self.severity {
            tags.push(format!("severity:{}", s.to_ascii_lowercase()));
        }
        if let Some(cve) = &self.cve_id {
            tags.push(format!("cve:{}", cve.to_ascii_lowercase()));
        }
        // Tag affected packages for filter-by-package queries.
        for v in &self.vulnerabilities {
            if let Some(pkg) = v.package.as_ref().and_then(|p| p.name.as_deref()) {
                tags.push(format!("package:{}", pkg.to_ascii_lowercase()));
            }
        }

        let mut extra = serde_json::Map::new();
        extra.insert(
            "ghsa_id".into(),
            serde_json::Value::String(self.ghsa_id.clone()),
        );
        if let Some(v) = self.cve_id {
            extra.insert("cve_id".into(), serde_json::Value::String(v));
        }
        if let Some(v) = self.severity {
            extra.insert("severity".into(), serde_json::Value::String(v));
        }
        if let Some(v) = self.published_at {
            extra.insert("published_at".into(), serde_json::Value::String(v));
        }
        if let Some(v) = self.updated_at {
            extra.insert("updated_at".into(), serde_json::Value::String(v));
        }
        if let Some(v) = self.html_url {
            extra.insert("html_url".into(), serde_json::Value::String(v));
        }
        // Flatten affected packages into a lightweight list so
        // downstream consumers don't have to re-walk the nested
        // structure.
        let affected: Vec<serde_json::Value> = self
            .vulnerabilities
            .iter()
            .map(|v| {
                serde_json::json!({
                    "package": v.package.as_ref().and_then(|p| p.name.clone()),
                    "ecosystem": v.package.as_ref().and_then(|p| p.ecosystem.clone()),
                    "vulnerable_version_range": v.vulnerable_version_range.clone(),
                    "patched_versions": v.patched_versions.clone(),
                })
            })
            .collect();
        if !affected.is_empty() {
            extra.insert("affected".into(), serde_json::Value::Array(affected));
        }

        let body = self.description.unwrap_or_else(|| self.summary.clone());

        IngestRecord {
            source: "openzeppelin".into(),
            source_id: self.ghsa_id,
            kind: "advisory".into(),
            title: self.summary,
            body,
            tags,
            extra: serde_json::Value::Object(extra),
        }
    }
}

/// Configurable GitHub endpoint — swapped for a mock server in
/// tests. Production callers use [`OzAdvisoriesIngester::new`].
#[derive(Clone, Debug)]
pub struct OzAdvisoriesIngester {
    base: String,
    token: Option<String>,
}

impl OzAdvisoriesIngester {
    /// Build against api.github.com. Reads `GITHUB_TOKEN` from the
    /// environment when present; unauthenticated otherwise.
    #[must_use]
    pub fn new() -> Self {
        Self {
            base: DEFAULT_BASE.into(),
            token: std::env::var("GITHUB_TOKEN").ok().filter(|s| !s.is_empty()),
        }
    }

    /// Point at a different base URL. Used by wiremock tests.
    #[must_use]
    pub fn with_base(base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            token: None,
        }
    }

    /// Override the token. Mostly for tests; operators set
    /// `GITHUB_TOKEN` in the environment.
    #[must_use]
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }
}

impl Default for OzAdvisoriesIngester {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Ingester for OzAdvisoriesIngester {
    #[allow(clippy::unnecessary_literal_bound)]
    fn source_name(&self) -> &str {
        "openzeppelin"
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

        let rows = fetch_advisories(&self.base, self.token.as_deref()).await?;
        report.records_scanned = rows.len();

        let spec = schema::advisories(embeddings.identifier(), embeddings.dimensions());
        vector_store.create_collection(spec).await?;

        let max = options.max_records.unwrap_or(usize::MAX);
        let batch_size = embeddings.max_batch_size().min(32);
        let mut latest_ghsa = prior.cursor.clone();

        let mut flat = Vec::new();
        for row in rows.into_iter().take(max) {
            let ghsa = row.ghsa_id.clone();
            // Skip rows we've already ingested on an incremental
            // run. GHSA ids sort lexicographically within the year
            // prefix; that's good enough for "seen this before."
            if options.incremental && prior.cursor.as_deref().is_some_and(|c| ghsa.as_str() <= c) {
                report.records_skipped += 1;
                continue;
            }
            if latest_ghsa.as_deref().is_none_or(|c| ghsa.as_str() > c) {
                latest_ghsa = Some(ghsa);
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
                cb(crate::ingester::IngestProgress {
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
                cursor: latest_ghsa,
                records_ingested: prior.records_ingested
                    + u64::try_from(report.records_new + report.records_updated)
                        .unwrap_or(u64::MAX),
                last_run_unix: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_secs()),
            },
        );
        persistent.save(&state_file)?;

        report.duration = start.elapsed();
        Ok(report)
    }
}

/// Hit `GET {base}/repos/OpenZeppelin/openzeppelin-contracts/security-advisories`.
async fn fetch_advisories(
    base: &str,
    token: Option<&str>,
) -> Result<Vec<OzAdvisoryRow>, IngestError> {
    let url = format!(
        "{}/repos/{OZ_OWNER}/{OZ_REPO}/security-advisories?per_page=100",
        base.trim_end_matches('/'),
    );
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(120))
        .user_agent(concat!("basilisk/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| IngestError::Other(format!("building http client: {e}")))?;
    let mut req = client
        .get(&url)
        .header("accept", "application/vnd.github+json")
        .header("x-github-api-version", "2022-11-28");
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let response = req
        .send()
        .await
        .map_err(|e| IngestError::Source(format!("GET {url}: {e}")))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(IngestError::Source(format!(
            "HTTP {status} from {url}: {body}"
        )));
    }
    let rows: Vec<OzAdvisoryRow> = response
        .json()
        .await
        .map_err(|e| IngestError::Parse(format!("parsing advisories JSON: {e}")))?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row_json() -> serde_json::Value {
        serde_json::json!({
            "ghsa_id": "GHSA-xxxx-yyyy-zzzz",
            "cve_id": "CVE-2024-12345",
            "summary": "Reentrancy in ERC777 hook",
            "description": "The ERC777 tokensReceived hook allows reentrancy in the withdraw flow.",
            "severity": "high",
            "published_at": "2024-06-01T00:00:00Z",
            "updated_at": "2024-06-02T00:00:00Z",
            "html_url": "https://github.com/OpenZeppelin/openzeppelin-contracts/security/advisories/GHSA-xxxx-yyyy-zzzz",
            "vulnerabilities": [
                {
                    "package": {"ecosystem": "npm", "name": "@openzeppelin/contracts"},
                    "vulnerable_version_range": "< 5.0.0",
                    "patched_versions": ">= 5.0.0"
                }
            ]
        })
    }

    #[test]
    fn row_parses_full_shape() {
        let row: OzAdvisoryRow = serde_json::from_value(sample_row_json()).unwrap();
        assert_eq!(row.ghsa_id, "GHSA-xxxx-yyyy-zzzz");
        assert_eq!(row.cve_id.as_deref(), Some("CVE-2024-12345"));
        assert_eq!(row.severity.as_deref(), Some("high"));
        assert_eq!(row.vulnerabilities.len(), 1);
        assert_eq!(
            row.vulnerabilities[0]
                .package
                .as_ref()
                .and_then(|p| p.name.as_deref()),
            Some("@openzeppelin/contracts"),
        );
    }

    #[test]
    fn row_parses_minimal_shape() {
        let minimal = serde_json::json!({
            "ghsa_id": "GHSA-aaa-bbb-ccc",
            "summary": "Terse advisory"
        });
        let row: OzAdvisoryRow = serde_json::from_value(minimal).unwrap();
        assert_eq!(row.ghsa_id, "GHSA-aaa-bbb-ccc");
        assert!(row.cve_id.is_none());
        assert!(row.severity.is_none());
        assert!(row.vulnerabilities.is_empty());
    }

    #[test]
    fn into_ingest_record_builds_tags_and_extras() {
        let row: OzAdvisoryRow = serde_json::from_value(sample_row_json()).unwrap();
        let ir = row.into_ingest_record();
        assert_eq!(ir.source, "openzeppelin");
        assert_eq!(ir.source_id, "GHSA-xxxx-yyyy-zzzz");
        assert_eq!(ir.kind, "advisory");
        assert!(ir.tags.contains(&"severity:high".to_string()));
        assert!(ir.tags.iter().any(|t| t.starts_with("cve:cve-2024-12345")));
        assert!(ir
            .tags
            .iter()
            .any(|t| t.starts_with("package:@openzeppelin/contracts")));
        assert_eq!(
            ir.extra.get("ghsa_id").and_then(|v| v.as_str()),
            Some("GHSA-xxxx-yyyy-zzzz"),
        );
        let affected = ir.extra.get("affected").and_then(|v| v.as_array()).unwrap();
        assert_eq!(affected.len(), 1);
        assert_eq!(
            affected[0].get("vulnerable_version_range"),
            Some(&serde_json::json!("< 5.0.0")),
        );
    }

    #[test]
    fn into_ingest_record_falls_back_to_summary_when_no_description() {
        let minimal = serde_json::json!({
            "ghsa_id": "GHSA-aaa-bbb-ccc",
            "summary": "Short desc"
        });
        let row: OzAdvisoryRow = serde_json::from_value(minimal).unwrap();
        let ir = row.into_ingest_record();
        assert_eq!(ir.title, "Short desc");
        assert_eq!(ir.body, "Short desc");
    }

    #[test]
    fn source_name_and_target_collection_are_stable() {
        let ingester = OzAdvisoriesIngester::new();
        assert_eq!(ingester.source_name(), "openzeppelin");
        assert_eq!(ingester.target_collection(), schema::ADVISORIES);
    }

    #[test]
    fn with_token_overrides_env() {
        let ingester = OzAdvisoriesIngester::with_base("http://localhost").with_token("xyz");
        assert_eq!(ingester.token.as_deref(), Some("xyz"));
    }
}
