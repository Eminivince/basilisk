//! Per-model pricing. Used by the agent to estimate + enforce cost budgets.
//!
//! Prices quoted in **cents per million tokens** (integer; one cent is
//! the cheapest unit the cost budget needs to express). Update when
//! Anthropic adjusts list prices.
//!
//! Only informational — stale numbers produce slightly-wrong cost
//! estimates, not broken functionality.

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
}

/// Static table of pricing for every model we know about.
///
/// Looked up by the provider-qualified identifier the backend reports
/// (e.g. `"claude-opus-4-7"`). Unknown models return `None` — callers
/// treat that as "cost estimation unavailable, charge forward".
pub struct PricingTable;

impl PricingTable {
    /// Current list prices as of 2026-04. Sourced from anthropic.com/pricing.
    /// When Anthropic updates, update here; not tested for recency.
    pub fn for_model(model: &str) -> Option<ModelPricing> {
        // Anthropic naming: `claude-opus-4-7-20250929` etc. We match the
        // family prefix so minor version bumps don't require changes.
        let m = model.to_ascii_lowercase();
        if m.starts_with("claude-opus-4") {
            Some(ModelPricing {
                // $15 / 1M input, $75 / 1M output, $1.50 cache read, $18.75 cache write.
                input_per_mtok_cents: 1500,
                output_per_mtok_cents: 7500,
                cache_read_per_mtok_cents: 150,
                cache_write_per_mtok_cents: 1875,
            })
        } else if m.starts_with("claude-sonnet-4") {
            Some(ModelPricing {
                // $3 / 1M input, $15 / 1M output, $0.30 cache read, $3.75 cache write.
                input_per_mtok_cents: 300,
                output_per_mtok_cents: 1500,
                cache_read_per_mtok_cents: 30,
                cache_write_per_mtok_cents: 375,
            })
        } else if m.starts_with("claude-haiku-4") {
            Some(ModelPricing {
                // $1 / 1M input, $5 / 1M output, $0.10 cache read, $1.25 cache write.
                input_per_mtok_cents: 100,
                output_per_mtok_cents: 500,
                cache_read_per_mtok_cents: 10,
                cache_write_per_mtok_cents: 125,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_pricing_resolves_for_canonical_id() {
        let p = PricingTable::for_model("claude-opus-4-7").unwrap();
        assert_eq!(p.input_per_mtok_cents, 1500);
    }

    #[test]
    fn opus_pricing_resolves_for_dated_variant() {
        let p = PricingTable::for_model("claude-opus-4-7-20250929").unwrap();
        assert_eq!(p.output_per_mtok_cents, 7500);
    }

    #[test]
    fn sonnet_pricing_resolves() {
        let p = PricingTable::for_model("claude-sonnet-4-6").unwrap();
        assert_eq!(p.input_per_mtok_cents, 300);
    }

    #[test]
    fn haiku_pricing_resolves() {
        let p = PricingTable::for_model("claude-haiku-4-5-20251001").unwrap();
        assert_eq!(p.input_per_mtok_cents, 100);
    }

    #[test]
    fn unknown_model_returns_none() {
        assert!(PricingTable::for_model("gpt-5").is_none());
    }

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
        // 1500 + (7500/2) + (150*2) + ceil(1875/10)
        // = 1500 + 3750 + 300 + 188 = 5738
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
        // One input token at $15/M → 0.0015 cents → ceil to 1 cent.
        let usage = TokenUsage {
            input_tokens: 1,
            ..Default::default()
        };
        assert_eq!(p.cost_cents(&usage), 1);
    }

    #[test]
    fn cost_cents_zero_usage_is_zero() {
        let p = PricingTable::for_model("claude-opus-4-7").unwrap();
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
