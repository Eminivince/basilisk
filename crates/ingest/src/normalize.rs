//! Turn one [`IngestRecord`] into one-or-more
//! [`basilisk_vector::Record`]s, chunking along semantic
//! boundaries when the body exceeds the embedding model's
//! per-input token limit.
//!
//! Chunking philosophy: every [`Ingester`] produces one logical
//! record per finding (or advisory, or doc page). When that
//! record's body fits within the provider's per-input token limit
//! (~8-16k tokens for shipped providers), we emit one
//! `Record`. When it exceeds the limit, we split along the
//! nearest paragraph or heading boundary; the resulting pieces
//! share a `parent_id` and carry `chunk_index`/`total_chunks`
//! in their metadata so retrieval can re-assemble the context if
//! needed.
//!
//! Token estimation: bytes/4 heuristic (matches the embeddings
//! crate's `estimate_tokens`). Conservative: over-counts dense
//! code, which means we split earlier rather than later — a
//! slightly-smaller chunk is always fine; a too-big chunk gets
//! rejected by the provider.
//!
//! [`Ingester`]: crate::ingester::Ingester

use basilisk_vector::{Metadata, Record};
use sha2::{Digest, Sha256};

use crate::ingester::IngestRecord;

/// Normalised pre-vector record: text + metadata + deterministic
/// id, ready to be paired with an embedding vector by the caller.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedRecord {
    pub id: String,
    pub text: String,
    pub metadata: Metadata,
}

impl NormalizedRecord {
    /// Pair with a vector to produce a `basilisk_vector::Record`
    /// ready for upsert.
    #[must_use]
    pub fn into_record(self, vector: Vec<f32>) -> Record {
        Record {
            id: self.id,
            vector,
            text: self.text,
            metadata: self.metadata,
        }
    }
}

/// Chunk one [`IngestRecord`] into one-or-more [`NormalizedRecord`]s.
///
/// `max_tokens_per_chunk` should match the configured embedding
/// provider's `max_tokens_per_input()`. When `body` fits, returns
/// a single chunk. When it doesn't, splits along paragraph
/// boundaries; if a paragraph is itself too long, falls back to a
/// hard byte cap to guarantee every chunk is under the limit.
#[must_use]
pub fn chunk_record(record: &IngestRecord, max_tokens_per_chunk: usize) -> Vec<NormalizedRecord> {
    // 4 bytes/token is the working heuristic (see crate docs).
    let max_bytes = max_tokens_per_chunk.saturating_mul(4);

    let full_text = if record.title.is_empty() {
        record.body.clone()
    } else {
        format!("{}\n\n{}", record.title, record.body)
    };

    if full_text.len() <= max_bytes {
        let id = derive_id(&record.source, &record.source_id, 0);
        return vec![NormalizedRecord {
            id: id.clone(),
            text: full_text,
            metadata: build_metadata(record, &id, 0, 1),
        }];
    }

    let paragraphs = split_on_paragraphs(&full_text);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for para in paragraphs {
        if para.len() > max_bytes {
            // A single paragraph larger than the limit — flush
            // anything pending and hard-split this one.
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            for piece in hard_split(&para, max_bytes) {
                chunks.push(piece);
            }
            continue;
        }
        if current.len() + para.len() + 2 > max_bytes && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(&para);
    }
    if !current.is_empty() {
        chunks.push(current);
    }

    let total = chunks.len();
    chunks
        .into_iter()
        .enumerate()
        .map(|(idx, text)| {
            let id = derive_id(&record.source, &record.source_id, idx);
            NormalizedRecord {
                id: id.clone(),
                text,
                metadata: build_metadata(record, &id, idx, total),
            }
        })
        .collect()
}

/// Deterministic chunk id: `sha256(source + "|" + source_id + "|" + chunk_idx)`,
/// hex-encoded. Stable across runs — re-ingesting the same record
/// produces the same ids (upsert-friendly).
fn derive_id(source: &str, source_id: &str, chunk_idx: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    hasher.update(b"|");
    hasher.update(source_id.as_bytes());
    hasher.update(b"|");
    hasher.update(chunk_idx.to_le_bytes());
    hex::encode(hasher.finalize())
}

fn build_metadata(
    record: &IngestRecord,
    chunk_id: &str,
    chunk_index: usize,
    total_chunks: usize,
) -> Metadata {
    // Merge the source's `extra` with chunk-linkage fields. We do a
    // shallow merge: if `extra` is already an object, add the
    // linkage keys to it; otherwise wrap it.
    let mut extra = match &record.extra {
        serde_json::Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };
    if total_chunks > 1 {
        let parent = derive_id(&record.source, &record.source_id, 0);
        extra.insert("parent_id".into(), serde_json::Value::String(parent));
        extra.insert("chunk_index".into(), chunk_index.into());
        extra.insert("total_chunks".into(), total_chunks.into());
    }
    let _ = chunk_id; // reserved for future use (e.g. self-ref)

    Metadata {
        source: record.source.clone(),
        source_id: record.source_id.clone(),
        kind: record.kind.clone(),
        tags: record.tags.clone(),
        engagement_id: None,
        extra: serde_json::Value::Object(extra),
        indexed_at: std::time::SystemTime::now(),
    }
}

/// Split on double-newline paragraph boundaries, preserving order
/// and trimming empties.
fn split_on_paragraphs(s: &str) -> Vec<String> {
    s.split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(std::string::ToString::to_string)
        .collect()
}

/// Byte-based fallback splitter for paragraphs that themselves
/// exceed the budget. Breaks on whitespace where possible to
/// avoid mid-word cuts; otherwise hard byte boundaries (last
/// resort — preserves utf-8 by splitting on char boundaries).
fn hard_split(s: &str, max_bytes: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut remaining = s;
    while remaining.len() > max_bytes {
        // Try splitting on whitespace near the boundary; fall back
        // to the last char boundary at or before `max_bytes`.
        let candidate = &remaining[..max_bytes];
        let cut = candidate
            .rfind(char::is_whitespace)
            .unwrap_or_else(|| last_char_boundary(candidate));
        let (head, tail) = remaining.split_at(cut);
        out.push(head.trim().to_string());
        remaining = tail.trim_start();
    }
    if !remaining.is_empty() {
        out.push(remaining.to_string());
    }
    out
}

fn last_char_boundary(s: &str) -> usize {
    let mut i = s.len();
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record(body: impl Into<String>) -> IngestRecord {
        IngestRecord {
            source: "solodit".into(),
            source_id: "sol-42".into(),
            kind: "finding".into(),
            title: "Reentrancy in withdraw()".into(),
            body: body.into(),
            tags: vec!["severity:high".into()],
            extra: serde_json::json!({ "auditor": "Trail of Bits" }),
        }
    }

    #[test]
    fn short_record_emits_one_chunk_without_linkage() {
        let chunks = chunk_record(&sample_record("short body"), 1000);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].text.contains("Reentrancy"));
        assert!(chunks[0].text.contains("short body"));
        // total_chunks==1 → no linkage keys added.
        assert!(chunks[0].metadata.extra.get("chunk_index").is_none());
    }

    #[test]
    fn oversize_record_splits_along_paragraph_boundaries() {
        // Three paragraphs, each 500 bytes; 1500 total vs cap of
        // 800 bytes (200 tokens × 4). Expected split: 2 chunks,
        // boundary between paragraphs.
        let p = "a".repeat(500);
        let body = format!("{p}\n\n{p}\n\n{p}");
        let chunks = chunk_record(&sample_record(body), 200);
        assert!(chunks.len() >= 2, "got {} chunks", chunks.len());
        // Every chunk should carry linkage metadata.
        let total = chunks.len();
        for (idx, c) in chunks.iter().enumerate() {
            assert_eq!(
                c.metadata
                    .extra
                    .get("chunk_index")
                    .and_then(serde_json::Value::as_u64),
                Some(idx as u64),
            );
            assert_eq!(
                c.metadata
                    .extra
                    .get("total_chunks")
                    .and_then(serde_json::Value::as_u64),
                Some(total as u64),
            );
            assert!(c.metadata.extra.get("parent_id").is_some());
        }
    }

    #[test]
    fn all_chunks_share_parent_id() {
        let p = "a".repeat(500);
        let body = format!("{p}\n\n{p}\n\n{p}");
        let chunks = chunk_record(&sample_record(body), 200);
        let parents: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.metadata.extra.get("parent_id").cloned())
            .collect();
        assert!(!parents.is_empty());
        let first = &parents[0];
        assert!(parents.iter().all(|p| p == first));
    }

    #[test]
    fn single_paragraph_over_budget_hard_splits() {
        // One paragraph of 2000 bytes; cap 200*4=800. Expected:
        // ≥ 3 chunks via whitespace-seeking hard split.
        let body = "word ".repeat(400);
        let chunks = chunk_record(&sample_record(body), 200);
        assert!(chunks.len() >= 3, "got {}", chunks.len());
        for c in &chunks {
            assert!(c.text.len() <= 800 + 50, "chunk too big: {}", c.text.len());
        }
    }

    #[test]
    fn ids_are_deterministic_across_runs() {
        let r = sample_record("body");
        let a = chunk_record(&r, 1000);
        let b = chunk_record(&r, 1000);
        assert_eq!(a[0].id, b[0].id);
    }

    #[test]
    fn ids_differ_across_sources() {
        let mut a = sample_record("body");
        a.source = "solodit".into();
        let mut b = sample_record("body");
        b.source = "code4rena".into();
        assert_ne!(chunk_record(&a, 1000)[0].id, chunk_record(&b, 1000)[0].id);
    }

    #[test]
    fn metadata_preserves_source_tags_kind() {
        let chunks = chunk_record(&sample_record("body"), 1000);
        let md = &chunks[0].metadata;
        assert_eq!(md.source, "solodit");
        assert_eq!(md.source_id, "sol-42");
        assert_eq!(md.kind, "finding");
        assert_eq!(md.tags, vec!["severity:high"]);
        assert_eq!(
            md.extra.get("auditor"),
            Some(&serde_json::json!("Trail of Bits"))
        );
    }

    #[test]
    fn into_record_pairs_with_vector() {
        let chunks = chunk_record(&sample_record("body"), 1000);
        let record = chunks[0].clone().into_record(vec![0.1, 0.2, 0.3]);
        assert_eq!(record.vector, vec![0.1, 0.2, 0.3]);
        assert!(record.text.contains("body"));
    }

    #[test]
    fn hard_split_respects_utf8_boundaries() {
        // Emoji is 4 bytes; random byte splits would panic. Cap
        // chosen so the splitter HAS to cut mid-paragraph.
        let body = "a🦀".repeat(200); // 5 bytes each * 200 = 1000 bytes
        let chunks = chunk_record(&sample_record(body), 50); // cap 200 bytes
                                                             // Must not panic; all chunks are valid UTF-8 by construction.
        assert!(!chunks.is_empty());
    }
}
