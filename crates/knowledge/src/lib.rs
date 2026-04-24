//! Public knowledge-base API.
//!
//! Layers search + findings-memory on top of
//! [`basilisk_vector::VectorStore`] and
//! [`basilisk_embeddings::EmbeddingProvider`]. Callers see one
//! type ([`KnowledgeBase`]) and don't handle vectors directly:
//! they pass natural-language queries or finding structs, and
//! the crate does the embedding + storage dance.
//!
//! Findings memory uses the `user_findings` collection with
//! **corrections folded into columns** on that same collection —
//! `is_correction: true`, `corrects_id: Option<String>`,
//! `correction_reason: Option<String>`, `user_verdict:
//! Option<String>` ("confirmed" | "dismissed" | "corrected"). A
//! single collection, one reembed path, same expressiveness as
//! the spec's original two-collection layout.

pub mod error;
pub mod finding;
pub mod knowledge_base;
pub mod search;
pub mod stats;

pub use error::KnowledgeError;
pub use finding::{Correction, FindingId, FindingLocation, FindingRecord, UserVerdict};
pub use knowledge_base::KnowledgeBase;
pub use search::{RetrievedChunk, SearchFilters};
pub use stats::KnowledgeStats;
