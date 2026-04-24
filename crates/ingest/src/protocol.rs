//! Protocol-context ingester — per-engagement documentation.
//!
//! Four source types, one ingester. The operator points at a URL,
//! a PDF, a local markdown/text file, or a GitHub docs directory;
//! the ingester extracts text, chunks into overlapping windows,
//! and upserts into the `protocols` collection with
//! `engagement_id` stamped on every record so retrieval filters
//! cleanly by engagement.
//!
//! Upsert semantics: record ids are
//! `sha256(engagement_id | source_descriptor | chunk_index)`, so
//! re-ingesting the same content for the same engagement produces
//! the same ids and becomes a no-op upsert. Content changes
//! produce different chunks with different ids — effectively a
//! full re-ingest for that source.
//!
//! Sources (via [`ProtocolSource`]):
//!  - URL — fetched with reqwest, main content extracted via
//!    [`readability`].
//!  - PDF — text extracted via [`pdf_extract`]. Heavy-table /
//!    image-only pages surface as empty extracted text; we log a
//!    warning and keep going.
//!  - File — markdown / plain text read straight from disk;
//!    markdown files split along H1/H2/H3 boundaries via
//!    [`pulldown_cmark`].
//!  - GitHub dir — shallow-clones the repo, walks a given subdir,
//!    ingests each `.md` file in turn (same splitter as File).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use basilisk_embeddings::{EmbeddingInput, EmbeddingProvider};
use basilisk_git::{CloneStrategy, FetchOptions, RepoCache};
use basilisk_github::GithubClient;
use basilisk_vector::{schema, Metadata, VectorStore};
use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};
use sha2::{Digest, Sha256};

use crate::{
    error::IngestError,
    ingester::{IngestOptions, IngestReport, Ingester},
};

/// Target tokens per chunk. Conservative vs the common 8k-16k
/// model input limits — leaves headroom for the embedding call
/// metadata and keeps retrieval snippets readable.
const CHUNK_TOKEN_TARGET: usize = 1000;
/// Overlap between consecutive chunks (tokens). Preserves enough
/// context at chunk boundaries that a query matching the edge of
/// one chunk also surfaces the next.
const CHUNK_TOKEN_OVERLAP: usize = 100;
const TOKEN_TO_BYTE_RATIO: usize = 4;

/// Which source the ingester pulls from.
#[derive(Debug, Clone)]
pub enum ProtocolSource {
    Url(String),
    Pdf(PathBuf),
    /// Local markdown/text file. Extension drives the splitter:
    /// `.md` / `.markdown` → pulldown-cmark header split; anything
    /// else → plain token-window split.
    File(PathBuf),
    /// Shallow-clone the given repo and ingest every `.md` under
    /// `subdir` (relative path). `subdir=None` means the repo root.
    GithubDir {
        owner: String,
        repo: String,
        subdir: Option<PathBuf>,
    },
}

impl ProtocolSource {
    /// Short human descriptor — used in record ids and the
    /// `source_pointer` metadata field.
    #[must_use]
    pub fn descriptor(&self) -> String {
        match self {
            Self::Url(u) => format!("url:{u}"),
            Self::Pdf(p) => format!("pdf:{}", p.display()),
            Self::File(p) => format!("file:{}", p.display()),
            Self::GithubDir {
                owner,
                repo,
                subdir,
            } => {
                let path = subdir
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                format!("github:{owner}/{repo}:{path}")
            }
        }
    }
}

/// The protocol-context [`Ingester`].
pub struct ProtocolIngester {
    engagement_id: String,
    source: ProtocolSource,
    repo_cache: Option<Arc<RepoCache>>,
    github: Option<GithubClient>,
}

impl ProtocolIngester {
    /// `engagement_id` scopes every record so `SearchFilters`
    /// with a matching engagement id returns only this engagement's
    /// docs.
    ///
    /// `repo_cache` is required only for
    /// [`ProtocolSource::GithubDir`]; other sources pass `None`.
    pub fn new(
        engagement_id: impl Into<String>,
        source: ProtocolSource,
        repo_cache: Option<Arc<RepoCache>>,
    ) -> Self {
        Self {
            engagement_id: engagement_id.into(),
            source,
            repo_cache,
            github: None,
        }
    }

    /// Wire a `GithubClient` used for default-branch lookup when
    /// the source is [`ProtocolSource::GithubDir`]. Without it,
    /// default-branch resolution isn't possible — the clone will
    /// fail for repos whose default isn't `master`.
    #[must_use]
    pub fn with_github(mut self, client: GithubClient) -> Self {
        self.github = Some(client);
        self
    }

    pub fn engagement_id(&self) -> &str {
        &self.engagement_id
    }
}

#[async_trait]
impl Ingester for ProtocolIngester {
    #[allow(clippy::unnecessary_literal_bound)]
    fn source_name(&self) -> &str {
        "protocol"
    }

    fn target_collection(&self) -> &str {
        schema::PROTOCOLS
    }

    async fn ingest(
        &self,
        vector_store: Arc<dyn VectorStore>,
        embeddings: Arc<dyn EmbeddingProvider>,
        _options: IngestOptions,
    ) -> Result<IngestReport, IngestError> {
        let start = Instant::now();
        let mut report = IngestReport::empty(self.source_name());

        let chunks = self.collect_chunks().await?;
        report.records_scanned = chunks.len();
        if chunks.is_empty() {
            report.duration = start.elapsed();
            return Ok(report);
        }

        let spec = schema::protocols(embeddings.identifier(), embeddings.dimensions());
        vector_store.create_collection(spec).await?;

        let batch_size = embeddings.max_batch_size().min(32);
        let descriptor = self.source.descriptor();

        for batch in chunks.chunks(batch_size) {
            let inputs: Vec<EmbeddingInput> = batch
                .iter()
                .map(|c| EmbeddingInput::document(&c.text))
                .collect();
            let vectors = embeddings.embed(&inputs).await?;
            report.embedding_tokens_used += vectors
                .iter()
                .map(|v| u64::from(v.input_tokens))
                .sum::<u64>();

            let mut records = Vec::with_capacity(batch.len());
            for (chunk, embedding) in batch.iter().zip(vectors) {
                records.push(basilisk_vector::Record {
                    id: chunk.id.clone(),
                    vector: embedding.vector,
                    text: chunk.text.clone(),
                    metadata: Metadata {
                        source: "protocol".into(),
                        source_id: descriptor.clone(),
                        kind: "doc".into(),
                        tags: vec![format!("engagement:{}", self.engagement_id)],
                        engagement_id: Some(self.engagement_id.clone()),
                        extra: chunk.extra.clone(),
                        indexed_at: SystemTime::now(),
                    },
                });
            }
            let stats = vector_store
                .upsert(self.target_collection(), records)
                .await?;
            report.records_new += stats.inserted;
            report.records_updated += stats.updated;
        }

        report.duration = start.elapsed();
        // Stamp "last run" cursor into the shared state so
        // `knowledge stats` reports progress.
        let mut state = crate::state::IngestState::load(&crate::state::default_state_path())?;
        state.set(
            format!("protocol:{}", self.engagement_id),
            crate::state::SourceState {
                cursor: Some(descriptor.clone()),
                records_ingested: u64::try_from(report.records_new + report.records_updated)
                    .unwrap_or(u64::MAX),
                last_run_unix: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_secs()),
            },
        );
        state.save(&crate::state::default_state_path())?;

        Ok(report)
    }
}

impl ProtocolIngester {
    async fn collect_chunks(&self) -> Result<Vec<ProtocolChunk>, IngestError> {
        match &self.source {
            ProtocolSource::Url(url) => self.chunks_from_url(url).await,
            ProtocolSource::Pdf(path) => self.chunks_from_pdf(path),
            ProtocolSource::File(path) => self.chunks_from_file(path),
            ProtocolSource::GithubDir {
                owner,
                repo,
                subdir,
            } => {
                self.chunks_from_github(owner, repo, subdir.as_deref())
                    .await
            }
        }
    }

    async fn chunks_from_url(&self, url: &str) -> Result<Vec<ProtocolChunk>, IngestError> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(60))
            .user_agent(concat!("basilisk/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| IngestError::Other(format!("building http client: {e}")))?;
        let body = client
            .get(url)
            .send()
            .await
            .map_err(|e| IngestError::Source(format!("GET {url}: {e}")))?
            .error_for_status()
            .map_err(|e| IngestError::Source(e.to_string()))?
            .text()
            .await
            .map_err(|e| IngestError::Source(format!("reading {url}: {e}")))?;

        let parsed_url = url
            .parse()
            .map_err(|e: url::ParseError| IngestError::Parse(e.to_string()))?;
        let product = readability::extractor::extract(&mut body.as_bytes(), &parsed_url)
            .map_err(|e| IngestError::Parse(format!("readability: {e}")))?;
        let text = if product.text.trim().is_empty() {
            body
        } else {
            product.text
        };
        Ok(token_window_chunks(
            &self.engagement_id,
            &self.source.descriptor(),
            &text,
            CHUNK_TOKEN_TARGET,
            CHUNK_TOKEN_OVERLAP,
        ))
    }

    fn chunks_from_pdf(&self, path: &Path) -> Result<Vec<ProtocolChunk>, IngestError> {
        if !path.exists() {
            return Err(IngestError::Source(format!(
                "PDF not found: {}",
                path.display()
            )));
        }
        let text = pdf_extract::extract_text(path)
            .map_err(|e| IngestError::Parse(format!("pdf-extract: {e}")))?;
        if text.trim().is_empty() {
            tracing::warn!(
                path = %path.display(),
                "pdf-extract returned empty text — likely image-only / scanned PDF; skipping",
            );
            return Ok(Vec::new());
        }
        Ok(token_window_chunks(
            &self.engagement_id,
            &self.source.descriptor(),
            &text,
            CHUNK_TOKEN_TARGET,
            CHUNK_TOKEN_OVERLAP,
        ))
    }

    fn chunks_from_file(&self, path: &Path) -> Result<Vec<ProtocolChunk>, IngestError> {
        if !path.exists() {
            return Err(IngestError::Source(format!(
                "file not found: {}",
                path.display()
            )));
        }
        let text = std::fs::read_to_string(path)?;
        let ext = path
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .map(str::to_ascii_lowercase);
        let chunks = match ext.as_deref() {
            Some("md" | "markdown") => markdown_sections(
                &self.engagement_id,
                &self.source.descriptor(),
                &text,
                CHUNK_TOKEN_TARGET,
            ),
            _ => token_window_chunks(
                &self.engagement_id,
                &self.source.descriptor(),
                &text,
                CHUNK_TOKEN_TARGET,
                CHUNK_TOKEN_OVERLAP,
            ),
        };
        Ok(chunks)
    }

    async fn chunks_from_github(
        &self,
        owner: &str,
        repo: &str,
        subdir: Option<&Path>,
    ) -> Result<Vec<ProtocolChunk>, IngestError> {
        let cache = self.repo_cache.as_ref().ok_or_else(|| {
            IngestError::Other(
                "ProtocolSource::GithubDir requires a RepoCache — pass one to ProtocolIngester::new"
                    .into(),
            )
        })?;
        let fetched = cache
            .fetch(
                owner,
                repo,
                None,
                FetchOptions {
                    strategy: CloneStrategy::Shallow,
                    force_refresh: false,
                    github: self.github.clone(),
                },
            )
            .await
            .map_err(|e| IngestError::Source(format!("clone {owner}/{repo}: {e}")))?;
        let walk_root = match subdir {
            Some(s) => fetched.working_tree.join(s),
            None => fetched.working_tree,
        };
        if !walk_root.exists() {
            return Err(IngestError::Source(format!(
                "subdir not found in {owner}/{repo}: {}",
                walk_root.display(),
            )));
        }

        let mut out = Vec::new();
        for entry in walkdir_markdown(&walk_root)? {
            let text = std::fs::read_to_string(&entry)?;
            // Update descriptor per-file so ids are stable per
            // (engagement, file).
            let file_desc = format!(
                "github:{owner}/{repo}:{}",
                entry.strip_prefix(&walk_root).unwrap_or(&entry).display(),
            );
            let chunks =
                markdown_sections(&self.engagement_id, &file_desc, &text, CHUNK_TOKEN_TARGET);
            out.extend(chunks);
        }
        Ok(out)
    }
}

/// Walk a directory and return every `.md` file (recursive).
/// Plain depth-first using `std::fs::read_dir` — no walkdir dep.
fn walkdir_markdown(root: &Path) -> Result<Vec<PathBuf>, IngestError> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let ext = path
                .extension()
                .and_then(std::ffi::OsStr::to_str)
                .map(str::to_ascii_lowercase);
            if matches!(ext.as_deref(), Some("md" | "markdown")) {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Intermediate chunk passed to the upsert path.
#[derive(Debug, Clone, PartialEq)]
struct ProtocolChunk {
    id: String,
    text: String,
    extra: serde_json::Value,
}

/// Sliding-window chunker used for URL / PDF / non-markdown file
/// bodies. Byte-based windows sized to `target_tokens * 4`, with
/// `overlap_tokens * 4` carried forward into the next chunk.
fn token_window_chunks(
    engagement_id: &str,
    descriptor: &str,
    text: &str,
    target_tokens: usize,
    overlap_tokens: usize,
) -> Vec<ProtocolChunk> {
    let window = target_tokens.saturating_mul(TOKEN_TO_BYTE_RATIO);
    let overlap = overlap_tokens.saturating_mul(TOKEN_TO_BYTE_RATIO);
    if window == 0 || text.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut idx = 0;
    let mut chunk_idx = 0;
    while idx < bytes.len() {
        let mut end = (idx + window).min(bytes.len());
        // Snap to a UTF-8 boundary so we never cut mid-char.
        while end < bytes.len() && !text.is_char_boundary(end) {
            end -= 1;
        }
        let slice = &text[idx..end];
        let id = chunk_id(engagement_id, descriptor, chunk_idx);
        out.push(ProtocolChunk {
            id,
            text: slice.to_string(),
            extra: serde_json::json!({
                "source_pointer": descriptor,
                "chunk_index": chunk_idx,
            }),
        });
        if end >= bytes.len() {
            break;
        }
        // Next window starts at `end - overlap`, clamped and
        // snapped to a char boundary.
        let mut next = end.saturating_sub(overlap);
        while next > 0 && !text.is_char_boundary(next) {
            next -= 1;
        }
        idx = next.max(idx + 1); // guarantee progress
        chunk_idx += 1;
    }
    out
}

/// Markdown-aware chunker. Splits on H1/H2/H3 boundaries; if a
/// section is itself too large, falls back to the token-window
/// chunker for that section.
fn markdown_sections(
    engagement_id: &str,
    descriptor: &str,
    text: &str,
    target_tokens: usize,
) -> Vec<ProtocolChunk> {
    let parser = Parser::new(text);
    let mut sections: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_heading = false;

    for event in parser {
        match event {
            Event::Start(Tag::Heading {
                level: HeadingLevel::H1 | HeadingLevel::H2 | HeadingLevel::H3,
                ..
            }) => {
                if !current.trim().is_empty() {
                    sections.push(std::mem::take(&mut current));
                }
                in_heading = true;
                current.push('\n');
            }
            Event::End(TagEnd::Heading(_)) => {
                in_heading = false;
                current.push('\n');
            }
            Event::Text(t) | Event::Code(t) => {
                if in_heading {
                    current.push_str("# ");
                    current.push_str(&t);
                } else {
                    current.push_str(&t);
                }
            }
            Event::SoftBreak | Event::HardBreak => current.push('\n'),
            Event::End(TagEnd::Paragraph) => current.push_str("\n\n"),
            _ => {}
        }
    }
    if !current.trim().is_empty() {
        sections.push(current);
    }

    // Within each section, if it exceeds the target, fall back to
    // the token-window chunker. Otherwise emit one ProtocolChunk.
    let mut out = Vec::new();
    let max_bytes = target_tokens.saturating_mul(TOKEN_TO_BYTE_RATIO);
    let mut idx = 0;
    for section in sections {
        if section.len() <= max_bytes {
            let id = chunk_id(engagement_id, descriptor, idx);
            out.push(ProtocolChunk {
                id,
                text: section,
                extra: serde_json::json!({
                    "source_pointer": descriptor,
                    "chunk_index": idx,
                }),
            });
            idx += 1;
        } else {
            for window in token_window_chunks(
                engagement_id,
                descriptor,
                &section,
                target_tokens,
                CHUNK_TOKEN_OVERLAP,
            ) {
                let id = chunk_id(engagement_id, descriptor, idx);
                out.push(ProtocolChunk {
                    id,
                    text: window.text,
                    extra: serde_json::json!({
                        "source_pointer": descriptor,
                        "chunk_index": idx,
                    }),
                });
                idx += 1;
            }
        }
    }
    out
}

/// Deterministic chunk id:
/// `sha256(engagement_id | descriptor | chunk_idx)`.
fn chunk_id(engagement_id: &str, descriptor: &str, chunk_idx: usize) -> String {
    let mut h = Sha256::new();
    h.update(engagement_id.as_bytes());
    h.update(b"|");
    h.update(descriptor.as_bytes());
    h.update(b"|");
    h.update(chunk_idx.to_le_bytes());
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_shape_matches_source_variant() {
        assert_eq!(
            ProtocolSource::Url("https://x".into()).descriptor(),
            "url:https://x",
        );
        assert!(ProtocolSource::Pdf("/tmp/a.pdf".into())
            .descriptor()
            .starts_with("pdf:"));
        assert!(ProtocolSource::File("a.md".into())
            .descriptor()
            .starts_with("file:"));
        assert_eq!(
            ProtocolSource::GithubDir {
                owner: "o".into(),
                repo: "r".into(),
                subdir: Some("docs".into()),
            }
            .descriptor(),
            "github:o/r:docs",
        );
    }

    #[test]
    fn token_window_chunks_emits_overlapping_chunks_for_long_text() {
        // 10k bytes; target 1000 tokens * 4 bytes = 4000-byte
        // window, overlap 100*4=400. Expect 3 chunks with small
        // overlap in bytes.
        let text = "a".repeat(10_000);
        let chunks = token_window_chunks("eng-1", "file:/x", &text, 1000, 100);
        assert!(chunks.len() >= 3, "got {}", chunks.len());
        // Ids are monotonic and deterministic.
        let ids: Vec<_> = chunks.iter().map(|c| c.id.clone()).collect();
        assert_eq!(
            ids.iter().collect::<std::collections::BTreeSet<_>>().len(),
            chunks.len()
        );
    }

    #[test]
    fn token_window_chunks_respects_utf8_boundaries() {
        // Each emoji is 4 bytes; random splits would panic. Window
        // chosen so the splitter MUST cut mid-text.
        let text = "🦀".repeat(500); // 2000 bytes
        let chunks = token_window_chunks("eng-1", "file:/x", &text, 100, 10);
        assert!(!chunks.is_empty());
    }

    #[test]
    fn token_window_chunks_empty_text_emits_nothing() {
        let chunks = token_window_chunks("eng-1", "file:/x", "", 1000, 100);
        assert!(chunks.is_empty());
    }

    #[test]
    fn markdown_sections_splits_on_h1_h2_h3() {
        let text = "# First\nBody 1\n\n## Second\nBody 2\n\n### Third\nBody 3\n";
        let chunks = markdown_sections("eng-1", "file:/x", text, 10_000);
        assert!(chunks.len() >= 3, "got {} chunks", chunks.len());
    }

    #[test]
    fn markdown_sections_ignores_h4_plus() {
        // H4 should not split; it stays with its parent section.
        let text = "# Parent\nA\n\n#### Sub\nB\n\n## Sibling\nC\n";
        let chunks = markdown_sections("eng-1", "file:/x", text, 10_000);
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn markdown_sections_large_section_falls_back_to_window_split() {
        // One H1 section of 10_000 bytes, target 100 tokens = 400
        // bytes. Should emit multiple window-based chunks.
        let big = "a".repeat(10_000);
        let text = format!("# Header\n\n{big}");
        let chunks = markdown_sections("eng-1", "file:/x", &text, 100);
        assert!(chunks.len() >= 5, "got {}", chunks.len());
    }

    #[test]
    fn chunk_id_is_deterministic() {
        let a = chunk_id("eng-1", "file:/x", 0);
        let b = chunk_id("eng-1", "file:/x", 0);
        assert_eq!(a, b);
    }

    #[test]
    fn chunk_id_changes_with_chunk_index() {
        let a = chunk_id("eng-1", "file:/x", 0);
        let b = chunk_id("eng-1", "file:/x", 1);
        assert_ne!(a, b);
    }

    #[test]
    fn chunk_id_changes_with_engagement() {
        let a = chunk_id("eng-1", "file:/x", 0);
        let b = chunk_id("eng-2", "file:/x", 0);
        assert_ne!(a, b);
    }

    #[test]
    fn walkdir_markdown_returns_sorted_md_files_recursive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "A").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.md"), "B").unwrap();
        std::fs::write(dir.path().join("sub/c.markdown"), "C").unwrap();
        std::fs::write(dir.path().join("sub/d.txt"), "D").unwrap();
        let files = walkdir_markdown(dir.path()).unwrap();
        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["a.md", "b.md", "c.markdown"]);
    }

    #[test]
    fn chunks_from_file_markdown_splits_on_headings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.md");
        std::fs::write(
            &path,
            "# Intro\nHello world\n\n## Details\nMore words here\n\n## Refs\nA link\n",
        )
        .unwrap();
        let ing = ProtocolIngester::new("eng-1", ProtocolSource::File(path.clone()), None);
        let chunks = ing.chunks_from_file(&path).unwrap();
        assert!(chunks.len() >= 3);
    }

    #[test]
    fn chunks_from_file_plain_text_uses_window_chunker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "a".repeat(10_000)).unwrap();
        let ing = ProtocolIngester::new("eng-1", ProtocolSource::File(path.clone()), None);
        let chunks = ing.chunks_from_file(&path).unwrap();
        assert!(chunks.len() >= 2);
    }

    #[test]
    fn chunks_from_file_missing_path_is_source_error() {
        let path = std::path::PathBuf::from("/tmp/missing-protocol-file.md");
        let ing = ProtocolIngester::new("eng-1", ProtocolSource::File(path.clone()), None);
        let err = ing.chunks_from_file(&path).unwrap_err();
        assert!(matches!(err, IngestError::Source(_)));
    }
}
