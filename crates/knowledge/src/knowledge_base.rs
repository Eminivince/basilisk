//! [`KnowledgeBase`] — the public API.
//!
//! One struct, composed of an `Arc<dyn VectorStore>` +
//! `Arc<dyn EmbeddingProvider>`. Every operation is async;
//! callers hold a single handle and don't thread the backends.

use std::sync::Arc;
use std::time::SystemTime;

use basilisk_embeddings::{EmbeddingInput, EmbeddingProvider};
use basilisk_vector::{
    schema, Filter, Metadata, Record, SearchQuery, VectorStore,
};
use sha2::{Digest, Sha256};

use crate::{
    error::KnowledgeError,
    finding::{Correction, FindingId, FindingRecord, UserVerdict},
    search::{RetrievedChunk, SearchFilters},
    stats::KnowledgeStats,
};

/// The knowledge base. Cheap to clone — inner state is
/// `Arc`-shared.
#[derive(Clone)]
pub struct KnowledgeBase {
    vector: Arc<dyn VectorStore>,
    embeddings: Arc<dyn EmbeddingProvider>,
}

impl KnowledgeBase {
    pub fn new(
        vector: Arc<dyn VectorStore>,
        embeddings: Arc<dyn EmbeddingProvider>,
    ) -> Self {
        Self { vector, embeddings }
    }

    pub fn vector(&self) -> &Arc<dyn VectorStore> {
        &self.vector
    }

    pub fn embeddings(&self) -> &Arc<dyn EmbeddingProvider> {
        &self.embeddings
    }

    /// Ensure all shipped collections exist with the current
    /// embedding provider + dimension. Idempotent; a mismatch on
    /// provider/dim surfaces as
    /// [`basilisk_vector::VectorError::IncompatibleSpec`] rather
    /// than silent corruption.
    pub async fn ensure_collections(&self) -> Result<(), KnowledgeError> {
        let specs =
            schema::all_specs(self.embeddings.identifier(), self.embeddings.dimensions());
        for spec in specs {
            self.vector.create_collection(spec).await?;
        }
        Ok(())
    }

    /// Natural-language search across one-or-more collections.
    ///
    /// Default target is `public_findings` + `advisories` +
    /// `post_mortems` + `user_findings`. Callers scope via
    /// `filters.collections` when they know which corpus they want.
    pub async fn search(
        &self,
        query: &str,
        filters: SearchFilters,
        limit: usize,
    ) -> Result<Vec<RetrievedChunk>, KnowledgeError> {
        if query.trim().is_empty() {
            return Err(KnowledgeError::BadInput("empty query".into()));
        }
        let embedded = self
            .embeddings
            .embed(&[EmbeddingInput::query(query)])
            .await?;
        let vector = embedded
            .into_iter()
            .next()
            .ok_or_else(|| KnowledgeError::BadInput("provider returned no embedding".into()))?
            .vector;
        self.search_by_vector(vector, filters, limit).await
    }

    /// Similar-code search. Same retrieval path as
    /// [`search`](Self::search) but uses
    /// [`InputKind::Document`](basilisk_embeddings::InputKind::Document)
    /// so asymmetric retrieval models (Voyage, Cohere) project
    /// into the corpus side of their dual encoder.
    pub async fn search_similar_code(
        &self,
        code: &str,
        filters: SearchFilters,
        limit: usize,
    ) -> Result<Vec<RetrievedChunk>, KnowledgeError> {
        if code.trim().is_empty() {
            return Err(KnowledgeError::BadInput("empty code snippet".into()));
        }
        let embedded = self
            .embeddings
            .embed(&[EmbeddingInput::document(code)])
            .await?;
        let vector = embedded
            .into_iter()
            .next()
            .ok_or_else(|| KnowledgeError::BadInput("provider returned no embedding".into()))?
            .vector;
        self.search_by_vector(vector, filters, limit).await
    }

    /// Record an agent-produced finding. Returns the [`FindingId`]
    /// the CLI can use for subsequent correct / dismiss / confirm.
    pub async fn record_finding(
        &self,
        session_id: &str,
        target: &str,
        finding: FindingRecord,
    ) -> Result<FindingId, KnowledgeError> {
        self.ensure_user_findings_collection().await?;

        let id_str = derive_finding_id(session_id, &finding.title, target);
        let embedded = self
            .embeddings
            .embed(&[EmbeddingInput::document(finding.embed_text())])
            .await?;
        let vector = embedded
            .into_iter()
            .next()
            .ok_or_else(|| KnowledgeError::BadInput("provider returned no embedding".into()))?
            .vector;

        let mut extra = serde_json::Map::new();
        extra.insert(
            "session_id".into(),
            serde_json::Value::String(session_id.to_string()),
        );
        extra.insert("target".into(), serde_json::Value::String(target.to_string()));
        extra.insert(
            "severity".into(),
            serde_json::Value::String(finding.severity.clone()),
        );
        extra.insert(
            "category".into(),
            serde_json::Value::String(finding.category.clone()),
        );
        extra.insert(
            "is_correction".into(),
            serde_json::Value::Bool(false),
        );
        if let Some(code) = &finding.vulnerable_code {
            extra.insert("vulnerable_code".into(), serde_json::Value::String(code.clone()));
        }
        if let Some(loc) = &finding.location {
            extra.insert("location".into(), serde_json::to_value(loc)?);
        }
        if !finding.related_findings.is_empty() {
            extra.insert(
                "related_findings".into(),
                serde_json::to_value(&finding.related_findings)?,
            );
        }
        if let Some(poc) = &finding.poc_sketch {
            extra.insert("poc_sketch".into(), serde_json::Value::String(poc.clone()));
        }

        let metadata = Metadata {
            source: "user_finding".into(),
            source_id: id_str.clone(),
            kind: "finding".into(),
            tags: vec![
                format!("severity:{}", finding.severity.to_ascii_lowercase()),
                format!("category:{}", finding.category.to_ascii_lowercase()),
            ],
            engagement_id: None,
            extra: serde_json::Value::Object(extra),
            indexed_at: SystemTime::now(),
        };

        let record = Record {
            id: id_str.clone(),
            vector,
            text: finding.embed_text(),
            metadata,
        };
        self.vector
            .upsert(schema::USER_FINDINGS, vec![record])
            .await?;
        Ok(FindingId::new(id_str))
    }

    /// Record a human correction against a prior finding.
    /// Implemented as a sibling row in `user_findings` with
    /// `is_correction: true` + `corrects_id: <finding>` in
    /// `metadata.extra` — no separate `LanceDB` collection to
    /// maintain.
    pub async fn record_correction(
        &self,
        finding_id: &FindingId,
        correction: Correction,
    ) -> Result<(), KnowledgeError> {
        self.ensure_user_findings_collection().await?;

        // Check the finding exists; otherwise surface a typed
        // not-found error so the CLI can tell the operator.
        let target = self
            .vector
            .get(schema::USER_FINDINGS, finding_id.as_str())
            .await?;
        let target = target.ok_or_else(|| KnowledgeError::FindingNotFound(finding_id.0.clone()))?;

        let correction_id = derive_correction_id(finding_id.as_str());
        let embed_text = format!(
            "Correction to finding {}: {}",
            finding_id.as_str(),
            correction.reason
        );
        let embedded = self
            .embeddings
            .embed(&[EmbeddingInput::document(&embed_text)])
            .await?;
        let vector = embedded
            .into_iter()
            .next()
            .ok_or_else(|| KnowledgeError::BadInput("provider returned no embedding".into()))?
            .vector;

        let mut extra = serde_json::Map::new();
        extra.insert("is_correction".into(), serde_json::Value::Bool(true));
        extra.insert(
            "corrects_id".into(),
            serde_json::Value::String(finding_id.0.clone()),
        );
        extra.insert(
            "correction_reason".into(),
            serde_json::Value::String(correction.reason.clone()),
        );
        if let Some(v) = &correction.corrected_severity {
            extra.insert(
                "corrected_severity".into(),
                serde_json::Value::String(v.clone()),
            );
        }
        if let Some(v) = &correction.corrected_category {
            extra.insert(
                "corrected_category".into(),
                serde_json::Value::String(v.clone()),
            );
        }
        extra.insert(
            "user_verdict".into(),
            serde_json::Value::String(UserVerdict::Corrected.as_str().into()),
        );
        // Copy the session_id / target from the corrected finding
        // so search can still filter by session or target.
        if let Some(v) = target.metadata.extra.get("session_id") {
            extra.insert("session_id".into(), v.clone());
        }
        if let Some(v) = target.metadata.extra.get("target") {
            extra.insert("target".into(), v.clone());
        }

        let metadata = Metadata {
            source: "user_finding".into(),
            source_id: correction_id.clone(),
            kind: "correction".into(),
            tags: vec!["user_verdict:corrected".into()],
            engagement_id: None,
            extra: serde_json::Value::Object(extra),
            indexed_at: SystemTime::now(),
        };

        let record = Record {
            id: correction_id,
            vector,
            text: embed_text,
            metadata,
        };
        self.vector
            .upsert(schema::USER_FINDINGS, vec![record])
            .await?;
        Ok(())
    }

    /// Record a simple verdict (`confirmed` / `dismissed`) without
    /// a textual correction. Writes a minimal correction row
    /// tagged with the verdict.
    pub async fn record_verdict(
        &self,
        finding_id: &FindingId,
        verdict: UserVerdict,
    ) -> Result<(), KnowledgeError> {
        self.ensure_user_findings_collection().await?;

        let target = self
            .vector
            .get(schema::USER_FINDINGS, finding_id.as_str())
            .await?;
        let target = target.ok_or_else(|| KnowledgeError::FindingNotFound(finding_id.0.clone()))?;

        let verdict_id = derive_verdict_id(finding_id.as_str(), verdict);
        let embed_text = format!(
            "Verdict {} on finding {}",
            verdict.as_str(),
            finding_id.as_str(),
        );
        let embedded = self
            .embeddings
            .embed(&[EmbeddingInput::document(&embed_text)])
            .await?;
        let vector = embedded
            .into_iter()
            .next()
            .ok_or_else(|| KnowledgeError::BadInput("provider returned no embedding".into()))?
            .vector;

        let mut extra = serde_json::Map::new();
        extra.insert("is_correction".into(), serde_json::Value::Bool(true));
        extra.insert(
            "corrects_id".into(),
            serde_json::Value::String(finding_id.0.clone()),
        );
        extra.insert(
            "user_verdict".into(),
            serde_json::Value::String(verdict.as_str().into()),
        );
        if let Some(v) = target.metadata.extra.get("session_id") {
            extra.insert("session_id".into(), v.clone());
        }
        if let Some(v) = target.metadata.extra.get("target") {
            extra.insert("target".into(), v.clone());
        }

        let metadata = Metadata {
            source: "user_finding".into(),
            source_id: verdict_id.clone(),
            kind: "correction".into(),
            tags: vec![format!("user_verdict:{}", verdict.as_str())],
            engagement_id: None,
            extra: serde_json::Value::Object(extra),
            indexed_at: SystemTime::now(),
        };

        let record = Record {
            id: verdict_id,
            vector,
            text: embed_text,
            metadata,
        };
        self.vector
            .upsert(schema::USER_FINDINGS, vec![record])
            .await?;
        Ok(())
    }

    /// Fetch a finding by id. Returns `None` if it doesn't exist.
    pub async fn get_finding(
        &self,
        id: &FindingId,
    ) -> Result<Option<Record>, KnowledgeError> {
        Ok(self.vector.get(schema::USER_FINDINGS, id.as_str()).await?)
    }

    /// Whole-KB statistics. Lists every collection that exists.
    pub async fn stats(&self) -> Result<KnowledgeStats, KnowledgeError> {
        let infos = self.vector.list_collections().await?;
        let mut collections = Vec::new();
        for info in infos {
            if let Ok(stats) = self.vector.stats(&info.name).await {
                collections.push(stats);
            }
        }
        Ok(KnowledgeStats {
            collections,
            embedding_provider: self.embeddings.identifier().to_string(),
            embedding_dim: self.embeddings.dimensions(),
        })
    }

    // --- internals -----------------------------------------------------

    async fn search_by_vector(
        &self,
        vector: Vec<f32>,
        filters: SearchFilters,
        limit: usize,
    ) -> Result<Vec<RetrievedChunk>, KnowledgeError> {
        let vec_filters = filters.as_vector_filters();
        let targets: Vec<&str> = if filters.collections.is_empty() {
            vec![
                schema::PUBLIC_FINDINGS,
                schema::ADVISORIES,
                schema::POST_MORTEMS,
                schema::USER_FINDINGS,
            ]
        } else {
            filters.collections.iter().map(String::as_str).collect()
        };

        let mut out = Vec::new();
        for collection in targets {
            let query = SearchQuery {
                vector: vector.clone(),
                limit,
                filters: vec_filters.clone(),
                min_score: None,
                include_text: true,
            };
            let hits = match self.vector.search(collection, query).await {
                Ok(h) => h,
                Err(basilisk_vector::VectorError::CollectionNotFound(_)) => continue,
                Err(e) => return Err(e.into()),
            };
            for hit in hits {
                let is_correction = hit
                    .metadata
                    .extra
                    .get("is_correction")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if is_correction && !filters.include_corrections {
                    continue;
                }
                let corrections =
                    self.collect_corrections_for(collection, &hit.id).await?;
                out.push(RetrievedChunk {
                    id: hit.id,
                    text: hit.text,
                    score: hit.score,
                    source: hit.metadata.source.clone(),
                    kind: hit.metadata.kind.clone(),
                    metadata: hit.metadata,
                    corrections,
                });
            }
        }

        // Merge across collections: sort by score descending,
        // apply the overall limit.
        out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        out.truncate(limit);
        Ok(out)
    }

    async fn collect_corrections_for(
        &self,
        collection: &str,
        finding_id: &str,
    ) -> Result<Vec<Correction>, KnowledgeError> {
        // Corrections only live in user_findings; no work to do
        // on other collections.
        if collection != schema::USER_FINDINGS {
            return Ok(Vec::new());
        }
        // Use a zero vector + large limit for an id-based lookup.
        // LanceDB isn't happy about "fetch all rows matching a
        // filter" without a vector, so we post-filter in memory.
        // In practice user_findings is small enough that the
        // linear scan is fine.
        let probe = vec![0.0; self.embeddings.dimensions()];
        let query = SearchQuery {
            vector: probe,
            limit: 1000,
            filters: vec![Filter::Equals {
                field: "corrects_id".into(),
                value: serde_json::Value::String(finding_id.to_string()),
            }],
            min_score: None,
            include_text: true,
        };
        let Ok(hits) = self.vector.search(collection, query).await else {
            return Ok(Vec::new());
        };
        Ok(hits
            .into_iter()
            .filter_map(|hit| {
                let reason = hit
                    .metadata
                    .extra
                    .get("correction_reason")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)?;
                Some(Correction {
                    reason,
                    corrected_severity: hit
                        .metadata
                        .extra
                        .get("corrected_severity")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    corrected_category: hit
                        .metadata
                        .extra
                        .get("corrected_category")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                })
            })
            .collect())
    }

    async fn ensure_user_findings_collection(&self) -> Result<(), KnowledgeError> {
        let spec = schema::user_findings(
            self.embeddings.identifier(),
            self.embeddings.dimensions(),
        );
        self.vector.create_collection(spec).await?;
        Ok(())
    }
}

/// Deterministic finding id: `sha256(session_id | title | target)`.
/// Stable across runs — the same triple re-emits the same id, so
/// idempotent re-ingest is free.
fn derive_finding_id(session_id: &str, title: &str, target: &str) -> String {
    let mut h = Sha256::new();
    h.update(session_id.as_bytes());
    h.update(b"|");
    h.update(title.as_bytes());
    h.update(b"|");
    h.update(target.as_bytes());
    hex::encode(h.finalize())
}

fn derive_correction_id(finding_id: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"correction|");
    h.update(finding_id.as_bytes());
    // Timestamp component so repeat corrections don't collide.
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    h.update(now.to_le_bytes());
    hex::encode(h.finalize())
}

fn derive_verdict_id(finding_id: &str, verdict: UserVerdict) -> String {
    let mut h = Sha256::new();
    h.update(b"verdict|");
    h.update(finding_id.as_bytes());
    h.update(b"|");
    h.update(verdict.as_str().as_bytes());
    // Verdict ids DON'T include a timestamp — re-running confirm
    // on the same finding should be a no-op upsert.
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use basilisk_embeddings::{Embedding, EmbeddingError};
    use basilisk_vector::MemoryVectorStore;

    /// Deterministic mock embedding provider — vector derived from
    /// the input's byte sum so "similar text produces similar
    /// vector" is roughly true and we can reason about retrieval
    /// ordering in tests.
    struct MockEmbed {
        dim: usize,
    }

    #[async_trait]
    impl EmbeddingProvider for MockEmbed {
        #[allow(clippy::unnecessary_literal_bound)]
        fn identifier(&self) -> &str {
            "mock/deterministic"
        }
        fn dimensions(&self) -> usize {
            self.dim
        }
        fn max_tokens_per_input(&self) -> usize {
            1000
        }
        fn max_batch_size(&self) -> usize {
            32
        }
        async fn embed(
            &self,
            inputs: &[EmbeddingInput],
        ) -> Result<Vec<Embedding>, EmbeddingError> {
            Ok(inputs
                .iter()
                .map(|i| {
                    // Sum bytes into a rolling "fingerprint" then
                    // spread across dims.
                    let mut v = vec![0.0_f32; self.dim];
                    for (idx, byte) in i.text.bytes().enumerate() {
                        v[idx % self.dim] += f32::from(byte);
                    }
                    Embedding {
                        vector: v,
                        input_tokens: u32::try_from(i.text.len() / 4 + 1).unwrap_or(u32::MAX),
                    }
                })
                .collect())
        }
    }

    fn kb() -> KnowledgeBase {
        let store: Arc<dyn VectorStore> = Arc::new(MemoryVectorStore::new());
        let embed: Arc<dyn EmbeddingProvider> = Arc::new(MockEmbed { dim: 8 });
        KnowledgeBase::new(store, embed)
    }

    fn sample_finding() -> FindingRecord {
        FindingRecord {
            title: "Reentrancy in withdraw".into(),
            severity: "high".into(),
            category: "reentrancy".into(),
            summary: "Attacker can re-enter during withdrawal".into(),
            vulnerable_code: Some("function withdraw() { ... }".into()),
            location: None,
            reasoning: Some("balance update after external call".into()),
            related_findings: vec![],
            poc_sketch: None,
        }
    }

    #[tokio::test]
    async fn ensure_collections_creates_all_five() {
        let kb = kb();
        kb.ensure_collections().await.unwrap();
        let stats = kb.stats().await.unwrap();
        let names: std::collections::BTreeSet<_> =
            stats.collections.iter().map(|c| c.name.as_str()).collect();
        for n in schema::ALL_COLLECTIONS {
            assert!(names.contains(n), "missing {n}");
        }
    }

    #[tokio::test]
    async fn record_finding_returns_stable_id() {
        let kb = kb();
        let id1 = kb
            .record_finding("session-1", "eth/0xdead", sample_finding())
            .await
            .unwrap();
        let id2 = kb
            .record_finding("session-1", "eth/0xdead", sample_finding())
            .await
            .unwrap();
        assert_eq!(id1, id2);
    }

    #[tokio::test]
    async fn record_finding_changes_id_with_session_or_target() {
        let kb = kb();
        let a = kb
            .record_finding("session-1", "eth/0xdead", sample_finding())
            .await
            .unwrap();
        let b = kb
            .record_finding("session-2", "eth/0xdead", sample_finding())
            .await
            .unwrap();
        assert_ne!(a, b);
        let c = kb
            .record_finding("session-1", "eth/0xbeef", sample_finding())
            .await
            .unwrap();
        assert_ne!(a, c);
    }

    #[tokio::test]
    async fn record_correction_surfaces_via_search() {
        let kb = kb();
        let id = kb
            .record_finding("session-1", "eth/0xdead", sample_finding())
            .await
            .unwrap();
        kb.record_correction(
            &id,
            Correction {
                reason: "false positive: check was actually present".into(),
                corrected_severity: Some("none".into()),
                corrected_category: None,
            },
        )
        .await
        .unwrap();

        let filters = SearchFilters {
            collections: vec![schema::USER_FINDINGS.into()],
            include_corrections: true,
            ..Default::default()
        };
        let hits = kb.search("reentrancy withdraw", filters, 5).await.unwrap();
        // Find the original finding in the results; its
        // corrections list should contain our correction.
        let original = hits
            .iter()
            .find(|h| h.id == id.0)
            .expect("original finding in results");
        assert_eq!(original.corrections.len(), 1);
        assert!(original.corrections[0]
            .reason
            .contains("false positive"));
    }

    #[tokio::test]
    async fn record_correction_on_unknown_finding_errors() {
        let kb = kb();
        let err = kb
            .record_correction(
                &FindingId::new("no-such-finding"),
                Correction {
                    reason: "x".into(),
                    corrected_severity: None,
                    corrected_category: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, KnowledgeError::FindingNotFound(_)));
    }

    #[tokio::test]
    async fn record_verdict_writes_a_correction_row() {
        let kb = kb();
        let id = kb
            .record_finding("session-1", "eth/0xdead", sample_finding())
            .await
            .unwrap();
        kb.record_verdict(&id, UserVerdict::Confirmed).await.unwrap();
        kb.record_verdict(&id, UserVerdict::Confirmed).await.unwrap(); // idempotent

        // Search with corrections included should return both the
        // finding and its confirmed verdict.
        let filters = SearchFilters {
            collections: vec![schema::USER_FINDINGS.into()],
            include_corrections: true,
            ..Default::default()
        };
        let hits = kb.search("reentrancy", filters, 10).await.unwrap();
        let confirm_rows: Vec<_> = hits
            .iter()
            .filter(|h| h.kind == "correction")
            .collect();
        assert_eq!(
            confirm_rows.len(),
            1,
            "repeated confirm should be idempotent via deterministic id; got {}",
            confirm_rows.len(),
        );
    }

    #[tokio::test]
    async fn search_empty_query_errors_rather_than_wasting_an_embed_call() {
        let kb = kb();
        let err = kb.search("   ", SearchFilters::default(), 5).await.unwrap_err();
        assert!(matches!(err, KnowledgeError::BadInput(_)));
    }

    #[tokio::test]
    async fn search_similar_code_empty_errors() {
        let kb = kb();
        let err = kb
            .search_similar_code("", SearchFilters::default(), 5)
            .await
            .unwrap_err();
        assert!(matches!(err, KnowledgeError::BadInput(_)));
    }

    #[tokio::test]
    async fn search_scopes_to_requested_collections() {
        let kb = kb();
        kb.ensure_collections().await.unwrap();
        kb.record_finding("s1", "t", sample_finding()).await.unwrap();
        // Restrict to advisories — should skip user_findings and
        // return empty.
        let filters = SearchFilters {
            collections: vec![schema::ADVISORIES.into()],
            include_corrections: true,
            ..Default::default()
        };
        let hits = kb.search("reentrancy", filters, 5).await.unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn include_corrections_false_drops_correction_rows() {
        let kb = kb();
        let id = kb.record_finding("s1", "t", sample_finding()).await.unwrap();
        kb.record_correction(
            &id,
            Correction {
                reason: "false positive".into(),
                corrected_severity: None,
                corrected_category: None,
            },
        )
        .await
        .unwrap();
        let filters = SearchFilters {
            collections: vec![schema::USER_FINDINGS.into()],
            include_corrections: false,
            ..Default::default()
        };
        let hits = kb.search("reentrancy", filters, 10).await.unwrap();
        // Correction rows have kind="correction"; with
        // include_corrections=false they should be excluded.
        assert!(hits.iter().all(|h| h.kind != "correction"));
    }

    #[tokio::test]
    async fn get_finding_none_for_unknown_id() {
        let kb = kb();
        kb.ensure_user_findings_collection().await.unwrap();
        let r = kb.get_finding(&FindingId::new("no-such-id")).await.unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn stats_reports_provider_and_per_collection_counts() {
        let kb = kb();
        kb.ensure_collections().await.unwrap();
        kb.record_finding("s1", "t", sample_finding()).await.unwrap();
        let s = kb.stats().await.unwrap();
        assert_eq!(s.embedding_provider, "mock/deterministic");
        assert_eq!(s.embedding_dim, 8);
        let total = s.total_records();
        assert!(total >= 1);
    }
}
