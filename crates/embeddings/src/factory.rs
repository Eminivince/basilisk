//! Provider selection + construction.
//!
//! Given a [`ProviderSelection`] (typically built from env vars /
//! `basilisk-core`'s `Config`), pick a concrete backend and wrap
//! it in a `BatchingProvider` with sensible defaults. Downstream
//! callers receive `Arc<dyn EmbeddingProvider>` and never see the
//! provider-specific types.
//!
//! Resolution order (when `provider` is `None`):
//!   1. Voyage — if `voyage_api_key` present.
//!   2. `OpenAI` — if `openai_api_key` present.
//!   3. Ollama — always (local, no key needed).
//!
//! An explicit `provider` setting is honoured verbatim even if its
//! key is missing — the caller gets a clear error, not a silent
//! fallback.

use std::sync::Arc;
use std::time::Duration;

use crate::{
    backend::EmbeddingProvider,
    batching::{BatchingProvider, TokenBudgetGate},
    error::EmbeddingError,
    openai_compat::OpenAICompatibleEmbeddingBackend,
    voyage::VoyageBackend,
};

/// Which embedding provider to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Voyage,
    OpenAi,
    Ollama,
}

impl ProviderKind {
    /// Parse the string form used by `EMBEDDINGS_PROVIDER` /
    /// `Config::embeddings_provider`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "voyage" => Some(Self::Voyage),
            "openai" => Some(Self::OpenAi),
            "ollama" => Some(Self::Ollama),
            _ => None,
        }
    }
}

/// Inputs for [`build_provider`]. Populated by the CLI from
/// `basilisk-core`'s `Config` + explicit flags.
#[derive(Debug, Clone, Default)]
pub struct ProviderSelection {
    /// Explicit provider choice. When `None`, resolution prefers
    /// Voyage → `OpenAI` → Ollama based on available keys.
    pub provider: Option<ProviderKind>,
    pub voyage_api_key: Option<String>,
    pub openai_api_key: Option<String>,
    /// Override the Ollama endpoint. `None` → `http://localhost:11434`.
    pub ollama_host: Option<String>,
    /// Override the model. Provider-specific default otherwise.
    pub model: Option<String>,
    /// Apply Voyage's token-per-minute free-tier gate. Set to
    /// `Some(10_000)` when running against Voyage's free tier.
    pub voyage_token_rate_per_minute: Option<u32>,
}

impl ProviderSelection {
    /// Resolve which provider to build. Honours explicit choice
    /// when set, else falls back by key availability.
    pub fn resolve(&self) -> Result<ProviderKind, EmbeddingError> {
        if let Some(p) = self.provider {
            return Ok(p);
        }
        if self.voyage_api_key.is_some() {
            return Ok(ProviderKind::Voyage);
        }
        if self.openai_api_key.is_some() {
            return Ok(ProviderKind::OpenAi);
        }
        Ok(ProviderKind::Ollama)
    }
}

/// Construct a provider ready for downstream use (wrapped with
/// batching + retry). Returns an `Arc<dyn EmbeddingProvider>` so
/// callers hold a single handle.
pub fn build_provider(
    selection: &ProviderSelection,
) -> Result<Arc<dyn EmbeddingProvider>, EmbeddingError> {
    let kind = selection.resolve()?;
    let base: Arc<dyn EmbeddingProvider> = match kind {
        ProviderKind::Voyage => {
            let key = selection.voyage_api_key.as_deref().ok_or_else(|| {
                EmbeddingError::AuthError(
                    "VOYAGE_API_KEY is not set (embeddings provider resolved to voyage)".into(),
                )
            })?;
            let model = selection
                .model
                .as_deref()
                .unwrap_or(crate::voyage::VOYAGE_DEFAULT_MODEL);
            Arc::new(VoyageBackend::with_model(key, model)?)
        }
        ProviderKind::OpenAi => {
            let key = selection.openai_api_key.as_deref().ok_or_else(|| {
                EmbeddingError::AuthError(
                    "OPENAI_API_KEY is not set (embeddings provider resolved to openai)".into(),
                )
            })?;
            let backend = match selection.model.as_deref() {
                Some(m) => OpenAICompatibleEmbeddingBackend::openai_with_model(key, m)?,
                None => OpenAICompatibleEmbeddingBackend::openai(key)?,
            };
            Arc::new(backend)
        }
        ProviderKind::Ollama => Arc::new(OpenAICompatibleEmbeddingBackend::ollama(
            selection.ollama_host.clone(),
            selection.model.clone(),
        )?),
    };

    let mut wrapped = BatchingProvider::new(base);
    if kind == ProviderKind::Voyage {
        if let Some(limit) = selection.voyage_token_rate_per_minute {
            let gate = Arc::new(TokenBudgetGate::new(limit, Duration::from_secs(60)));
            wrapped = wrapped.with_token_gate(gate);
        }
    }
    Ok(Arc::new(wrapped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_canonical_names() {
        assert_eq!(ProviderKind::parse("voyage"), Some(ProviderKind::Voyage));
        assert_eq!(ProviderKind::parse("openai"), Some(ProviderKind::OpenAi));
        assert_eq!(ProviderKind::parse("ollama"), Some(ProviderKind::Ollama));
    }

    #[test]
    fn parse_is_case_insensitive_and_trims() {
        assert_eq!(ProviderKind::parse("  VOYAGE "), Some(ProviderKind::Voyage));
        assert_eq!(ProviderKind::parse("OpenAI"), Some(ProviderKind::OpenAi));
    }

    #[test]
    fn parse_rejects_unknown() {
        assert_eq!(ProviderKind::parse("cohere"), None);
    }

    #[test]
    fn resolve_honours_explicit_choice_even_without_keys() {
        let s = ProviderSelection {
            provider: Some(ProviderKind::OpenAi),
            ..Default::default()
        };
        assert_eq!(s.resolve().unwrap(), ProviderKind::OpenAi);
    }

    #[test]
    fn resolve_falls_back_to_voyage_when_its_key_is_set() {
        let s = ProviderSelection {
            voyage_api_key: Some("sk-v".into()),
            openai_api_key: Some("sk-o".into()),
            ..Default::default()
        };
        assert_eq!(s.resolve().unwrap(), ProviderKind::Voyage);
    }

    #[test]
    fn resolve_falls_back_to_openai_when_only_openai_key_is_set() {
        let s = ProviderSelection {
            openai_api_key: Some("sk-o".into()),
            ..Default::default()
        };
        assert_eq!(s.resolve().unwrap(), ProviderKind::OpenAi);
    }

    #[test]
    fn resolve_falls_back_to_ollama_when_no_keys() {
        let s = ProviderSelection::default();
        assert_eq!(s.resolve().unwrap(), ProviderKind::Ollama);
    }

    #[test]
    fn build_ollama_works_without_keys() {
        let s = ProviderSelection::default();
        let p = build_provider(&s).expect("builds without keys");
        assert!(p.identifier().starts_with("ollama/"));
    }

    fn expect_err(r: Result<Arc<dyn EmbeddingProvider>, EmbeddingError>) -> EmbeddingError {
        match r {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        }
    }

    #[test]
    fn build_voyage_requires_key() {
        let s = ProviderSelection {
            provider: Some(ProviderKind::Voyage),
            ..Default::default()
        };
        let err = expect_err(build_provider(&s));
        assert!(matches!(err, EmbeddingError::AuthError(_)));
    }

    #[test]
    fn build_openai_requires_key() {
        let s = ProviderSelection {
            provider: Some(ProviderKind::OpenAi),
            ..Default::default()
        };
        let err = expect_err(build_provider(&s));
        assert!(matches!(err, EmbeddingError::AuthError(_)));
    }

    #[test]
    fn build_voyage_honours_explicit_model_override() {
        let s = ProviderSelection {
            provider: Some(ProviderKind::Voyage),
            voyage_api_key: Some("sk-v".into()),
            model: Some("voyage-3-large".into()),
            ..Default::default()
        };
        let p = build_provider(&s).unwrap();
        assert_eq!(p.identifier(), "voyage/voyage-3-large");
    }
}
