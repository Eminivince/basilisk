//! Canonical [`CollectionSpec`]s for Basilisk's five knowledge
//! collections.
//!
//! Tightened from the spec's six collections: `corrections` is
//! folded into `user_findings` as an `is_correction` column (see
//! `basilisk-knowledge::finding`). One collection, one reembed
//! path, same expressiveness.
//!
//! Each builder takes `embedding_provider` + `embedding_dim` —
//! callers get these from their configured
//! `EmbeddingProvider::identifier` and `::dimensions`.

use crate::types::{CollectionSpec, DistanceMetric, IndexKind};

pub const PUBLIC_FINDINGS: &str = "public_findings";
pub const ADVISORIES: &str = "advisories";
pub const POST_MORTEMS: &str = "post_mortems";
pub const PROTOCOLS: &str = "protocols";
pub const USER_FINDINGS: &str = "user_findings";

/// The current schema version understood by this crate. Bumped
/// when the record shape (column layout, metadata key conventions)
/// changes in an incompatible way. Individual collections store
/// this number on creation; opening rejects higher values.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Every collection name shipped today. Useful for `knowledge
/// stats`, `knowledge export`, and migration audits.
pub const ALL_COLLECTIONS: &[&str] = &[
    PUBLIC_FINDINGS,
    ADVISORIES,
    POST_MORTEMS,
    PROTOCOLS,
    USER_FINDINGS,
];

/// Build a spec for `public_findings` — external corpus (Solodit,
/// Code4rena et al. later). `IvfPq` default since the Solodit
/// ingest alone pushes north of 50k records.
#[must_use]
pub fn public_findings(provider: impl Into<String>, dim: usize) -> CollectionSpec {
    CollectionSpec {
        name: PUBLIC_FINDINGS.to_string(),
        embedding_dim: dim,
        embedding_provider: provider.into(),
        index: IndexKind::IvfPq,
        distance: DistanceMetric::Cosine,
        schema_version: CURRENT_SCHEMA_VERSION,
    }
}

/// Build a spec for `advisories` — SWC, `OpenZeppelin`, `ToB` (later).
/// Small-to-moderate size; HNSW gives low-latency search without
/// the IVF training cost.
#[must_use]
pub fn advisories(provider: impl Into<String>, dim: usize) -> CollectionSpec {
    CollectionSpec {
        name: ADVISORIES.to_string(),
        embedding_dim: dim,
        embedding_provider: provider.into(),
        index: IndexKind::Hnsw,
        distance: DistanceMetric::Cosine,
        schema_version: CURRENT_SCHEMA_VERSION,
    }
}

/// Build a spec for `post_mortems` — rekt.news + future incident
/// writeups. Reserved now; first ingester lands in Set 7.5.
#[must_use]
pub fn post_mortems(provider: impl Into<String>, dim: usize) -> CollectionSpec {
    CollectionSpec {
        name: POST_MORTEMS.to_string(),
        embedding_dim: dim,
        embedding_provider: provider.into(),
        index: IndexKind::Hnsw,
        distance: DistanceMetric::Cosine,
        schema_version: CURRENT_SCHEMA_VERSION,
    }
}

/// Build a spec for `protocols` — per-engagement documentation
/// ingested by the user. `engagement_id` on metadata filters
/// results to the active engagement.
#[must_use]
pub fn protocols(provider: impl Into<String>, dim: usize) -> CollectionSpec {
    CollectionSpec {
        name: PROTOCOLS.to_string(),
        embedding_dim: dim,
        embedding_provider: provider.into(),
        index: IndexKind::Hnsw,
        distance: DistanceMetric::Cosine,
        schema_version: CURRENT_SCHEMA_VERSION,
    }
}

/// Build a spec for `user_findings` — Basilisk's own accumulated
/// findings, plus corrections (stored as rows with
/// `metadata.extra.is_correction = true`).
///
/// `Flat` index: high precision over modest volume. Precision
/// matters disproportionately here — it's the "what did I learn
/// last time?" surface.
#[must_use]
pub fn user_findings(provider: impl Into<String>, dim: usize) -> CollectionSpec {
    CollectionSpec {
        name: USER_FINDINGS.to_string(),
        embedding_dim: dim,
        embedding_provider: provider.into(),
        index: IndexKind::Flat,
        distance: DistanceMetric::Cosine,
        schema_version: CURRENT_SCHEMA_VERSION,
    }
}

/// All five canonical specs, ready to create. Used by the CLI on
/// first run (`audit knowledge stats` on a fresh machine creates
/// the empty collections).
#[must_use]
pub fn all_specs(provider: impl Into<String>, dim: usize) -> Vec<CollectionSpec> {
    let provider = provider.into();
    vec![
        public_findings(&provider, dim),
        advisories(&provider, dim),
        post_mortems(&provider, dim),
        protocols(&provider, dim),
        user_findings(&provider, dim),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_collections_list_matches_spec_builders() {
        let specs = all_specs("voyage/voyage-code-3", 1024);
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ALL_COLLECTIONS);
    }

    #[test]
    fn each_spec_stamps_schema_version() {
        for spec in all_specs("voyage/voyage-code-3", 1024) {
            assert_eq!(spec.schema_version, CURRENT_SCHEMA_VERSION);
        }
    }

    #[test]
    fn user_findings_uses_flat_for_precision() {
        let spec = user_findings("voyage/voyage-code-3", 1024);
        assert_eq!(spec.index, IndexKind::Flat);
    }

    #[test]
    fn public_findings_uses_ivf_for_scale() {
        // Solodit alone is 50k+ — IVF-PQ handles it; HNSW would
        // hit memory ceilings.
        let spec = public_findings("voyage/voyage-code-3", 1024);
        assert_eq!(spec.index, IndexKind::IvfPq);
    }

    #[test]
    fn specs_carry_embedding_provider_identifier() {
        let spec = public_findings("openai/nvidia/llama-nemotron-embed-vl-1b-v2:free", 3072);
        assert_eq!(spec.embedding_provider, "openai/nvidia/llama-nemotron-embed-vl-1b-v2:free");
        assert_eq!(spec.embedding_dim, 3072);
    }
}
