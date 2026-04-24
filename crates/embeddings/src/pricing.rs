//! Per-model pricing for embeddings. One dimension (per-1M input
//! tokens); no output side, no cache distinction.
//!
//! Pricing data is hardcoded and may drift. **Last reviewed:
//! 2026-04-24.** Re-check the provider pages when a re-embed
//! operation quotes a surprising number:
//!
//!  - Voyage:  <https://docs.voyageai.com/docs/pricing>
//!  - `OpenAI`:  <https://openai.com/pricing>
//!  - Local providers (`Ollama`, `llama.cpp`, `vLLM`): always $0.
//!
//! Same contract as `basilisk-llm`'s pricing module: unknown models
//! return a known-zero pricing paired with
//! [`EmbeddingPricingSource::Unknown`], so `cost_cents()` stays
//! null-free and the `Unknown` source drives a warn-and-continue
//! path at call sites.

/// Per-1M-token pricing for one embedding model. Costs in cents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddingPricing {
    pub per_mtok_cents: u32,
}

impl EmbeddingPricing {
    /// Ceiling-divides so the budget never under-estimates.
    #[must_use]
    pub fn cost_cents(&self, input_tokens: u64) -> u64 {
        let n = input_tokens.saturating_mul(u64::from(self.per_mtok_cents));
        n.div_ceil(1_000_000)
    }

    /// Explicit zero pricing for local providers. Known, not unknown.
    #[must_use]
    pub const fn free() -> Self {
        Self { per_mtok_cents: 0 }
    }
}

/// Where pricing data came from. Same semantics as
/// `basilisk-llm`'s `ModelPricingSource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingPricingSource {
    /// Exact or alias match against a known model id.
    Known,
    /// No entry; callers should log and run without cost enforcement.
    Unknown,
}

/// Static table of embedding pricing.
///
/// Lookup tries exact-family match, then an alias for common
/// provider-prefixed forms.
pub struct EmbeddingPricingTable;

impl EmbeddingPricingTable {
    /// Resolve pricing for `model` along with the source that
    /// supplied it. `Unknown` returns `free()` as a placeholder so
    /// callers can compute `cost_cents()` without a null check.
    #[must_use]
    pub fn for_model(model: &str) -> (EmbeddingPricing, EmbeddingPricingSource) {
        let m = model.to_ascii_lowercase();
        if let Some(p) = lookup_direct(&m) {
            return (p, EmbeddingPricingSource::Known);
        }
        if let Some(rest) = m.strip_prefix("voyage/") {
            if let Some(p) = lookup_direct(rest) {
                return (p, EmbeddingPricingSource::Known);
            }
        }
        if let Some(rest) = m.strip_prefix("openai/") {
            if let Some(p) = lookup_direct(rest) {
                return (p, EmbeddingPricingSource::Known);
            }
        }
        (EmbeddingPricing::free(), EmbeddingPricingSource::Unknown)
    }
}

fn lookup_direct(m: &str) -> Option<EmbeddingPricing> {
    // Voyage (source: docs.voyageai.com/docs/pricing, 2026-04-24).
    // voyage-code-3: $0.18 / 1M tokens = 18 cents.
    if m.starts_with("voyage-code-3") {
        return Some(EmbeddingPricing { per_mtok_cents: 18 });
    }
    // voyage-3-large: $0.18 / 1M tokens.
    if m.starts_with("voyage-3-large") {
        return Some(EmbeddingPricing { per_mtok_cents: 18 });
    }
    // voyage-3: $0.06 / 1M tokens.
    if m.starts_with("voyage-3") {
        return Some(EmbeddingPricing { per_mtok_cents: 6 });
    }

    // OpenAI (source: openai.com/pricing, 2026-04-24).
    // nvidia/llama-nemotron-embed-vl-1b-v2:free: $0.13 / 1M tokens = 13 cents.
    if m.starts_with("nvidia/llama-nemotron-embed-vl-1b-v2:free") {
        return Some(EmbeddingPricing { per_mtok_cents: 13 });
    }
    // text-embedding-3-small: $0.02 / 1M tokens = 2 cents.
    if m.starts_with("text-embedding-3-small") {
        return Some(EmbeddingPricing { per_mtok_cents: 2 });
    }

    // Local providers: explicit free().
    if m.starts_with("ollama/")
        || m.starts_with("llama.cpp/")
        || m.starts_with("vllm/")
        || m.starts_with("lmstudio/")
        || m.starts_with("localai/")
    {
        return Some(EmbeddingPricing::free());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(model: &str) -> EmbeddingPricing {
        let (p, src) = EmbeddingPricingTable::for_model(model);
        assert_eq!(src, EmbeddingPricingSource::Known, "for {model}");
        p
    }

    #[test]
    fn voyage_code_3_pricing_resolves() {
        assert_eq!(known("voyage-code-3").per_mtok_cents, 18);
    }

    #[test]
    fn voyage_3_cheaper_than_code_3() {
        assert!(known("voyage-3").per_mtok_cents < known("voyage-code-3").per_mtok_cents);
    }

    #[test]
    fn openai_embedding_3_large_resolves() {
        assert_eq!(
            known("nvidia/llama-nemotron-embed-vl-1b-v2:free").per_mtok_cents,
            13
        );
    }

    #[test]
    fn voyage_alias_resolves_like_bare() {
        assert_eq!(known("voyage/voyage-code-3"), known("voyage-code-3"),);
    }

    #[test]
    fn openai_alias_resolves_like_bare() {
        assert_eq!(
            known("openai/nvidia/llama-nemotron-embed-vl-1b-v2:free"),
            known("nvidia/llama-nemotron-embed-vl-1b-v2:free"),
        );
    }

    #[test]
    fn ollama_resolves_to_free_with_known_source() {
        let p = known("ollama/nomic-embed-text");
        assert_eq!(p, EmbeddingPricing::free());
    }

    #[test]
    fn unknown_model_returns_unknown_source_without_panic() {
        let (p, src) = EmbeddingPricingTable::for_model("some-future-embed-v99");
        assert_eq!(src, EmbeddingPricingSource::Unknown);
        assert_eq!(p, EmbeddingPricing::free());
    }

    #[test]
    fn cost_cents_ceiling_rounds_tiny_usage_up() {
        let p = EmbeddingPricing { per_mtok_cents: 18 };
        // 1 token at 18¢/M → 0.000018¢ → ceil to 1.
        assert_eq!(p.cost_cents(1), 1);
    }

    #[test]
    fn cost_cents_handles_large_token_counts() {
        let p = EmbeddingPricing { per_mtok_cents: 18 };
        // 10M tokens at 18¢/M = 180¢.
        assert_eq!(p.cost_cents(10_000_000), 180);
    }

    #[test]
    fn cost_cents_saturates_at_u64_max_without_panic() {
        let p = EmbeddingPricing {
            per_mtok_cents: u32::MAX,
        };
        // Should saturate, not panic.
        let _ = p.cost_cents(u64::MAX);
    }

    #[test]
    fn free_pricing_reports_zero_for_any_input() {
        assert_eq!(EmbeddingPricing::free().cost_cents(10_000_000), 0);
    }
}
