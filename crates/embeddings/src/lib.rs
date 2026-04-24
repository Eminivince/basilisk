//! Model-agnostic embedding provider for Basilisk.
//!
//! Mirrors the shape of [`basilisk-llm`]: a provider-neutral trait
//! ([`EmbeddingProvider`]) and a small set of well-defined
//! implementations. Set 7 ships:
//!
//!  - [`VoyageBackend`] — primary. Voyage's code-specialized models
//!    outperform general-purpose embeddings on Solidity retrieval.
//!  - [`OpenAIEmbeddingBackend`] — backup, arriving in `CP7.2`.
//!  - [`OllamaEmbeddingBackend`] — local/offline, arriving in `CP7.2`.
//!
//! All backends share error classification, retry shape, and the same
//! request/response vocabulary. Downstream crates
//! (`basilisk-vector`, `basilisk-ingest`, `basilisk-knowledge`) talk
//! to `dyn EmbeddingProvider` and never see provider-specific types.
//!
//! Design decisions:
//!
//!  - **`InputKind::{Query, Document}`.** Some providers (Voyage
//!    especially) emit meaningfully different vectors for the same
//!    text depending on whether you're embedding a query or a
//!    document. Exposing the kind in the request lets each backend
//!    forward it where supported and ignore it where not.
//!  - **Integer-cent pricing.** Same reasoning as `basilisk-llm`'s
//!    `PricingTable` — costs are small, arithmetic needs to be
//!    exact, and we ceiling-round so budget checks never
//!    under-estimate.
//!  - **`Redacted` keys.** API keys never print in `Debug` output.
//!    Mirrors the `basilisk-llm` pattern at
//!    `crates/llm/src/anthropic.rs`.
//!
//! [`basilisk-llm`]: https://docs.rs/basilisk-llm

pub mod backend;
pub mod batching;
pub mod error;
pub mod factory;
pub mod openai_compat;
pub mod pricing;
pub mod types;
pub mod voyage;

pub use backend::EmbeddingProvider;
pub use batching::{BatchingProvider, RetryConfig, TokenBudgetGate};
pub use error::EmbeddingError;
pub use factory::{build_provider, ProviderKind, ProviderSelection};
pub use openai_compat::{
    OpenAICompatibleEmbeddingBackend, Provider, OLLAMA_BASE, OLLAMA_DEFAULT_MODEL, OPENAI_BASE,
    OPENAI_DEFAULT_MODEL,
};
pub use pricing::{EmbeddingPricing, EmbeddingPricingSource, EmbeddingPricingTable};
pub use types::{Embedding, EmbeddingInput, InputKind};
pub use voyage::{VoyageBackend, VOYAGE_DEFAULT_MODEL};
