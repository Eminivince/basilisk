//! Typed configuration loaded from environment variables (and optionally a
//! `.env` file via `dotenvy`).
//!
//! API keys are `Option<String>` during Phase 1 — nothing is required to run
//! `--help` or the stub `recon` command. Features that need a specific key
//! will enforce its presence at the point of use.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Default value for [`Config::log_level`] when neither env nor config sets it.
pub const DEFAULT_LOG_LEVEL: &str = "info";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
}

fn default_log_level() -> String {
    DEFAULT_LOG_LEVEL.to_string()
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

        Ok(Self {
            anthropic_api_key: non_empty_env("ANTHROPIC_API_KEY"),
            etherscan_api_key: non_empty_env("ETHERSCAN_API_KEY"),
            alchemy_api_key: non_empty_env("ALCHEMY_API_KEY"),
            github_token: non_empty_env("GITHUB_TOKEN"),
            log_level: non_empty_env("LOG_LEVEL").unwrap_or_else(default_log_level),
        })
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}
