//! Request / response types shared by every [`EmbeddingProvider`].
//!
//! Kept intentionally small. Each backend translates to and from its
//! own wire format at the boundary; downstream consumers see only
//! these types.

use serde::{Deserialize, Serialize};

/// One input to be embedded.
///
/// `text` carries the content. `kind` hints whether this is a query
/// (short, about to be compared against documents) or a document
/// (long, being indexed). Providers that optimise asymmetric
/// retrieval (Voyage, Cohere, most modern code-specialised models)
/// use the hint; providers that don't (`OpenAI` text-embedding-3)
/// ignore it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingInput {
    pub text: String,
    #[serde(default)]
    pub kind: InputKind,
}

impl EmbeddingInput {
    /// Shortcut for a document (the typical case when ingesting a
    /// corpus).
    #[must_use]
    pub fn document(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: InputKind::Document,
        }
    }

    /// Shortcut for a query (used at search time).
    #[must_use]
    pub fn query(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: InputKind::Query,
        }
    }
}

/// Retrieval role for an input.
///
/// The asymmetry matters for providers that train separate projection
/// heads for "document" and "query" — the dual-encoder setup
/// Voyage and most top-ranking retrieval models use. For providers
/// that don't distinguish, this hint is dropped silently at the
/// backend boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    /// A short, about-to-be-compared string. Typical at search time.
    Query,
    /// A long, being-indexed string. Typical at ingest time.
    #[default]
    Document,
}

/// One embedded vector plus accounting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Embedding {
    pub vector: Vec<f32>,
    /// Tokens charged for this particular input. For batched calls,
    /// the provider typically reports total tokens; the backend
    /// divides fairly (or reports `None` on providers that don't
    /// surface per-input counts).
    pub input_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_kind_defaults_to_document() {
        // Ingest is the common case; defaulting to Document means
        // callers don't have to think about it during corpus loads.
        assert_eq!(InputKind::default(), InputKind::Document);
    }

    #[test]
    fn input_constructors_set_the_right_kind() {
        assert_eq!(EmbeddingInput::query("x").kind, InputKind::Query);
        assert_eq!(EmbeddingInput::document("y").kind, InputKind::Document);
    }

    #[test]
    fn input_serialises_as_snake_case() {
        let out = serde_json::to_string(&EmbeddingInput::query("hi")).unwrap();
        assert!(out.contains(r#""kind":"query""#), "got {out}");
    }
}
