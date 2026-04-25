//! SWC registry ingester.
//!
//! [SWC](https://swcregistry.io) (Smart Contract Weakness
//! Classification) publishes one markdown file per weakness under
//! `entries/SWC-NNN.md` at
//! `github.com/SmartContractSecurity/SWC-registry`. Each file has
//! a small YAML-bullet front-matter (name, number, relations,
//! markdown) followed by `## Description`, `## Remediation`, and
//! example Solidity code fences.
//!
//! Ingestion:
//!  - Shallow-clone the repo via [`basilisk_git::RepoCache`].
//!  - Walk `entries/` for `SWC-*.md` files.
//!  - Parse each with [`parse_swc_entry`]; emit one
//!    [`IngestRecord`] per weakness.
//!  - Upsert into the `advisories` collection.
//!
//! SWC is static (no new entries since 2020-ish) so this ingester
//! is effectively one-shot. `incremental=true` still passes the
//! state cursor, but it's rarely meaningful here.

use std::path::Path;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use basilisk_core::GitRef;
use basilisk_embeddings::{EmbeddingInput, EmbeddingProvider};
use basilisk_git::{CloneStrategy, FetchOptions, RepoCache};
use basilisk_github::GithubClient;
use basilisk_vector::{schema, VectorStore};

use crate::{
    error::IngestError,
    ingester::{IngestOptions, IngestRecord, IngestReport, Ingester},
    normalize::chunk_record,
    state::{IngestState, SourceState},
};

const SWC_OWNER: &str = "SmartContractSecurity";
const SWC_REPO: &str = "SWC-registry";

/// One parsed SWC entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwcEntry {
    /// `"SWC-100"` etc. Extracted from the filename.
    pub number: String,
    /// Human title from front-matter (e.g. "Function Default
    /// Visibility").
    pub name: String,
    /// Full body — everything after front-matter, including
    /// Description / Remediation / code fences.
    pub body: String,
}

/// Parse a single `SWC-NNN.md` file's content.
///
/// SWC front-matter is idiosyncratic — a YAML-like bulleted list
/// between `---` delimiters, not standard TOML or YAML. We do a
/// forgiving line-scan rather than pull a YAML crate for this one
/// file format.
#[must_use]
pub fn parse_swc_entry(number: &str, content: &str) -> SwcEntry {
    let (front, body) = split_front_matter(content);
    let name =
        extract_field(front.as_deref().unwrap_or(""), "name").unwrap_or_else(|| number.to_string());
    SwcEntry {
        number: number.to_string(),
        name,
        body: body.trim().to_string(),
    }
}

/// Split the content into `(front_matter, body)`. Front-matter is
/// the region between the first pair of `---` lines at the start
/// of the file. `None` front when no front-matter is present.
fn split_front_matter(content: &str) -> (Option<String>, String) {
    let mut lines = content.lines();
    let first = lines.next();
    if first.is_none_or(|l| l.trim() != "---") {
        return (None, content.to_string());
    }
    let mut front = String::new();
    for line in lines.by_ref() {
        if line.trim() == "---" {
            let body: String = lines.collect::<Vec<_>>().join("\n");
            return (Some(front), body);
        }
        front.push_str(line);
        front.push('\n');
    }
    // No closing delimiter — treat everything as body.
    (None, content.to_string())
}

/// Extract a field value from the bullet-list front-matter. Matches
/// lines like `- name: Value` or `-   name:    Value`. Returns the
/// first match; SWC entries don't have duplicate keys in practice.
fn extract_field(front: &str, field: &str) -> Option<String> {
    for line in front.lines() {
        let trimmed = line.trim_start_matches(|c: char| c == '-' || c.is_whitespace());
        if let Some(rest) = trimmed.strip_prefix(&format!("{field}:")) {
            let value = rest.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Build an [`IngestRecord`] from an [`SwcEntry`].
#[must_use]
pub fn entry_into_ingest_record(entry: SwcEntry) -> IngestRecord {
    let mut extra = serde_json::Map::new();
    extra.insert(
        "swc_id".into(),
        serde_json::Value::String(entry.number.clone()),
    );
    IngestRecord {
        source: "swc".into(),
        source_id: entry.number.clone(),
        kind: "advisory".into(),
        title: entry.name,
        body: entry.body,
        tags: vec![format!("swc:{}", entry.number.to_ascii_lowercase())],
        extra: serde_json::Value::Object(extra),
    }
}

/// The SWC [`Ingester`].
pub struct SwcIngester {
    cache: Arc<RepoCache>,
    github: Option<GithubClient>,
}

impl SwcIngester {
    pub fn new(cache: Arc<RepoCache>) -> Self {
        Self {
            cache,
            github: None,
        }
    }

    /// Provide a `GithubClient` used for default-branch lookup.
    /// Optional — the ingester otherwise falls back to pinning
    /// `refs/heads/master` (SWC-registry's historical default,
    /// and the repo is effectively frozen).
    #[must_use]
    pub fn with_github(mut self, client: GithubClient) -> Self {
        self.github = Some(client);
        self
    }
}

#[async_trait]
impl Ingester for SwcIngester {
    #[allow(clippy::unnecessary_literal_bound)]
    fn source_name(&self) -> &str {
        "swc"
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

        let fetched = fetch_swc_registry(&self.cache).await?;

        // SWC-registry reorganised its layout: files used to live at
        // `entries/SWC-NNN.md`, they now live at
        // `entries/docs/SWC-NNN.md`. Try the new layout first; fall
        // back to the old one for pinned historic refs.
        let entries_new = fetched.working_tree.join("entries").join("docs");
        let entries_old = fetched.working_tree.join("entries");
        let swc_files = if entries_new.exists() {
            collect_swc_files(&entries_new)?
        } else {
            collect_swc_files(&entries_old)?
        };
        report.records_scanned = swc_files.len();
        if let Some(max) = options.max_records {
            if swc_files.len() > max {
                // Cap by truncation but keep the scan count honest.
            }
        }

        // Fire an early progress event so the operator sees
        // "scanning done, embedding about to start" instead of a
        // frozen-looking line during what may be a slow first call
        // (e.g. Ollama loading a cold model).
        if let Some(cb) = &options.progress {
            cb(crate::ingester::IngestProgress {
                records_scanned: report.records_scanned,
                records_upserted: 0,
                records_skipped: 0,
                embedding_tokens_used: 0,
            });
        }

        let spec = schema::advisories(embeddings.identifier(), embeddings.dimensions());
        vector_store.create_collection(spec).await?;

        let max_to_ingest = options.max_records.unwrap_or(usize::MAX);
        // Token-aware batching — see `crate::batch::pack_batches`.

        let mut all_chunks = Vec::new();
        for (idx, (number, path)) in swc_files.iter().enumerate() {
            if idx >= max_to_ingest {
                break;
            }
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(e) => {
                    report.errors.push((number.clone(), format!("read: {e}")));
                    continue;
                }
            };
            let entry = parse_swc_entry(number, &content);
            let ir = entry_into_ingest_record(entry);
            let chunks = chunk_record(&ir, embeddings.max_tokens_per_input());
            all_chunks.extend(chunks);
        }

        for batch in crate::batch::pack_batches(&all_chunks, &*embeddings) {
            if batch.is_empty() {
                continue;
            }
            let inputs: Vec<EmbeddingInput> = batch
                .iter()
                .map(|c| EmbeddingInput::document(&c.text))
                .collect();
            let vectors = embeddings.embed(&inputs).await?;
            if vectors.len() != batch.len() {
                return Err(IngestError::Other(format!(
                    "embedding provider returned {} vectors for {} inputs",
                    vectors.len(),
                    batch.len(),
                )));
            }
            report.embedding_tokens_used += vectors
                .iter()
                .map(|v| u64::from(v.input_tokens))
                .sum::<u64>();

            let records: Vec<_> = batch
                .iter()
                .cloned()
                .zip(vectors)
                .map(|(norm, emb)| norm.into_record(emb.vector))
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

        // Persist state.
        let mut persistent = IngestState::load(&crate::state::default_state_path())?;
        persistent.set(
            self.source_name(),
            SourceState {
                cursor: Some(fetched.commit_sha.clone()),
                records_ingested: u64::try_from(report.records_new + report.records_updated)
                    .unwrap_or(u64::MAX),
                last_run_unix: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_secs()),
            },
        );
        persistent.save(&crate::state::default_state_path())?;

        report.duration = start.elapsed();
        Ok(report)
    }
}

/// Shallow-clone SWC-registry, trying `master` first and falling
/// back to `main`. SWC-registry is effectively frozen (no new
/// entries since ~2021) and its historical default is `master`.
/// Pinning the ref means we don't need a `GithubClient` for
/// default-branch lookup — so a bad `GITHUB_TOKEN` can't break
/// this path.
async fn fetch_swc_registry(cache: &RepoCache) -> Result<basilisk_git::FetchedRepo, IngestError> {
    let shallow = FetchOptions {
        strategy: CloneStrategy::Shallow,
        force_refresh: false,
        github: None,
    };
    match cache
        .fetch(
            SWC_OWNER,
            SWC_REPO,
            Some(GitRef::Branch("master".into())),
            shallow.clone(),
        )
        .await
    {
        Ok(r) => Ok(r),
        Err(basilisk_git::GitError::RefNotFound { .. }) => cache
            .fetch(
                SWC_OWNER,
                SWC_REPO,
                Some(GitRef::Branch("main".into())),
                shallow,
            )
            .await
            .map_err(|e| IngestError::Source(format!("cloning SWC-registry: {e}"))),
        Err(e) => Err(IngestError::Source(format!("cloning SWC-registry: {e}"))),
    }
}

/// Walk `entries/` for `SWC-*.md` files. Returns `(number, path)`
/// tuples; `number` is derived from the filename stem.
fn collect_swc_files(dir: &Path) -> Result<Vec<(String, std::path::PathBuf)>, IngestError> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Err(IngestError::Source(format!(
            "SWC entries dir not found: {}",
            dir.display()
        )));
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(std::ffi::OsStr::to_str) != Some("md") {
            continue;
        }
        let stem = match path.file_stem().and_then(std::ffi::OsStr::to_str) {
            Some(s) if s.starts_with("SWC-") => s.to_string(),
            _ => continue,
        };
        out.push((stem, path));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_ENTRY: &str = r"---
- name: Function Default Visibility
- number: SWC-100
- relations:
  - CWE-710
- platforms:
  - Solidity
---
## Title
Function Default Visibility

## Description
Functions that do not have a function visibility type specified are `public` by default.

## Remediation
Always explicitly mark functions.

```solidity
function foo() public { }
```
";

    #[test]
    fn parse_swc_entry_extracts_name_and_body() {
        let entry = parse_swc_entry("SWC-100", SAMPLE_ENTRY);
        assert_eq!(entry.number, "SWC-100");
        assert_eq!(entry.name, "Function Default Visibility");
        assert!(entry.body.contains("## Description"));
        assert!(entry.body.contains("## Remediation"));
        assert!(entry.body.contains("```solidity"));
    }

    #[test]
    fn parse_swc_entry_survives_missing_front_matter() {
        let entry = parse_swc_entry("SWC-101", "## Description\nSomething.\n");
        assert_eq!(entry.number, "SWC-101");
        // Fallback: name defaults to the number.
        assert_eq!(entry.name, "SWC-101");
        assert!(entry.body.contains("Description"));
    }

    #[test]
    fn split_front_matter_without_closing_delim_returns_everything_as_body() {
        let (front, body) = split_front_matter("---\nunclosed\nmore body\n");
        assert!(front.is_none());
        assert!(body.contains("unclosed"));
    }

    #[test]
    fn extract_field_finds_name_in_bulleted_front_matter() {
        let front = "- name: Hello\n- number: SWC-999\n";
        assert_eq!(extract_field(front, "name"), Some("Hello".into()));
        assert_eq!(extract_field(front, "number"), Some("SWC-999".into()));
        assert!(extract_field(front, "missing").is_none());
    }

    #[test]
    fn entry_into_ingest_record_tags_with_swc_id() {
        let entry = SwcEntry {
            number: "SWC-100".into(),
            name: "Function Default Visibility".into(),
            body: "text".into(),
        };
        let ir = entry_into_ingest_record(entry);
        assert_eq!(ir.source, "swc");
        assert_eq!(ir.source_id, "SWC-100");
        assert_eq!(ir.kind, "advisory");
        assert_eq!(ir.title, "Function Default Visibility");
        assert!(ir.tags.contains(&"swc:swc-100".to_string()));
        assert_eq!(
            ir.extra.get("swc_id").and_then(|v| v.as_str()),
            Some("SWC-100"),
        );
    }

    #[test]
    fn collect_swc_files_picks_md_files_sorted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SWC-103.md"), "a").unwrap();
        std::fs::write(dir.path().join("SWC-100.md"), "a").unwrap();
        std::fs::write(dir.path().join("SWC-101.md"), "a").unwrap();
        // Non-md files ignored.
        std::fs::write(dir.path().join("README.txt"), "a").unwrap();
        // Non-SWC-prefixed md ignored.
        std::fs::write(dir.path().join("other.md"), "a").unwrap();

        let out = collect_swc_files(dir.path()).unwrap();
        let nums: Vec<_> = out.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(nums, vec!["SWC-100", "SWC-101", "SWC-103"]);
    }

    #[test]
    fn collect_swc_files_missing_dir_errors_cleanly() {
        let err =
            collect_swc_files(std::path::Path::new("/tmp/nonexistent-path-basilisk")).unwrap_err();
        assert!(matches!(err, IngestError::Source(_)));
    }
}
