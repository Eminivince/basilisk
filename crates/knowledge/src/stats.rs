//! [`KnowledgeStats`] — the shape returned by
//! [`KnowledgeBase::stats`](crate::knowledge_base::KnowledgeBase::stats).

use basilisk_vector::CollectionStats;
use serde::{Deserialize, Serialize};

/// Summary statistics for the whole knowledge base.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KnowledgeStats {
    pub collections: Vec<CollectionStats>,
    pub embedding_provider: String,
    pub embedding_dim: usize,
}

impl KnowledgeStats {
    /// Sum of records across all collections.
    #[must_use]
    pub fn total_records(&self) -> usize {
        self.collections.iter().map(|c| c.record_count).sum()
    }
}
