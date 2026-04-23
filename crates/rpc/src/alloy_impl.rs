//! [`RpcProvider`] backed by `alloy-provider` over HTTP.
//!
//! Wraps a `RootProvider` with our narrower trait, adds URL-redacting
//! reporting, bytecode caching (namespace `"bytecode"`, 1h TTL), and the
//! shared [`crate::retry::with_retry`] loop.

use std::{sync::Arc, time::Duration};

use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_transport_http::{Client, Http};
use async_trait::async_trait;
use basilisk_cache::Cache;
use basilisk_core::{Chain, Config};
use url::Url;

use crate::{
    error::RpcError,
    provider::RpcProvider,
    resolver::{resolve_rpc_url, ResolvedEndpoint},
    retry::with_retry,
};

/// TTL for cached bytecode entries. Short because proxies may be upgraded.
const BYTECODE_TTL: Duration = Duration::from_secs(60 * 60);
/// Cache namespace for bytecode.
const BYTECODE_NAMESPACE: &str = "bytecode";

type HttpProvider = RootProvider<Http<Client>>;

/// Default [`RpcProvider`] implementation.
#[derive(Clone)]
pub struct AlloyProvider {
    chain: Chain,
    endpoint: ResolvedEndpoint,
    inner: Arc<HttpProvider>,
    cache: Option<Cache>,
}

impl std::fmt::Debug for AlloyProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlloyProvider")
            .field("chain", &self.chain.canonical_name())
            .field("endpoint", &self.endpoint.redacted_url())
            .field("source", &self.endpoint.source)
            .field("cache_enabled", &self.cache.is_some())
            .finish_non_exhaustive()
    }
}

impl AlloyProvider {
    /// Resolve an endpoint for `chain` and construct a provider.
    pub fn for_chain(chain: &Chain, config: &Config) -> Result<Self, RpcError> {
        let endpoint = resolve_rpc_url(chain, config)?;
        let url: Url = endpoint
            .url
            .parse()
            .map_err(|e: url::ParseError| RpcError::InvalidUrl {
                url: endpoint.redacted_url(),
                detail: e.to_string(),
            })?;
        let inner = ProviderBuilder::new().on_http(url);
        let cache = Cache::open(BYTECODE_NAMESPACE).ok();
        Ok(Self {
            chain: chain.clone(),
            endpoint,
            inner: Arc::new(inner),
            cache,
        })
    }

    /// Disable the bytecode cache for this handle (used by `--no-cache`).
    #[must_use]
    pub fn without_cache(mut self) -> Self {
        self.cache = None;
        self
    }

    fn cache_key(&self, address: &Address) -> String {
        format!("{}:{address}", self.chain.canonical_name())
    }
}

#[async_trait]
impl RpcProvider for AlloyProvider {
    fn chain(&self) -> &Chain {
        &self.chain
    }

    fn endpoint(&self) -> String {
        self.endpoint.redacted_url()
    }

    async fn get_code(&self, address: Address) -> Result<Bytes, RpcError> {
        if let Some(cache) = &self.cache {
            match cache.get::<Bytes>(&self.cache_key(&address)).await {
                Ok(Some(hit)) => return Ok(hit.value),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        chain = self.chain.canonical_name(),
                        error = %e,
                        "bytecode cache read failed; falling through to RPC",
                    );
                }
            }
        }

        let bytes = with_retry(|| async {
            self.inner
                .get_code_at(address)
                .await
                .map_err(map_alloy_error)
        })
        .await?;

        if let Some(cache) = &self.cache {
            if let Err(e) = cache
                .put(&self.cache_key(&address), &bytes, BYTECODE_TTL)
                .await
            {
                tracing::warn!(
                    chain = self.chain.canonical_name(),
                    error = %e,
                    "bytecode cache write failed; continuing without persistence",
                );
            }
        }

        Ok(bytes)
    }

    async fn get_storage_at(&self, address: Address, slot: B256) -> Result<B256, RpcError> {
        let slot_u = U256::from_be_bytes(slot.0);
        let value = with_retry(|| async {
            self.inner
                .get_storage_at(address, slot_u)
                .await
                .map_err(map_alloy_error)
        })
        .await?;
        Ok(B256::from(value.to_be_bytes()))
    }

    async fn call(&self, to: Address, data: Bytes) -> Result<Bytes, RpcError> {
        let tx = TransactionRequest::default().to(to).input(data.into());
        with_retry(|| async { self.inner.call(&tx).await.map_err(map_alloy_error) }).await
    }

    async fn chain_id(&self) -> Result<u64, RpcError> {
        with_retry(|| async { self.inner.get_chain_id().await.map_err(map_alloy_error) }).await
    }
}

/// Best-effort classification of transport errors.
fn map_alloy_error<E: std::fmt::Display>(err: E) -> RpcError {
    let msg = err.to_string();
    let lower = msg.to_ascii_lowercase();
    if lower.contains("429") || lower.contains("rate limit") {
        RpcError::RateLimited
    } else if lower.contains("timeout") || lower.contains("timed out") {
        RpcError::Timeout { secs: 0 }
    } else if lower.contains("connection") || lower.contains("reset") || lower.contains("503") {
        RpcError::Transient(msg)
    } else {
        RpcError::Server(msg)
    }
}

#[cfg(test)]
mod tests {
    use basilisk_core::Config;

    use super::*;

    #[test]
    fn for_chain_fails_without_config_on_testnet() {
        let config = Config::default();
        let err = AlloyProvider::for_chain(&Chain::Sepolia, &config).unwrap_err();
        assert!(
            matches!(err, RpcError::NoProviderConfigured { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn for_chain_constructs_when_alchemy_key_set() {
        let config = Config {
            alchemy_api_key: Some("key".into()),
            ..Config::default()
        };
        let p = AlloyProvider::for_chain(&Chain::EthereumMainnet, &config).unwrap();
        assert_eq!(p.chain(), &Chain::EthereumMainnet);
        assert!(
            p.endpoint().contains("***"),
            "endpoint should redact the key: {}",
            p.endpoint()
        );
    }

    #[test]
    fn for_chain_constructs_with_user_rpc_url() {
        let mut rpc_urls = std::collections::HashMap::new();
        rpc_urls.insert("sepolia".into(), "https://example.test/sep".into());
        let config = Config {
            rpc_urls,
            ..Config::default()
        };
        let p = AlloyProvider::for_chain(&Chain::Sepolia, &config).unwrap();
        assert_eq!(p.endpoint(), "https://example.test/sep");
    }

    #[test]
    fn invalid_url_classified() {
        let mut rpc_urls = std::collections::HashMap::new();
        rpc_urls.insert("ethereum".into(), "not a url".into());
        let config = Config {
            rpc_urls,
            ..Config::default()
        };
        let err = AlloyProvider::for_chain(&Chain::EthereumMainnet, &config).unwrap_err();
        assert!(matches!(err, RpcError::InvalidUrl { .. }), "got {err:?}");
    }

    #[test]
    fn map_alloy_error_classifies_common_cases() {
        assert!(matches!(
            map_alloy_error("HTTP 429 Too Many Requests"),
            RpcError::RateLimited
        ));
        assert!(matches!(
            map_alloy_error("request timed out"),
            RpcError::Timeout { .. }
        ));
        assert!(matches!(
            map_alloy_error("connection reset by peer"),
            RpcError::Transient(_)
        ));
        assert!(matches!(
            map_alloy_error("unknown method"),
            RpcError::Server(_)
        ));
    }
}
