//! Token-aware batching helper for embedding requests.
//!
//! The provider trait exposes two batch limits — count
//! ([`EmbeddingProvider::max_batch_size`]) and total tokens
//! ([`EmbeddingProvider::max_tokens_per_batch`]). The naive
//! `slice.chunks(N)` pattern only honours the first; a batch of N
//! large chunks can still blow past the token cap. Voyage's 120k
//! cap is the first one we've hit in practice, but `OpenAI` also
//! enforces a per-batch token limit.
//!
//! [`pack_batches`] walks chunks in order and flushes whenever the
//! next chunk would exceed either limit.

use crate::normalize::NormalizedRecord;
use basilisk_embeddings::EmbeddingProvider;

/// Group `chunks` into batches that respect both the provider's
/// `max_batch_size` (count) and `max_tokens_per_batch` (sum) caps.
///
/// Order is preserved. A single chunk whose own estimated token
/// count exceeds the per-batch token cap is still emitted alone —
/// chunking at `max_tokens_per_input` upstream already bounded
/// each chunk, so this only affects pathological inputs.
///
/// `chunks.chunks(provider.max_batch_size())` is the previous
/// behaviour; use this helper in its place anywhere an ingester
/// embeds in a loop.
#[must_use]
pub fn pack_batches<'a>(
    chunks: &'a [NormalizedRecord],
    provider: &dyn EmbeddingProvider,
) -> Vec<&'a [NormalizedRecord]> {
    let max_count = provider.max_batch_size();
    let max_tokens = provider.max_tokens_per_batch();
    if chunks.is_empty() || max_count == 0 {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut start = 0usize;
    let mut running_tokens: usize = 0;
    let mut running_count: usize = 0;

    for (i, c) in chunks.iter().enumerate() {
        let est = estimate_tokens(&c.text);
        // If adding this chunk would push the running batch over
        // either cap, flush what we have. The "i > start" guard
        // ensures we never emit an empty batch and never get stuck
        // when a single chunk's tokens exceed the cap (it goes
        // through alone on the next iteration).
        if i > start && (running_count + 1 > max_count || running_tokens + est > max_tokens) {
            out.push(&chunks[start..i]);
            start = i;
            running_count = 0;
            running_tokens = 0;
        }
        running_count += 1;
        running_tokens += est;
    }
    if start < chunks.len() {
        out.push(&chunks[start..]);
    }
    out
}

/// Conservative token estimate: bytes / 4. Mirrors the heuristic in
/// `basilisk-embeddings::batching` — over-counts for dense code,
/// which keeps the packer on the safe side of the provider's cap.
fn estimate_tokens(text: &str) -> usize {
    text.len() / 4 + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingester::IngestRecord;
    use crate::normalize::chunk_record;
    use async_trait::async_trait;
    use basilisk_embeddings::{Embedding, EmbeddingError, EmbeddingInput, EmbeddingProvider};

    /// Minimal stub provider with configurable caps. Doesn't embed
    /// anything — `pack_batches` only reads cap accessors.
    struct Stub {
        max_batch: usize,
        max_tokens: usize,
        per_input: usize,
    }

    #[async_trait]
    impl EmbeddingProvider for Stub {
        #[allow(clippy::unnecessary_literal_bound)]
        fn identifier(&self) -> &str {
            "stub/0"
        }
        fn dimensions(&self) -> usize {
            1
        }
        fn max_tokens_per_input(&self) -> usize {
            self.per_input
        }
        fn max_batch_size(&self) -> usize {
            self.max_batch
        }
        fn max_tokens_per_batch(&self) -> usize {
            self.max_tokens
        }
        async fn embed(
            &self,
            _inputs: &[EmbeddingInput],
        ) -> Result<Vec<Embedding>, EmbeddingError> {
            Ok(Vec::new())
        }
    }

    fn record(body: &str) -> IngestRecord {
        IngestRecord {
            source_id: "x".into(),
            source: "test".into(),
            kind: "k".into(),
            title: "T".into(),
            body: body.into(),
            tags: vec![],
            extra: serde_json::Value::Null,
        }
    }

    fn chunks(body: &str) -> Vec<NormalizedRecord> {
        chunk_record(&record(body), 100_000)
    }

    #[test]
    fn empty_input_yields_no_batches() {
        let provider = Stub {
            max_batch: 32,
            max_tokens: 100_000,
            per_input: 16_000,
        };
        let batches = pack_batches(&[], &provider);
        assert!(batches.is_empty());
    }

    #[test]
    fn count_cap_binds_when_chunks_are_small() {
        let provider = Stub {
            max_batch: 3,
            max_tokens: 1_000_000,
            per_input: 16_000,
        };
        // 7 small chunks → 3 batches of [3, 3, 1].
        let mut all = Vec::new();
        for _ in 0..7 {
            all.extend(chunks("tiny"));
        }
        let batches = pack_batches(&all, &provider);
        let sizes: Vec<usize> = batches.iter().map(|b| b.len()).collect();
        assert_eq!(sizes, vec![3, 3, 1]);
    }

    #[test]
    fn token_cap_binds_when_chunks_are_large() {
        let provider = Stub {
            max_batch: 32,
            max_tokens: 200,
            per_input: 16_000,
        };
        // Each chunk: ~160 chars / 4 ≈ 40 estimated tokens. Five
        // chunks total → one batch fits ~5 chunks (200/40); the
        // packer should split when adding the 6th would exceed 200.
        let body = "x".repeat(160);
        let mut all = Vec::new();
        for _ in 0..10 {
            all.extend(chunks(&body));
        }
        let batches = pack_batches(&all, &provider);
        // No batch should exceed the token cap (using same estimate).
        for b in &batches {
            let total: usize = b.iter().map(|c| estimate_tokens(&c.text)).sum();
            assert!(total <= 200, "batch over cap: {total}");
        }
        // And the total chunk count is preserved.
        let count: usize = batches.iter().map(|b| b.len()).sum();
        assert_eq!(count, all.len());
    }

    #[test]
    fn single_oversize_chunk_goes_through_alone() {
        let provider = Stub {
            max_batch: 32,
            max_tokens: 100,
            per_input: 16_000,
        };
        // One chunk of 1000 chars → 251 estimated tokens, over the
        // 100-token batch cap. It should still emit, alone.
        let big = "x".repeat(1000);
        let all = chunks(&big);
        let batches = pack_batches(&all, &provider);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 1);
    }

    #[test]
    fn order_preserved_across_batches() {
        let provider = Stub {
            max_batch: 2,
            max_tokens: 1_000_000,
            per_input: 16_000,
        };
        let mut all = Vec::new();
        for i in 0..5 {
            let body = format!("body_{i}");
            all.extend(chunks(&body));
        }
        let batches = pack_batches(&all, &provider);
        // chunker prepends the record title; assert the body
        // suffix order rather than the full text.
        let flat: Vec<String> = batches
            .iter()
            .flat_map(|b| b.iter().map(|c| c.text.clone()))
            .collect();
        for (i, t) in flat.iter().enumerate() {
            assert!(t.ends_with(&format!("body_{i}")), "got: {t}");
        }
        assert_eq!(flat.len(), 5);
    }
}
