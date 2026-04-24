//! Per-model pricing. Used by the agent to estimate + enforce cost budgets.
//!
//! Prices quoted in **cents per million tokens** (integer; one cent is the
//! cheapest unit the cost budget needs to express).
//!
//! Pricing data is hardcoded and may drift. **Last reviewed: 2026-04-24.**
//! Re-check each provider's public pricing page quarterly or the next time
//! cost surfaces as a pain point:
//!   - Anthropic: <https://www.anthropic.com/pricing>
//!   - `OpenAI`:    <https://openai.com/pricing>
//!   - `OpenRouter` aggregates all of the above at <https://openrouter.ai/models>
//!
//! Only informational — stale numbers produce slightly-wrong cost
//! estimates, not broken functionality. The runner falls back to a
//! warn-and-continue path when a model is unknown, so new-provider launches
//! don't break operators using the agent against them.

use serde::{Deserialize, Serialize};

use crate::types::TokenUsage;

/// Per-1M-token pricing for one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPricing {
    pub input_per_mtok_cents: u32,
    pub output_per_mtok_cents: u32,
    pub cache_read_per_mtok_cents: u32,
    pub cache_write_per_mtok_cents: u32,
}

impl ModelPricing {
    /// Estimated cost in cents for one response's usage. Rounds up so
    /// the budget never under-estimates.
    pub fn cost_cents(&self, usage: &TokenUsage) -> u32 {
        fn mul(tokens: u32, per_mtok: u32) -> u32 {
            // ceil(tokens * per_mtok / 1_000_000) in u64 to avoid overflow.
            let n = u64::from(tokens) * u64::from(per_mtok);
            let div = n.div_ceil(1_000_000);
            u32::try_from(div).unwrap_or(u32::MAX)
        }
        let in_c = mul(usage.input_tokens, self.input_per_mtok_cents);
        let out_c = mul(usage.output_tokens, self.output_per_mtok_cents);
        let read_c = mul(
            usage.cache_read_input_tokens.unwrap_or(0),
            self.cache_read_per_mtok_cents,
        );
        let write_c = mul(
            usage.cache_creation_input_tokens.unwrap_or(0),
            self.cache_write_per_mtok_cents,
        );
        in_c.saturating_add(out_c)
            .saturating_add(read_c)
            .saturating_add(write_c)
    }

    /// Zero-cost pricing. Used for local providers (Ollama, llama.cpp,
    /// vLLM, LM Studio) so cost accounting is *explicit* — operators
    /// see "$0" rather than "unknown" when running locally.
    #[must_use]
    pub const fn free() -> Self {
        Self {
            input_per_mtok_cents: 0,
            output_per_mtok_cents: 0,
            cache_read_per_mtok_cents: 0,
            cache_write_per_mtok_cents: 0,
        }
    }
}

/// Where the pricing data for a given model lookup came from.
///
/// Callers use this to distinguish confidently-priced runs (`Known`,
/// `ProviderPrefix`) from "no data" (`Unknown`). Cost enforcement kicks
/// in only for the first two; `Unknown` surfaces a one-shot warning and
/// lets the run proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelPricingSource {
    /// Exact or aliased match against a known model id.
    Known,
    /// Matched after stripping a provider prefix like `openrouter/`.
    ProviderPrefix,
    /// No entry — cost estimation is disabled for this model.
    Unknown,
}

/// Static table of pricing for every model we know about.
///
/// Lookup is not just exact match — we strip known provider prefixes and
/// honour an alias table so the common forms all resolve:
///
///  - `claude-opus-4-7-20250929`              → Anthropic (prefix family match)
///  - `anthropic/claude-opus-4-7`             → Anthropic (alias)
///  - `openrouter/anthropic/claude-opus-4-7`  → Anthropic (prefix strip → alias)
///  - `openrouter/openai/gpt-4o`              → `OpenAI`   (prefix strip)
///  - `ollama/llama3.1:70b`                   → zero-cost local
///  - `gpt-99-future-model`                   → `Unknown`; cost disabled
pub struct PricingTable;

impl PricingTable {
    /// Resolve pricing for `model`, along with the source that
    /// supplied it. Returns `(pricing, Unknown)` with zeros if there's
    /// no match — callers should branch on the source, not the price,
    /// since a `Known` entry can legitimately be `free()` for local
    /// providers.
    #[must_use]
    pub fn for_model(model: &str) -> (ModelPricing, ModelPricingSource) {
        let lowered = model.to_ascii_lowercase();

        // 1) Exact-prefix hits go through `lookup_anthropic_family` /
        //    `lookup_openai_family` / `lookup_local_family` — these are
        //    substring-prefix matches, not alias lookups.
        if let Some(p) = lookup_direct(&lowered) {
            return (p, ModelPricingSource::Known);
        }

        // 2) Strip the OpenRouter prefix and try again. OpenRouter
        //    routes every model as `<vendor>/<model>`; after strip we
        //    usually land on a canonical id like `anthropic/claude-...`
        //    or `openai/gpt-...` which the alias table covers.
        if let Some(rest) = lowered.strip_prefix("openrouter/") {
            if let Some(p) = lookup_aliased(rest).or_else(|| lookup_direct(rest)) {
                return (p, ModelPricingSource::ProviderPrefix);
            }
        }

        // 3) Alias-only hits for explicit `<vendor>/<model>` forms
        //    passed directly (not via openrouter wrapping).
        if let Some(p) = lookup_aliased(&lowered) {
            return (p, ModelPricingSource::Known);
        }

        (ModelPricing::free(), ModelPricingSource::Unknown)
    }
}

/// Substring-prefix lookup for canonical model families. Anthropic
/// uses `claude-opus-4-7-<date>` / `claude-sonnet-4-6-<date>` etc., so
/// a `starts_with` check on the family prefix matches every dated
/// variant without needing a row per release.
///
/// `OpenAI` uses `gpt-4o`, `gpt-4o-mini`, `gpt-5`, `gpt-5-mini` with
/// optional `-<date>` suffixes — same trick.
///
/// Local providers get explicit zero-cost rows so operators see `$0`
/// instead of `Unknown`.
fn lookup_direct(m: &str) -> Option<ModelPricing> {
    // --- Anthropic (source: anthropic.com/pricing, 2026-04-24) ---
    if m.starts_with("claude-opus-4") {
        // $15 / 1M input, $75 / 1M output, $1.50 cache read, $18.75 cache write.
        return Some(ModelPricing {
            input_per_mtok_cents: 1500,
            output_per_mtok_cents: 7500,
            cache_read_per_mtok_cents: 150,
            cache_write_per_mtok_cents: 1875,
        });
    }
    if m.starts_with("claude-sonnet-4") {
        // $3 / 1M input, $15 / 1M output, $0.30 cache read, $3.75 cache write.
        return Some(ModelPricing {
            input_per_mtok_cents: 300,
            output_per_mtok_cents: 1500,
            cache_read_per_mtok_cents: 30,
            cache_write_per_mtok_cents: 375,
        });
    }
    if m.starts_with("claude-haiku-4") {
        // $1 / 1M input, $5 / 1M output, $0.10 cache read, $1.25 cache write.
        return Some(ModelPricing {
            input_per_mtok_cents: 100,
            output_per_mtok_cents: 500,
            cache_read_per_mtok_cents: 10,
            cache_write_per_mtok_cents: 125,
        });
    }

    // --- OpenAI (source: openai.com/pricing, 2026-04-24) ---
    // GPT-5 family. OpenAI's prompt-caching discount is 50% of input
    // price; we record it in the cache_read slot so cost_cents() maths
    // stays right. They don't charge for cache writes.
    if m.starts_with("gpt-5-mini") {
        // $0.25 / 1M input, $2 / 1M output; cache read ~ $0.125.
        return Some(ModelPricing {
            input_per_mtok_cents: 25,
            output_per_mtok_cents: 200,
            cache_read_per_mtok_cents: 12,
            cache_write_per_mtok_cents: 0,
        });
    }
    if m.starts_with("gpt-5") {
        // $1.25 / 1M input, $10 / 1M output; cache read ~ $0.625.
        return Some(ModelPricing {
            input_per_mtok_cents: 125,
            output_per_mtok_cents: 1000,
            cache_read_per_mtok_cents: 62,
            cache_write_per_mtok_cents: 0,
        });
    }
    if m.starts_with("gpt-4o-mini") {
        // $0.15 / 1M input, $0.60 / 1M output; cache read ~ $0.075.
        return Some(ModelPricing {
            input_per_mtok_cents: 15,
            output_per_mtok_cents: 60,
            cache_read_per_mtok_cents: 7,
            cache_write_per_mtok_cents: 0,
        });
    }
    if m.starts_with("gpt-4o") {
        // $2.50 / 1M input, $10 / 1M output; cache read ~ $1.25.
        return Some(ModelPricing {
            input_per_mtok_cents: 250,
            output_per_mtok_cents: 1000,
            cache_read_per_mtok_cents: 125,
            cache_write_per_mtok_cents: 0,
        });
    }

    // --- Local providers: explicit zero cost ---
    // Explicit > absent. Operators running locally should see "$0",
    // not "unknown model," and --max-cost should still enforce (which
    // for free() is a no-op for any positive cap).
    if m.starts_with("ollama/")
        || m.starts_with("llama.cpp/")
        || m.starts_with("vllm/")
        || m.starts_with("lmstudio/")
        || m.starts_with("localai/")
    {
        return Some(ModelPricing::free());
    }

    None
}

/// Alias lookup for provider-prefixed canonical forms. These are the
/// most common shapes callers pass directly when they want to be
/// explicit about routing — e.g. `anthropic/claude-opus-4-7` or
/// `openai/gpt-4o`.
fn lookup_aliased(m: &str) -> Option<ModelPricing> {
    if let Some(rest) = m.strip_prefix("anthropic/") {
        return lookup_direct(rest);
    }
    if let Some(rest) = m.strip_prefix("openai/") {
        return lookup_direct(rest);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(model: &str) -> ModelPricing {
        let (p, src) = PricingTable::for_model(model);
        assert_eq!(src, ModelPricingSource::Known, "expected Known for {model}");
        p
    }

    fn prefix(model: &str) -> ModelPricing {
        let (p, src) = PricingTable::for_model(model);
        assert_eq!(
            src,
            ModelPricingSource::ProviderPrefix,
            "expected ProviderPrefix for {model}",
        );
        p
    }

    // ---- direct-family matching (existing behaviour) --------------------

    #[test]
    fn opus_pricing_resolves_for_canonical_id() {
        assert_eq!(known("claude-opus-4-7").input_per_mtok_cents, 1500);
    }

    #[test]
    fn opus_pricing_resolves_for_dated_variant() {
        assert_eq!(
            known("claude-opus-4-7-20250929").output_per_mtok_cents,
            7500
        );
    }

    #[test]
    fn sonnet_pricing_resolves() {
        assert_eq!(known("claude-sonnet-4-6").input_per_mtok_cents, 300);
    }

    #[test]
    fn haiku_pricing_resolves() {
        assert_eq!(known("claude-haiku-4-5-20251001").input_per_mtok_cents, 100);
    }

    // ---- OpenAI coverage -------------------------------------------------

    #[test]
    fn gpt_5_pricing_resolves() {
        assert_eq!(known("gpt-5").input_per_mtok_cents, 125);
    }

    #[test]
    fn gpt_5_mini_pricing_resolves_and_is_cheaper_than_gpt_5() {
        let m = known("gpt-5-mini");
        let base = known("gpt-5");
        assert!(m.input_per_mtok_cents < base.input_per_mtok_cents);
    }

    #[test]
    fn gpt_4o_pricing_resolves() {
        assert_eq!(known("gpt-4o").input_per_mtok_cents, 250);
    }

    #[test]
    fn gpt_4o_mini_pricing_resolves_and_is_cheaper_than_gpt_4o() {
        let m = known("gpt-4o-mini");
        let base = known("gpt-4o");
        assert!(m.input_per_mtok_cents < base.input_per_mtok_cents);
    }

    // ---- OpenRouter prefix stripping ------------------------------------

    #[test]
    fn openrouter_anthropic_prefix_strips_to_anthropic_pricing() {
        let via_or = prefix("openrouter/anthropic/claude-opus-4-7");
        let native = known("claude-opus-4-7");
        assert_eq!(via_or, native);
    }

    #[test]
    fn openrouter_openai_prefix_strips_to_openai_pricing() {
        let via_or = prefix("openrouter/openai/gpt-4o");
        let native = known("gpt-4o");
        assert_eq!(via_or, native);
    }

    #[test]
    fn openrouter_google_passthrough_is_unknown_until_we_add_gemini() {
        // No Gemini pricing yet — OpenRouter's `openrouter/google/*`
        // correctly falls through to Unknown rather than accidentally
        // aliasing to something else.
        let (_, src) = PricingTable::for_model("openrouter/google/gemini-2.5-pro");
        assert_eq!(src, ModelPricingSource::Unknown);
    }

    // ---- Alias forms ----------------------------------------------------

    #[test]
    fn anthropic_alias_resolves() {
        assert_eq!(known("anthropic/claude-opus-4-7"), known("claude-opus-4-7"));
    }

    #[test]
    fn openai_alias_resolves() {
        assert_eq!(known("openai/gpt-4o"), known("gpt-4o"));
    }

    // ---- Local providers explicit zero ----------------------------------

    #[test]
    fn ollama_model_resolves_to_free_with_known_source() {
        let p = known("ollama/llama3.1:70b");
        assert_eq!(p, ModelPricing::free());
    }

    #[test]
    fn lmstudio_and_vllm_and_llamacpp_all_resolve_free() {
        assert_eq!(known("lmstudio/qwen2.5"), ModelPricing::free());
        assert_eq!(known("vllm/mixtral-8x7b"), ModelPricing::free());
        assert_eq!(known("llama.cpp/some-local"), ModelPricing::free());
    }

    #[test]
    fn free_pricing_reports_zero_for_any_usage() {
        let p = ModelPricing::free();
        let usage = TokenUsage {
            input_tokens: 10_000_000,
            output_tokens: 5_000_000,
            cache_read_input_tokens: Some(1_000_000),
            cache_creation_input_tokens: Some(100_000),
        };
        assert_eq!(p.cost_cents(&usage), 0);
    }

    // ---- Unknown models -------------------------------------------------

    #[test]
    fn unknown_model_returns_unknown_source_without_panic() {
        let (p, src) = PricingTable::for_model("some-future-model-v99");
        assert_eq!(src, ModelPricingSource::Unknown);
        // Unknown resolves to a zero-cost placeholder so the caller
        // can still compute cost_cents() without a null check — the
        // source tells them not to enforce it.
        assert_eq!(p, ModelPricing::free());
    }

    #[test]
    fn empty_string_is_unknown_not_panic() {
        let (_, src) = PricingTable::for_model("");
        assert_eq!(src, ModelPricingSource::Unknown);
    }

    // ---- cost_cents() arithmetic (unchanged behaviour) ------------------

    #[test]
    fn cost_cents_handles_all_four_buckets() {
        let p = ModelPricing {
            input_per_mtok_cents: 1500,
            output_per_mtok_cents: 7500,
            cache_read_per_mtok_cents: 150,
            cache_write_per_mtok_cents: 1875,
        };
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            cache_read_input_tokens: Some(2_000_000),
            cache_creation_input_tokens: Some(100_000),
        };
        assert_eq!(p.cost_cents(&usage), 5738);
    }

    #[test]
    fn cost_cents_rounds_up_tiny_usage() {
        let p = ModelPricing {
            input_per_mtok_cents: 1500,
            output_per_mtok_cents: 7500,
            cache_read_per_mtok_cents: 150,
            cache_write_per_mtok_cents: 1875,
        };
        let usage = TokenUsage {
            input_tokens: 1,
            ..Default::default()
        };
        assert_eq!(p.cost_cents(&usage), 1);
    }

    #[test]
    fn cost_cents_zero_usage_is_zero() {
        let p = known("claude-opus-4-7");
        assert_eq!(p.cost_cents(&TokenUsage::default()), 0);
    }

    #[test]
    fn cost_cents_caps_at_u32_max() {
        let p = ModelPricing {
            input_per_mtok_cents: u32::MAX,
            output_per_mtok_cents: u32::MAX,
            cache_read_per_mtok_cents: u32::MAX,
            cache_write_per_mtok_cents: u32::MAX,
        };
        let usage = TokenUsage {
            input_tokens: u32::MAX,
            output_tokens: u32::MAX,
            cache_read_input_tokens: Some(u32::MAX),
            cache_creation_input_tokens: Some(u32::MAX),
        };
        // Should saturate, not panic.
        let _ = p.cost_cents(&usage);
    }
}
