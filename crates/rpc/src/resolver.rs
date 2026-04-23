//! RPC URL resolution.
//!
//! Strategy (first match wins):
//! 1. Alchemy: if `config.alchemy_api_key` is set and the chain is supported
//!    by Alchemy, use `https://<prefix>.g.alchemy.com/v2/<key>`.
//! 2. User-configured: `config.rpc_urls[chain.canonical_name()]` (populated
//!    from `RPC_URL_<CHAIN>` environment variables).
//! 3. Built-in public fallback for a handful of mainnet chains.
//! 4. Otherwise [`RpcError::NoProviderConfigured`] with a helpful suggestion.

use basilisk_core::{Chain, Config};
use serde::{Deserialize, Serialize};

use crate::error::RpcError;

/// Which source supplied the endpoint URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RpcSource {
    /// Alchemy over the user's API key.
    Alchemy,
    /// URL supplied via `RPC_URL_<CHAIN>` / `config.rpc_urls`.
    UserConfig,
    /// Built-in public endpoint (best-effort, may be rate limited).
    PublicFallback,
}

/// Successful URL resolution result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEndpoint {
    pub url: String,
    pub source: RpcSource,
}

impl ResolvedEndpoint {
    /// Render the URL with any API-key query parameter or path segment redacted.
    ///
    /// The redaction is conservative: for Alchemy URLs we replace the key in
    /// `/v2/<key>`; otherwise we leave the URL alone since we don't know which
    /// components are sensitive.
    pub fn redacted_url(&self) -> String {
        redact_alchemy_key(&self.url)
    }
}

/// Resolve an RPC URL for `chain` under `config`.
pub fn resolve_rpc_url(chain: &Chain, config: &Config) -> Result<ResolvedEndpoint, RpcError> {
    if let Some(key) = config.alchemy_api_key.as_deref() {
        if let Some(prefix) = alchemy_prefix(chain) {
            return Ok(ResolvedEndpoint {
                url: format!("https://{prefix}.g.alchemy.com/v2/{key}"),
                source: RpcSource::Alchemy,
            });
        }
    }

    if let Some(url) = config.rpc_urls.get(chain.canonical_name()) {
        return Ok(ResolvedEndpoint {
            url: url.clone(),
            source: RpcSource::UserConfig,
        });
    }

    if let Some(url) = public_fallback(chain) {
        tracing::warn!(
            chain = chain.canonical_name(),
            url,
            "using public RPC fallback; consider setting an Alchemy API key \
             or RPC_URL_<CHAIN> for reliability",
        );
        return Ok(ResolvedEndpoint {
            url: url.to_string(),
            source: RpcSource::PublicFallback,
        });
    }

    Err(RpcError::NoProviderConfigured {
        chain: chain.canonical_name().to_string(),
        suggestion: format!(
            "set ALCHEMY_API_KEY (if supported), or RPC_URL_{} in your environment",
            chain
                .canonical_name()
                .to_ascii_uppercase()
                .replace('-', "_"),
        ),
    })
}

/// Alchemy subdomain prefix for chains Alchemy supports. `None` for chains
/// Alchemy doesn't cover (Bnb, Avalanche, `Other`).
fn alchemy_prefix(chain: &Chain) -> Option<&'static str> {
    Some(match chain {
        Chain::EthereumMainnet => "eth-mainnet",
        Chain::Sepolia => "eth-sepolia",
        Chain::Arbitrum => "arb-mainnet",
        Chain::ArbitrumSepolia => "arb-sepolia",
        Chain::Base => "base-mainnet",
        Chain::BaseSepolia => "base-sepolia",
        Chain::Optimism => "opt-mainnet",
        Chain::OptimismSepolia => "opt-sepolia",
        Chain::Polygon => "polygon-mainnet",
        Chain::Bnb | Chain::Avalanche | Chain::Other { .. } => return None,
    })
}

/// Public RPC fallback for mainnets we can reasonably depend on without a key.
fn public_fallback(chain: &Chain) -> Option<&'static str> {
    Some(match chain {
        Chain::EthereumMainnet => "https://eth.llamarpc.com",
        Chain::Arbitrum => "https://arb1.arbitrum.io/rpc",
        Chain::Base => "https://mainnet.base.org",
        Chain::Optimism => "https://mainnet.optimism.io",
        Chain::Polygon => "https://polygon-rpc.com",
        Chain::Bnb => "https://bsc-dataseed.binance.org",
        Chain::Avalanche => "https://api.avax.network/ext/bc/C/rpc",
        // Testnets + Other: require explicit configuration.
        Chain::Sepolia
        | Chain::ArbitrumSepolia
        | Chain::BaseSepolia
        | Chain::OptimismSepolia
        | Chain::Other { .. } => return None,
    })
}

fn redact_alchemy_key(url: &str) -> String {
    if let Some(idx) = url.find("/v2/") {
        let tail_start = idx + "/v2/".len();
        let tail = &url[tail_start..];
        let next_sep = tail.find(['/', '?']).unwrap_or(tail.len());
        let mut out = String::with_capacity(url.len());
        out.push_str(&url[..tail_start]);
        out.push_str("***");
        out.push_str(&tail[next_sep..]);
        out
    } else {
        url.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::default()
    }

    #[test]
    fn alchemy_wins_when_key_set_and_chain_supported() {
        let mut c = cfg();
        c.alchemy_api_key = Some("test-key".into());
        let r = resolve_rpc_url(&Chain::EthereumMainnet, &c).unwrap();
        assert_eq!(r.source, RpcSource::Alchemy);
        assert_eq!(r.url, "https://eth-mainnet.g.alchemy.com/v2/test-key");
    }

    #[test]
    fn alchemy_ignored_when_chain_unsupported() {
        let mut c = cfg();
        c.alchemy_api_key = Some("test-key".into());
        // Bnb isn't Alchemy-supported; fall through to public fallback.
        let r = resolve_rpc_url(&Chain::Bnb, &c).unwrap();
        assert_eq!(r.source, RpcSource::PublicFallback);
    }

    #[test]
    fn user_config_wins_over_public_fallback() {
        let mut c = cfg();
        c.rpc_urls
            .insert("ethereum".into(), "https://priv.example/eth".into());
        let r = resolve_rpc_url(&Chain::EthereumMainnet, &c).unwrap();
        assert_eq!(r.source, RpcSource::UserConfig);
        assert_eq!(r.url, "https://priv.example/eth");
    }

    #[test]
    fn alchemy_wins_over_user_config() {
        let mut c = cfg();
        c.alchemy_api_key = Some("k".into());
        c.rpc_urls
            .insert("ethereum".into(), "https://priv.example/eth".into());
        let r = resolve_rpc_url(&Chain::EthereumMainnet, &c).unwrap();
        assert_eq!(r.source, RpcSource::Alchemy);
    }

    #[test]
    fn public_fallback_for_bnb() {
        let r = resolve_rpc_url(&Chain::Bnb, &cfg()).unwrap();
        assert_eq!(r.source, RpcSource::PublicFallback);
        assert!(r.url.contains("bsc"));
    }

    #[test]
    fn testnet_without_config_is_no_provider() {
        let err = resolve_rpc_url(&Chain::Sepolia, &cfg()).unwrap_err();
        match err {
            RpcError::NoProviderConfigured { chain, suggestion } => {
                assert_eq!(chain, "sepolia");
                assert!(suggestion.contains("RPC_URL_SEPOLIA"), "got {suggestion:?}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn other_chain_requires_config() {
        let other = Chain::Other {
            chain_id: 31_337,
            name: "anvil".into(),
        };
        let err = resolve_rpc_url(&other, &cfg()).unwrap_err();
        assert!(matches!(err, RpcError::NoProviderConfigured { .. }));
    }

    #[test]
    fn other_chain_picks_up_user_config() {
        let mut c = cfg();
        c.rpc_urls
            .insert("anvil".into(), "http://127.0.0.1:8545".into());
        let other = Chain::Other {
            chain_id: 31_337,
            name: "anvil".into(),
        };
        let r = resolve_rpc_url(&other, &c).unwrap();
        assert_eq!(r.source, RpcSource::UserConfig);
        assert_eq!(r.url, "http://127.0.0.1:8545");
    }

    #[test]
    fn redacts_alchemy_key() {
        let r = ResolvedEndpoint {
            url: "https://eth-mainnet.g.alchemy.com/v2/supersecret".into(),
            source: RpcSource::Alchemy,
        };
        assert_eq!(r.redacted_url(), "https://eth-mainnet.g.alchemy.com/v2/***");
    }

    #[test]
    fn redacts_alchemy_key_with_trailing_path() {
        let r = ResolvedEndpoint {
            url: "https://eth-mainnet.g.alchemy.com/v2/supersecret/foo".into(),
            source: RpcSource::Alchemy,
        };
        assert_eq!(
            r.redacted_url(),
            "https://eth-mainnet.g.alchemy.com/v2/***/foo"
        );
    }

    #[test]
    fn redaction_is_idempotent_for_non_alchemy_urls() {
        let r = ResolvedEndpoint {
            url: "https://bsc-dataseed.binance.org".into(),
            source: RpcSource::PublicFallback,
        };
        assert_eq!(r.redacted_url(), "https://bsc-dataseed.binance.org");
    }
}
