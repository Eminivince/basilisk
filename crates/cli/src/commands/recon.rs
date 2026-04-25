//! `audit recon <target>` — agent-driven reconnaissance and
//! vulnerability hunting.
//!
//! Set 9.5 / CP9.5.3 deleted the deterministic ingestion dispatch
//! (the path that ran without `--agent`). `audit recon` always runs
//! the LLM-driven agent loop now: recon-mode by default, vuln-mode
//! when `--vuln` is set.
//!
//! What the agent does:
//!   - Recon mode: classify the target, resolve on-chain systems,
//!     pull verified source, characterize the architecture, finalize
//!     a recon brief.
//!   - Vuln mode: same plus hypothesize, simulate, optionally
//!     write+run a Foundry test, surface limitations and suspicions
//!     through the structured-recording channels, finalize after
//!     self-critique.
//!
//! The deterministic path was Sets 1–5's deliverable and Set 9's
//! regression fixture. We trust the agent enough now to make it the
//! sole path. See `reports/SET-9.md` and `reports/wave-launcher-trial.md`
//! for the validation that justifies the deletion.

use anyhow::Result;
use basilisk_core::Config;
use clap::Args;

use crate::commands::agent_runner::{self, AgentFlags};

#[derive(Debug, Args)]
pub struct ReconArgs {
    /// Target to audit: a URL, address, path, or free-form string.
    /// Plain hex addresses default to ethereum mainnet; pass
    /// `--chain <name>` to disambiguate. The agent's
    /// `classify_target` tool resolves the rest.
    pub target: String,

    /// Optional chain hint when the target is an on-chain address.
    /// Examples: `ethereum`, `arbitrum`, `base`, `polygon`,
    /// `optimism`, `bnb`. Prepended to the target as
    /// `<chain>/<address>` so the agent's `classify_target` picks it
    /// up. Ignored for non-address targets.
    #[arg(long)]
    pub chain: Option<String>,

    #[command(flatten)]
    pub agent_flags: AgentFlags,
}

pub async fn run(args: &ReconArgs, config: &Config) -> Result<()> {
    let target = format_target(&args.target, args.chain.as_deref());
    agent_runner::run_agent(&target, &args.agent_flags, config).await
}

/// Apply the `--chain` hint to a hex-address target by prefixing it
/// as `<chain>/<address>` — the format the agent's `classify_target`
/// tool already understands and that `audit bench run` uses
/// internally. Targets that aren't hex addresses pass through as-is.
fn format_target(raw: &str, chain: Option<&str>) -> String {
    let trimmed = raw.trim();
    let looks_like_address = trimmed.starts_with("0x") && !trimmed.contains('/');
    match (chain, looks_like_address) {
        (Some(c), true) => format!("{c}/{trimmed}"),
        _ => raw.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_target_prepends_chain_to_bare_address() {
        let s = format_target(
            "0xB9873b482d51b8b0f989DCD6CCf1D91520092b95",
            Some("ethereum"),
        );
        assert_eq!(s, "ethereum/0xB9873b482d51b8b0f989DCD6CCf1D91520092b95");
    }

    #[test]
    fn format_target_passes_through_when_no_chain() {
        let s = format_target("0xabc", None);
        assert_eq!(s, "0xabc");
    }

    #[test]
    fn format_target_does_not_double_prepend() {
        let s = format_target("optimism/0xabc", Some("ethereum"));
        // The address has a slash already → assume the operator
        // already chain-tagged it; don't override.
        assert_eq!(s, "optimism/0xabc");
    }

    #[test]
    fn format_target_passes_through_for_paths_and_urls() {
        assert_eq!(
            format_target("./contracts/Foo.sol", Some("ethereum")),
            "./contracts/Foo.sol"
        );
        assert_eq!(
            format_target("https://github.com/foo/bar", Some("ethereum")),
            "https://github.com/foo/bar"
        );
    }
}
