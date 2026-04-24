//! Voyage AI embedding backend.
//!
//! Voyage publishes code-specialised embedding models that
//! meaningfully outperform general-purpose embeddings on Solidity +
//! smart-contract retrieval. `voyage-code-3` is our primary model
//! for the `public_findings` + `user_findings` collections.
//!
//! Wire shape (POST `https://api.voyageai.com/v1/embeddings`):
//!
//! ```json
//! {
//!   "input":      ["text1", "text2"],
//!   "model":      "voyage-code-3",
//!   "input_type": "query" | "document"
//! }
//! ```
//!
//! Auth: `Authorization: Bearer <VOYAGE_API_KEY>`.
//!
//! Response:
//!
//! ```json
//! {
//!   "object": "list",
//!   "data": [ { "object": "embedding", "embedding": [...], "index": 0 }, ... ],
//!   "model": "voyage-code-3",
//!   "usage": { "total_tokens": 123 }
//! }
//! ```
//!
//! Voyage reports a single `total_tokens` across the batch. We
//! divide it evenly across inputs so `Embedding::input_tokens` sums
//! back to the charged total within rounding.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::{
    backend::EmbeddingProvider,
    error::EmbeddingError,
    types::{Embedding, EmbeddingInput, InputKind},
};

/// Default model — Voyage's code-specialised embedding, 1024-dim.
/// Change only with a clear reason; switching models requires a
/// `reembed` of every collection stamped with this identifier.
pub const VOYAGE_DEFAULT_MODEL: &str = "voyage-code-3";

const DEFAULT_BASE: &str = "https://api.voyageai.com";
const VOYAGE_CODE_3_DIM: usize = 1024;
const VOYAGE_CODE_3_MAX_TOKENS: usize = 16_000;
const VOYAGE_MAX_BATCH: usize = 128;

/// Voyage API embedding backend.
///
/// Cheap to clone — inner state is `Arc`-shared.
#[derive(Clone)]
pub struct VoyageBackend {
    inner: Arc<Inner>,
}

struct Inner {
    client: Client,
    base: String,
    api_key: Redacted,
    model: String,
    identifier: String,
}

/// API-key wrapper that prints as `Redacted(***)` so keys can't
/// accidentally leak into `Debug` output or panic messages.
struct Redacted(String);

impl Redacted {
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Redacted(***)")
    }
}

impl VoyageBackend {
    /// Construct with an explicit API key + the default model.
    pub fn new(api_key: impl Into<String>) -> Result<Self, EmbeddingError> {
        Self::with_model(api_key, VOYAGE_DEFAULT_MODEL)
    }

    /// Construct with an explicit model id. Pin to a dated variant
    /// (`voyage-code-3-20250101`) when reproducibility matters more
    /// than automatic upgrades.
    pub fn with_model(
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, EmbeddingError> {
        Self::with_base_and_model(DEFAULT_BASE, api_key, model)
    }

    /// Full-control constructor. The `base` override is used by
    /// wiremock tests; production callers should use
    /// [`Self::new`] or [`Self::with_model`].
    pub fn with_base_and_model(
        base: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, EmbeddingError> {
        let key = api_key.into();
        let trimmed = key.trim().to_string();
        if trimmed.is_empty() {
            return Err(EmbeddingError::AuthError(
                "VOYAGE_API_KEY is empty".into(),
            ));
        }
        let client = Client::builder()
            // Connect fast; be patient on the response side since
            // large batched embed calls can legitimately run
            // several minutes. Same shape as the LLM backends.
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(600))
            .user_agent(concat!("basilisk/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| EmbeddingError::Other(format!("building http client: {e}")))?;
        let model = model.into();
        let identifier = format!("voyage/{model}");
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                base: base.into().trim_end_matches('/').to_string(),
                api_key: Redacted(trimmed),
                model,
                identifier,
            }),
        })
    }

    /// Model id sent on the wire (no provider prefix).
    pub fn model(&self) -> &str {
        &self.inner.model
    }
}

impl std::fmt::Debug for VoyageBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoyageBackend")
            .field("base", &self.inner.base)
            .field("model", &self.inner.model)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl EmbeddingProvider for VoyageBackend {
    fn identifier(&self) -> &str {
        &self.inner.identifier
    }

    fn dimensions(&self) -> usize {
        // All current Voyage code-specialised models emit 1024-dim
        // vectors. If/when Voyage ships a different-dim model, match
        // it here by model id.
        VOYAGE_CODE_3_DIM
    }

    fn max_tokens_per_input(&self) -> usize {
        VOYAGE_CODE_3_MAX_TOKENS
    }

    fn max_batch_size(&self) -> usize {
        VOYAGE_MAX_BATCH
    }

    async fn embed(&self, inputs: &[EmbeddingInput]) -> Result<Vec<Embedding>, EmbeddingError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        if inputs.len() > self.max_batch_size() {
            return Err(EmbeddingError::BadInput(format!(
                "batch size {} exceeds Voyage max {}",
                inputs.len(),
                self.max_batch_size(),
            )));
        }
        // Voyage's `input_type` is a single scalar for the whole
        // batch; we pick the dominant kind. In practice a batch is
        // homogeneous (all docs during ingest, all queries at search
        // time), so this tie-breaking is defensive.
        let input_type = dominant_kind(inputs);
        let body = build_request_body(&self.inner.model, inputs, input_type);
        let url = format!("{}/v1/embeddings", self.inner.base);
        let response = self
            .inner
            .client
            .post(&url)
            .header("authorization", format!("Bearer {}", self.inner.api_key.as_str()))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(classify_reqwest_error)?;

        let status = response.status();
        if !status.is_success() {
            return Err(map_http_error(status, response).await);
        }

        let wire: WireResponse = response
            .json()
            .await
            .map_err(|e| EmbeddingError::ParseError(e.to_string()))?;
        parse_response(wire, inputs.len())
    }
}

// ---- wire types + builders ------------------------------------------

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    input: Vec<&'a str>,
    input_type: &'static str,
}

#[derive(Deserialize)]
struct WireResponse {
    #[serde(default)]
    data: Vec<WireEmbedding>,
    #[serde(default)]
    usage: WireUsage,
}

#[derive(Deserialize)]
struct WireEmbedding {
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Deserialize, Default)]
struct WireUsage {
    #[serde(default)]
    total_tokens: u32,
}

fn dominant_kind(inputs: &[EmbeddingInput]) -> &'static str {
    let queries = inputs
        .iter()
        .filter(|i| matches!(i.kind, InputKind::Query))
        .count();
    // Ties go to "document" — it's the ingest default and the less
    // specialised side of Voyage's asymmetric projection.
    if queries * 2 > inputs.len() {
        "query"
    } else {
        "document"
    }
}

fn build_request_body<'a>(
    model: &'a str,
    inputs: &'a [EmbeddingInput],
    input_type: &'static str,
) -> serde_json::Value {
    let wire = WireRequest {
        model,
        input: inputs.iter().map(|i| i.text.as_str()).collect(),
        input_type,
    };
    serde_json::to_value(&wire).expect("WireRequest serialises")
}

/// Fold the wire response into `Vec<Embedding>`. Voyage reports one
/// `total_tokens` for the whole batch; we split it evenly so the
/// sum of per-input `input_tokens` equals the charged total within
/// rounding.
fn parse_response(wire: WireResponse, expected: usize) -> Result<Vec<Embedding>, EmbeddingError> {
    if wire.data.len() != expected {
        return Err(EmbeddingError::ParseError(format!(
            "expected {expected} embeddings, got {}",
            wire.data.len(),
        )));
    }
    // Sort by the `index` Voyage returns so the caller's input
    // ordering is preserved even if the API returns out-of-order.
    let mut rows = wire.data;
    rows.sort_by_key(|r| r.index);
    let per_input: u32 = if expected == 0 {
        0
    } else {
        // ceil(total / expected), clamped to u32. `total` is already
        // u32; `expected` is usize but bounded by max_batch_size so
        // the `try_from` can't realistically fail — defensive anyway.
        let expected_u32 = u32::try_from(expected).unwrap_or(u32::MAX);
        wire.usage.total_tokens.div_ceil(expected_u32.max(1))
    };
    Ok(rows
        .into_iter()
        .map(|r| Embedding {
            vector: r.embedding,
            input_tokens: per_input,
        })
        .collect())
}

fn classify_reqwest_error(e: reqwest::Error) -> EmbeddingError {
    if e.is_timeout() {
        EmbeddingError::Timeout
    } else if e.is_connect() || e.is_request() {
        EmbeddingError::NetworkError(e.to_string())
    } else {
        EmbeddingError::Other(e.to_string())
    }
}

async fn map_http_error(status: reqwest::StatusCode, response: reqwest::Response) -> EmbeddingError {
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs);
    let body = response.text().await.unwrap_or_default();
    match status.as_u16() {
        401 | 403 => EmbeddingError::AuthError(body),
        429 => EmbeddingError::RateLimited { retry_after },
        code @ 400..=499 => EmbeddingError::BadInput(format!("HTTP {code}: {body}")),
        code => EmbeddingError::ServerError { status: code, body },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rejects_empty_key() {
        let err = VoyageBackend::new("").unwrap_err();
        assert!(matches!(err, EmbeddingError::AuthError(_)));
    }

    #[test]
    fn new_rejects_whitespace_key() {
        let err = VoyageBackend::new("   \t\n").unwrap_err();
        assert!(matches!(err, EmbeddingError::AuthError(_)));
    }

    #[test]
    fn identifier_is_provider_slash_model() {
        let b = VoyageBackend::new("sk-voyage-x").unwrap();
        assert_eq!(b.identifier(), "voyage/voyage-code-3");
    }

    #[test]
    fn with_model_overrides_the_id() {
        let b = VoyageBackend::with_model("sk-voyage-x", "voyage-3-large").unwrap();
        assert_eq!(b.identifier(), "voyage/voyage-3-large");
        assert_eq!(b.model(), "voyage-3-large");
    }

    #[test]
    fn dimensions_matches_voyage_code_3() {
        let b = VoyageBackend::new("sk-voyage-x").unwrap();
        assert_eq!(b.dimensions(), 1024);
    }

    #[test]
    fn max_batch_and_tokens_nonzero() {
        let b = VoyageBackend::new("sk-voyage-x").unwrap();
        assert!(b.max_batch_size() > 0);
        assert!(b.max_tokens_per_input() > 0);
    }

    #[test]
    fn redacted_debug_masks_the_key() {
        let b = VoyageBackend::with_base_and_model("http://localhost", "sk-secret", "m").unwrap();
        let s = format!("{:?}", b.inner.api_key);
        assert!(!s.contains("sk-secret"), "key leaked: {s}");
    }

    #[test]
    fn request_body_shape_matches_voyage() {
        let inputs = [
            EmbeddingInput::document("hello world"),
            EmbeddingInput::document("second doc"),
        ];
        let body = build_request_body("voyage-code-3", &inputs, "document");
        assert_eq!(body["model"], "voyage-code-3");
        assert_eq!(body["input_type"], "document");
        let arr = body["input"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0], "hello world");
        assert_eq!(arr[1], "second doc");
    }

    #[test]
    fn dominant_kind_picks_query_when_majority() {
        let inputs = [
            EmbeddingInput::query("q1"),
            EmbeddingInput::query("q2"),
            EmbeddingInput::document("d1"),
        ];
        assert_eq!(dominant_kind(&inputs), "query");
    }

    #[test]
    fn dominant_kind_defaults_to_document_on_tie() {
        let inputs = [
            EmbeddingInput::query("q1"),
            EmbeddingInput::document("d1"),
        ];
        // Tie → document (the ingest default, matching Voyage's
        // less-specialised projection).
        assert_eq!(dominant_kind(&inputs), "document");
    }

    #[test]
    fn parse_response_reorders_by_index() {
        // Voyage may return out of order; we sort by `index` so
        // callers get results aligned to their input order.
        let wire = WireResponse {
            data: vec![
                WireEmbedding {
                    index: 1,
                    embedding: vec![2.0],
                },
                WireEmbedding {
                    index: 0,
                    embedding: vec![1.0],
                },
            ],
            usage: WireUsage { total_tokens: 10 },
        };
        let out = parse_response(wire, 2).unwrap();
        assert_eq!(out[0].vector, vec![1.0]);
        assert_eq!(out[1].vector, vec![2.0]);
    }

    #[test]
    fn parse_response_divides_tokens_evenly() {
        let wire = WireResponse {
            data: vec![
                WireEmbedding {
                    index: 0,
                    embedding: vec![1.0],
                },
                WireEmbedding {
                    index: 1,
                    embedding: vec![2.0],
                },
                WireEmbedding {
                    index: 2,
                    embedding: vec![3.0],
                },
            ],
            usage: WireUsage { total_tokens: 7 },
        };
        let out = parse_response(wire, 3).unwrap();
        // ceil(7/3) = 3 — per-input is the ceiling share.
        assert!(out.iter().all(|e| e.input_tokens == 3));
    }

    #[test]
    fn parse_response_rejects_length_mismatch() {
        let wire = WireResponse {
            data: vec![WireEmbedding {
                index: 0,
                embedding: vec![1.0],
            }],
            usage: WireUsage::default(),
        };
        assert!(matches!(parse_response(wire, 2), Err(EmbeddingError::ParseError(_))));
    }

    #[tokio::test]
    async fn embed_empty_returns_empty_without_network() {
        // An empty batch should short-circuit before the HTTP layer.
        let b = VoyageBackend::with_base_and_model("http://0.0.0.0:1", "k", "voyage-code-3")
            .unwrap();
        let out = b.embed(&[]).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn embed_rejects_over_batch_size_without_network() {
        let b =
            VoyageBackend::with_base_and_model("http://0.0.0.0:1", "k", "voyage-code-3").unwrap();
        let huge: Vec<_> = (0..=VOYAGE_MAX_BATCH)
            .map(|i| EmbeddingInput::document(format!("d{i}")))
            .collect();
        let err = b.embed(&huge).await.unwrap_err();
        assert!(matches!(err, EmbeddingError::BadInput(_)));
    }
}
