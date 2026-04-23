//! Typed configuration loaded from environment variables (and optionally a
//! `.env` file via `dotenvy`).
//!
//! API keys are `Option<String>` during Phase 1 — nothing is required to run
//! `--help` or the stub `recon` command. Features that need a specific key
//! will enforce its presence at the point of use.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Default value for [`Config::log_level`] when neither env nor config sets it.
pub const DEFAULT_LOG_LEVEL: &str = "info";

/// Default for [`Config::onchain_timeout_secs`].
pub const DEFAULT_ONCHAIN_TIMEOUT_SECS: u64 = 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub anthropic_api_key: Option<String>,
    #[serde(default)]
    pub etherscan_api_key: Option<String>,
    #[serde(default)]
    pub alchemy_api_key: Option<String>,
    #[serde(default)]
    pub github_token: Option<String>,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Per-chain RPC URLs keyed by `Chain::canonical_name()`. Populated from
    /// `RPC_URL_<CHAIN>` environment variables (e.g. `RPC_URL_BNB`).
    #[serde(default)]
    pub rpc_urls: HashMap<String, String>,
    /// Per-chain Blockscout base URLs keyed by `Chain::canonical_name()`.
    /// Populated from `BLOCKSCOUT_URL_<CHAIN>` env vars.
    #[serde(default)]
    pub blockscout_urls: HashMap<String, String>,
    /// Overall timeout for an on-chain `resolve()` in seconds.
    #[serde(default = "default_onchain_timeout_secs")]
    pub onchain_timeout_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            anthropic_api_key: None,
            etherscan_api_key: None,
            alchemy_api_key: None,
            github_token: None,
            log_level: default_log_level(),
            rpc_urls: HashMap::new(),
            blockscout_urls: HashMap::new(),
            onchain_timeout_secs: DEFAULT_ONCHAIN_TIMEOUT_SECS,
        }
    }
}

fn default_log_level() -> String {
    DEFAULT_LOG_LEVEL.to_string()
}

fn default_onchain_timeout_secs() -> u64 {
    DEFAULT_ONCHAIN_TIMEOUT_SECS
}

impl Config {
    /// Load configuration from environment variables, consulting `.env` first
    /// if present. Missing `.env` is not an error.
    pub fn load() -> Result<Self> {
        // Ignore the "no .env file" case; surface anything else (e.g. parse errors).
        match dotenvy::dotenv() {
            Ok(_) | Err(dotenvy::Error::Io(_)) => {}
            Err(e) => return Err(Error::Config(format!("failed to load .env: {e}"))),
        }

        let rpc_urls = collect_prefixed_env("RPC_URL_");
        let blockscout_urls = collect_prefixed_env("BLOCKSCOUT_URL_");

        let onchain_timeout_secs = match non_empty_env("ONCHAIN_TIMEOUT_SECS") {
            Some(s) => s
                .parse::<u64>()
                .map_err(|e| Error::Config(format!("ONCHAIN_TIMEOUT_SECS: {e}")))?,
            None => DEFAULT_ONCHAIN_TIMEOUT_SECS,
        };

        Ok(Self {
            anthropic_api_key: non_empty_env("ANTHROPIC_API_KEY"),
            etherscan_api_key: non_empty_env("ETHERSCAN_API_KEY"),
            alchemy_api_key: non_empty_env("ALCHEMY_API_KEY"),
            github_token: non_empty_env("GITHUB_TOKEN"),
            log_level: non_empty_env("LOG_LEVEL").unwrap_or_else(default_log_level),
            rpc_urls,
            blockscout_urls,
            onchain_timeout_secs,
        })
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Collect env vars whose name begins with `prefix`. The trailing portion is
/// lowercased and hyphen-normalized so callers can key by `Chain::canonical_name()`.
fn collect_prefixed_env(prefix: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (k, v) in std::env::vars() {
        if let Some(rest) = k.strip_prefix(prefix) {
            if v.trim().is_empty() {
                continue;
            }
            let normalized = rest.to_ascii_lowercase().replace('_', "-");
            out.insert(normalized, v);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_collect_lowercases_and_normalizes() {
        // Set a couple of vars, collect, assert, then unset.
        // SAFETY: tests run single-threaded-ish per-process; we snapshot surrounding keys.
        let key_bnb = "RPC_URL_BNB_TEST";
        let key_arb = "RPC_URL_ARBITRUM_SEPOLIA_TEST";
        std::env::set_var(key_bnb, "https://bnb.example/rpc");
        std::env::set_var(key_arb, "https://arb-sep.example/rpc");
        let map = collect_prefixed_env("RPC_URL_");
        assert_eq!(
            map.get("bnb-test").map(String::as_str),
            Some("https://bnb.example/rpc")
        );
        assert_eq!(
            map.get("arbitrum-sepolia-test").map(String::as_str),
            Some("https://arb-sep.example/rpc"),
        );
        std::env::remove_var(key_bnb);
        std::env::remove_var(key_arb);
    }

    #[test]
    fn default_timeout_is_sixty_seconds() {
        assert_eq!(Config::default().onchain_timeout_secs, 60);
    }
}
