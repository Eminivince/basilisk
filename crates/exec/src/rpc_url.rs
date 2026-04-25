//! RPC URL resolution for forking.
//!
//! Three sources, in priority order:
//!
//!   1. `MAINNET_RPC_URL` (or `RPC_URL_<CHAIN>`) environment variable —
//!      explicit URLs win.
//!   2. The chain-keyed entries in [`Config::rpc_urls`].
//!   3. `ALCHEMY_API_KEY` constructed into a chain-appropriate template.
//!
//! Returns [`ExecError::NoRpcUrl`] when nothing resolves.

use basilisk_core::Config;

use crate::{error::ExecError, types::ForkChain};

/// Resolve an upstream RPC URL for the given chain.
///
/// `cfg` is consulted for `rpc_urls` and `alchemy_api_key`; env vars
/// are consulted for `MAINNET_RPC_URL` and `RPC_URL_<CHAIN>` (these
/// override the config — operators iterate via env in practice).
pub fn resolve_rpc_url(cfg: &Config, chain: ForkChain) -> Result<String, ExecError> {
    // Env-var override: `MAINNET_RPC_URL` for ethereum, else
    // `RPC_URL_<CHAIN>`.
    if matches!(chain, ForkChain::Ethereum) {
        if let Some(v) = std::env::var("MAINNET_RPC_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
        {
            return Ok(v);
        }
    }
    let env_key = format!("RPC_URL_{}", chain.canonical().to_ascii_uppercase());
    if let Some(v) = std::env::var(&env_key)
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        return Ok(v);
    }

    // Config-loaded rpc_urls (populated from RPC_URL_<CHAIN>; see
    // crates/core/src/config.rs::collect_prefixed_env). The loader
    // lower-cases the key.
    if let Some(v) = cfg.rpc_urls.get(chain.canonical()) {
        if !v.trim().is_empty() {
            return Ok(v.clone());
        }
    }

    // Alchemy fallback: template per chain.
    if let Some(key) = cfg
        .alchemy_api_key
        .as_ref()
        .filter(|s| !s.trim().is_empty())
    {
        let template = match chain {
            ForkChain::Ethereum => "https://eth-mainnet.g.alchemy.com/v2/",
            ForkChain::Optimism => "https://opt-mainnet.g.alchemy.com/v2/",
            ForkChain::Arbitrum => "https://arb-mainnet.g.alchemy.com/v2/",
            ForkChain::Polygon => "https://polygon-mainnet.g.alchemy.com/v2/",
            ForkChain::Base => "https://base-mainnet.g.alchemy.com/v2/",
            // Alchemy doesn't host BNB; require an explicit URL.
            ForkChain::Bnb => return Err(ExecError::NoRpcUrl { chain: chain.canonical().into() }),
        };
        return Ok(format!("{template}{key}"));
    }

    Err(ExecError::NoRpcUrl {
        chain: chain.canonical().into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Env vars are process-global; serialize the env-touching tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn explicit_mainnet_rpc_url_wins() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("MAINNET_RPC_URL", "https://explicit.example/eth");
        let cfg = Config::default();
        let url = resolve_rpc_url(&cfg, ForkChain::Ethereum).unwrap();
        assert_eq!(url, "https://explicit.example/eth");
        std::env::remove_var("MAINNET_RPC_URL");
    }

    #[test]
    fn env_rpc_url_for_other_chain() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("RPC_URL_ARBITRUM", "https://arb.example/rpc");
        let cfg = Config::default();
        let url = resolve_rpc_url(&cfg, ForkChain::Arbitrum).unwrap();
        assert_eq!(url, "https://arb.example/rpc");
        std::env::remove_var("RPC_URL_ARBITRUM");
    }

    #[test]
    fn config_rpc_urls_used_when_env_missing() {
        let _g = ENV_LOCK.lock().unwrap();
        // Ensure no env interference.
        std::env::remove_var("MAINNET_RPC_URL");
        std::env::remove_var("RPC_URL_ETHEREUM");
        let mut rpc_urls = std::collections::HashMap::new();
        rpc_urls.insert("ethereum".into(), "https://config.example/eth".into());
        let cfg = Config {
            rpc_urls,
            ..Config::default()
        };
        let url = resolve_rpc_url(&cfg, ForkChain::Ethereum).unwrap();
        assert_eq!(url, "https://config.example/eth");
    }

    #[test]
    fn alchemy_key_constructs_url() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("MAINNET_RPC_URL");
        std::env::remove_var("RPC_URL_ETHEREUM");
        let cfg = Config {
            alchemy_api_key: Some("ABC123".into()),
            ..Config::default()
        };
        let url = resolve_rpc_url(&cfg, ForkChain::Ethereum).unwrap();
        assert_eq!(url, "https://eth-mainnet.g.alchemy.com/v2/ABC123");
    }

    #[test]
    fn missing_returns_typed_error() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("MAINNET_RPC_URL");
        std::env::remove_var("RPC_URL_ETHEREUM");
        let cfg = Config::default();
        let err = resolve_rpc_url(&cfg, ForkChain::Ethereum).unwrap_err();
        match err {
            ExecError::NoRpcUrl { chain } => assert_eq!(chain, "ethereum"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn bnb_alchemy_unsupported() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("RPC_URL_BNB");
        let cfg = Config {
            alchemy_api_key: Some("XYZ".into()),
            ..Config::default()
        };
        let err = resolve_rpc_url(&cfg, ForkChain::Bnb).unwrap_err();
        assert!(matches!(err, ExecError::NoRpcUrl { .. }));
    }
}
