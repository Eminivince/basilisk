//! `OpenAICompatibleEmbeddingBackend` — targets any
//! `/v1/embeddings` endpoint. Covers both `OpenAI` (the real thing,
//! paid, Bearer-authed) and local `Ollama` (free, key optional,
//! runs offline).
//!
//! Wire shape identical for both:
//!
//! ```json
//! POST <base>/v1/embeddings
//! Authorization: Bearer <key>    // omitted when key is empty
//! { "input": ["..."], "model": "...", "encoding_format": "float" }
//! ```
//!
//! Response:
//!
//! ```json
//! {
//!   "data": [ { "index": 0, "embedding": [...] }, ... ],
//!   "usage": { "prompt_tokens": 12, "total_tokens": 12 }
//! }
//! ```
//!
//! Ollama exposes this shape at `/v1/embeddings` on the same port
//! as completions (11434 by default). We use the OpenAI-compatible
//! endpoint rather than Ollama's native `/api/embed` so the wire
//! parser is one block of code.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::{
    backend::EmbeddingProvider,
    error::EmbeddingError,
    types::{Embedding, EmbeddingInput},
};

/// `OpenAI`'s production endpoint.
pub const OPENAI_BASE: &str = "https://api.openai.com";
/// `Ollama`'s default local endpoint.
pub const OLLAMA_BASE: &str = "http://localhost:11434";

/// `OpenAI`'s flagship retrieval model (3072-dim).
pub const OPENAI_DEFAULT_MODEL: &str = "text-embedding-3-large";
const OPENAI_DIM: usize = 3072;
const OPENAI_MAX_TOKENS: usize = 8191;
const OPENAI_MAX_BATCH: usize = 2048;

/// `Ollama`'s most popular embedding model (768-dim). Users can
/// override with any model they've `ollama pull`ed.
pub const OLLAMA_DEFAULT_MODEL: &str = "nomic-embed-text";
const OLLAMA_DEFAULT_DIM: usize = 768;
const OLLAMA_DEFAULT_MAX_TOKENS: usize = 8192;
const OLLAMA_MAX_BATCH: usize = 512;

/// Which flavour of the OpenAI-compatible wire we're talking to.
/// Determines auth + default model + dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// Real `api.openai.com`. Bearer key required.
    OpenAi,
    /// Local Ollama (`http://localhost:11434` by default). Key
    /// optional; ignored if the operator has put a proxy in front.
    Ollama,
}

impl Provider {
    fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenAi => OPENAI_BASE,
            Self::Ollama => OLLAMA_BASE,
        }
    }
    fn default_dim(self) -> usize {
        match self {
            Self::OpenAi => OPENAI_DIM,
            Self::Ollama => OLLAMA_DEFAULT_DIM,
        }
    }
    fn default_max_tokens(self) -> usize {
        match self {
            Self::OpenAi => OPENAI_MAX_TOKENS,
            Self::Ollama => OLLAMA_DEFAULT_MAX_TOKENS,
        }
    }
    fn default_max_batch(self) -> usize {
        match self {
            Self::OpenAi => OPENAI_MAX_BATCH,
            Self::Ollama => OLLAMA_MAX_BATCH,
        }
    }
    fn identifier_prefix(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Ollama => "ollama",
        }
    }
    fn requires_key(self) -> bool {
        matches!(self, Self::OpenAi)
    }
}

/// Backend for any `/v1/embeddings`-speaking service.
#[derive(Clone)]
pub struct OpenAICompatibleEmbeddingBackend {
    inner: Arc<Inner>,
}

struct Inner {
    client: Client,
    base: String,
    api_key: Option<Redacted>,
    model: String,
    identifier: String,
    provider: Provider,
    dimensions: usize,
    max_tokens: usize,
    max_batch: usize,
}

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

impl OpenAICompatibleEmbeddingBackend {
    /// Construct an `OpenAI` backend with the default model. Key
    /// required.
    pub fn openai(api_key: impl Into<String>) -> Result<Self, EmbeddingError> {
        Self::new(Provider::OpenAi, api_key, OPENAI_DEFAULT_MODEL, None)
    }

    /// Construct an `OpenAI` backend with an explicit model.
    pub fn openai_with_model(
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, EmbeddingError> {
        Self::new(Provider::OpenAi, api_key, model, None)
    }

    /// Construct an `Ollama` backend. `host` defaults to
    /// `http://localhost:11434` when `None`. Empty key allowed.
    pub fn ollama(host: Option<String>, model: Option<String>) -> Result<Self, EmbeddingError> {
        let base = host.unwrap_or_else(|| OLLAMA_BASE.to_string());
        let model = model.unwrap_or_else(|| OLLAMA_DEFAULT_MODEL.to_string());
        Self::with_full_config(Provider::Ollama, &base, "", &model, None, None)
    }

    /// Full-control constructor. `dimensions`/`max_tokens`/
    /// `max_batch` default to the provider's known values when
    /// `None`; callers who know they're running a non-default
    /// model on Ollama (e.g. `bge-m3` at 1024-dim) can override.
    pub fn with_full_config(
        provider: Provider,
        base: &str,
        api_key: &str,
        model: &str,
        dimensions: Option<usize>,
        max_tokens: Option<usize>,
    ) -> Result<Self, EmbeddingError> {
        let trimmed = api_key.trim().to_string();
        if trimmed.is_empty() && provider.requires_key() {
            return Err(EmbeddingError::AuthError(format!(
                "{} API key is empty",
                provider.identifier_prefix()
            )));
        }
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(600))
            .user_agent(concat!("basilisk/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| EmbeddingError::Other(format!("building http client: {e}")))?;
        let identifier = format!("{}/{}", provider.identifier_prefix(), model);
        let api_key = if trimmed.is_empty() {
            None
        } else {
            Some(Redacted(trimmed))
        };
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                base: base.trim_end_matches('/').to_string(),
                api_key,
                model: model.to_string(),
                identifier,
                provider,
                dimensions: dimensions.unwrap_or_else(|| provider.default_dim()),
                max_tokens: max_tokens.unwrap_or_else(|| provider.default_max_tokens()),
                max_batch: provider.default_max_batch(),
            }),
        })
    }

    fn new(
        provider: Provider,
        api_key: impl Into<String>,
        model: impl Into<String>,
        dim: Option<usize>,
    ) -> Result<Self, EmbeddingError> {
        Self::with_full_config(
            provider,
            provider.default_base_url(),
            &api_key.into(),
            &model.into(),
            dim,
            None,
        )
    }

    fn apply_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.inner.api_key {
            Some(key) => req.header("authorization", format!("Bearer {}", key.as_str())),
            None => req,
        }
    }
}

impl std::fmt::Debug for OpenAICompatibleEmbeddingBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAICompatibleEmbeddingBackend")
            .field("provider", &self.inner.provider)
            .field("base", &self.inner.base)
            .field("model", &self.inner.model)
            .field("dimensions", &self.inner.dimensions)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAICompatibleEmbeddingBackend {
    fn identifier(&self) -> &str {
        &self.inner.identifier
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions
    }

    fn max_tokens_per_input(&self) -> usize {
        self.inner.max_tokens
    }

    fn max_batch_size(&self) -> usize {
        self.inner.max_batch
    }

    async fn embed(&self, inputs: &[EmbeddingInput]) -> Result<Vec<Embedding>, EmbeddingError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        if inputs.len() > self.max_batch_size() {
            return Err(EmbeddingError::BadInput(format!(
                "batch size {} exceeds {} max {}",
                inputs.len(),
                self.inner.provider.identifier_prefix(),
                self.max_batch_size(),
            )));
        }

        let body = build_request_body(&self.inner.model, inputs);
        let url = format!("{}/v1/embeddings", self.inner.base);
        let builder = self
            .inner
            .client
            .post(&url)
            .header("content-type", "application/json");
        let builder = self.apply_auth(builder);
        let response = builder
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

// ---- wire types ------------------------------------------------------

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    input: Vec<&'a str>,
    /// Spell out `float` explicitly — some shims default to base64.
    encoding_format: &'static str,
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
    #[serde(default)]
    prompt_tokens: u32,
}

fn build_request_body<'a>(model: &'a str, inputs: &'a [EmbeddingInput]) -> serde_json::Value {
    let wire = WireRequest {
        model,
        input: inputs.iter().map(|i| i.text.as_str()).collect(),
        encoding_format: "float",
    };
    serde_json::to_value(&wire).expect("WireRequest serialises")
}

fn parse_response(wire: WireResponse, expected: usize) -> Result<Vec<Embedding>, EmbeddingError> {
    if wire.data.len() != expected {
        return Err(EmbeddingError::ParseError(format!(
            "expected {expected} embeddings, got {}",
            wire.data.len(),
        )));
    }
    let mut rows = wire.data;
    rows.sort_by_key(|r| r.index);
    // OpenAI surfaces both `prompt_tokens` and `total_tokens`; they
    // equal each other for embeddings. Ollama may report 0. Use
    // whichever is non-zero.
    let total = wire.usage.total_tokens.max(wire.usage.prompt_tokens);
    let expected_u32 = u32::try_from(expected).unwrap_or(u32::MAX);
    let per_input = if expected == 0 {
        0
    } else {
        total.div_ceil(expected_u32.max(1))
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

async fn map_http_error(
    status: reqwest::StatusCode,
    response: reqwest::Response,
) -> EmbeddingError {
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
    fn openai_rejects_empty_key() {
        let err = OpenAICompatibleEmbeddingBackend::openai("").unwrap_err();
        assert!(matches!(err, EmbeddingError::AuthError(_)));
    }

    #[test]
    fn ollama_allows_empty_key() {
        let b = OpenAICompatibleEmbeddingBackend::ollama(None, None).unwrap();
        assert_eq!(b.identifier(), "ollama/nomic-embed-text");
    }

    #[test]
    fn identifier_reflects_provider_and_model() {
        let b = OpenAICompatibleEmbeddingBackend::openai("sk-x").unwrap();
        assert_eq!(b.identifier(), "openai/text-embedding-3-large");
    }

    #[test]
    fn dimensions_match_provider_defaults() {
        let openai = OpenAICompatibleEmbeddingBackend::openai("sk-x").unwrap();
        let ollama = OpenAICompatibleEmbeddingBackend::ollama(None, None).unwrap();
        assert_eq!(openai.dimensions(), 3072);
        assert_eq!(ollama.dimensions(), 768);
    }

    #[test]
    fn ollama_override_host_and_model() {
        let b = OpenAICompatibleEmbeddingBackend::ollama(
            Some("http://192.168.1.5:11434".into()),
            Some("bge-m3".into()),
        )
        .unwrap();
        assert_eq!(b.identifier(), "ollama/bge-m3");
    }

    #[test]
    fn request_body_includes_encoding_format_float() {
        let inputs = [EmbeddingInput::document("hi")];
        let body = build_request_body("text-embedding-3-large", &inputs);
        assert_eq!(body["encoding_format"], "float");
        assert_eq!(body["model"], "text-embedding-3-large");
        assert_eq!(body["input"][0], "hi");
    }

    #[test]
    fn parse_response_reorders_and_prefers_nonzero_tokens() {
        // Ollama might send `total_tokens: 0` but `prompt_tokens: N`.
        // We take the max so per-input tokens is meaningful either way.
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
            usage: WireUsage {
                total_tokens: 0,
                prompt_tokens: 4,
            },
        };
        let out = parse_response(wire, 2).unwrap();
        assert_eq!(out[0].vector, vec![1.0]);
        assert_eq!(out[1].vector, vec![2.0]);
        // ceil(4/2) = 2
        assert_eq!(out[0].input_tokens, 2);
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
        assert!(matches!(
            parse_response(wire, 3),
            Err(EmbeddingError::ParseError(_))
        ));
    }

    #[tokio::test]
    async fn embed_empty_short_circuits() {
        let b = OpenAICompatibleEmbeddingBackend::with_full_config(
            Provider::OpenAi,
            "http://0.0.0.0:1",
            "sk-x",
            "text-embedding-3-large",
            None,
            None,
        )
        .unwrap();
        let out = b.embed(&[]).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn embed_over_batch_rejects_before_network() {
        let b = OpenAICompatibleEmbeddingBackend::with_full_config(
            Provider::Ollama,
            "http://0.0.0.0:1",
            "",
            "nomic-embed-text",
            None,
            None,
        )
        .unwrap();
        let huge: Vec<_> = (0..=OLLAMA_MAX_BATCH)
            .map(|i| EmbeddingInput::document(format!("d{i}")))
            .collect();
        let err = b.embed(&huge).await.unwrap_err();
        assert!(matches!(err, EmbeddingError::BadInput(_)));
    }

    #[test]
    fn redacted_debug_masks_key() {
        let b = OpenAICompatibleEmbeddingBackend::openai("sk-hideme").unwrap();
        let s = format!("{b:?}");
        assert!(!s.contains("sk-hideme"), "leak: {s}");
    }
}
