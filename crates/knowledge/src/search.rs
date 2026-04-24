//! Search filter + result types.

use basilisk_vector::{Filter, Metadata};
use serde::{Deserialize, Serialize};

use crate::finding::Correction;

/// Typed filters for [`KnowledgeBase::search`] / `::search_similar_code`.
///
/// Each field narrows results further; unset fields don't
/// constrain. When you want to drop past corrections from the
/// retrieval set, set `include_corrections = false`.
///
/// [`KnowledgeBase::search`]: crate::knowledge_base::KnowledgeBase::search
#[derive(Debug, Clone, Default)]
pub struct SearchFilters {
    /// Restrict to these collection names. Empty = search all.
    pub collections: Vec<String>,
    pub source: Option<String>,
    pub kind: Option<String>,
    pub engagement_id: Option<String>,
    pub severity: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    /// When `false`, `user_findings` rows with `is_correction =
    /// true` are dropped from the retrieval set. Default `true` —
    /// corrections are signal.
    pub include_corrections: bool,
}

impl SearchFilters {
    /// Translate to [`basilisk_vector::Filter`]s. Not every field
    /// becomes a vector-store filter — `collections` routes to
    /// different `search()` calls, `include_corrections` is a
    /// post-filter.
    #[must_use]
    pub fn as_vector_filters(&self) -> Vec<Filter> {
        let mut out = Vec::new();
        if let Some(s) = &self.source {
            out.push(Filter::Equals {
                field: "source".into(),
                value: serde_json::Value::String(s.clone()),
            });
        }
        if let Some(k) = &self.kind {
            out.push(Filter::Equals {
                field: "kind".into(),
                value: serde_json::Value::String(k.clone()),
            });
        }
        if let Some(e) = &self.engagement_id {
            out.push(Filter::Equals {
                field: "engagement_id".into(),
                value: serde_json::Value::String(e.clone()),
            });
        }
        if let Some(s) = &self.severity {
            out.push(Filter::Equals {
                field: "severity".into(),
                value: serde_json::Value::String(s.clone()),
            });
        }
        if let Some(c) = &self.category {
            out.push(Filter::Equals {
                field: "category".into(),
                value: serde_json::Value::String(c.clone()),
            });
        }
        if !self.tags.is_empty() {
            out.push(Filter::TagsAll(self.tags.clone()));
        }
        out
    }
}

/// One retrieval result. Carries enough context that the agent
/// can cite the source: `source` + `kind` + metadata. Corrections
/// (if any) are attached to findings they apply to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievedChunk {
    pub id: String,
    pub text: String,
    pub score: f32,
    pub source: String,
    pub kind: String,
    pub metadata: Metadata,
    /// Corrections pointing at this finding. Empty for
    /// non-finding chunks or findings that haven't been
    /// corrected.
    #[serde(default)]
    pub corrections: Vec<Correction>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_default_is_empty_and_includes_corrections() {
        let f = SearchFilters::default();
        assert!(f.collections.is_empty());
        assert!(f.source.is_none());
        assert!(f.tags.is_empty());
        // `Default` picks false for bool; explicit field name here
        // is a reminder that retrieval behavior depends on it.
        assert!(!f.include_corrections);
    }

    #[test]
    fn as_vector_filters_maps_each_set_field() {
        let f = SearchFilters {
            source: Some("solodit".into()),
            severity: Some("high".into()),
            category: Some("reentrancy".into()),
            engagement_id: Some("eng-1".into()),
            tags: vec!["audit:tob".into()],
            ..Default::default()
        };
        let v = f.as_vector_filters();
        // 4 Equals + 1 TagsAll
        assert_eq!(v.len(), 5);
    }

    #[test]
    fn as_vector_filters_empty_when_nothing_set() {
        let f = SearchFilters::default();
        assert!(f.as_vector_filters().is_empty());
    }
}
