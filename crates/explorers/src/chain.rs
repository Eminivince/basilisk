//! Fallback chain that composes multiple [`crate::SourceExplorer`]s.
//!
//! The chain iterates explorers in order. The first one to return
//! `Ok(Some(source))` wins and short-circuits the chain. All attempts
//! (successful, unverified, errored) are recorded in the returned
//! [`ResolutionAttempt`] for audit.
//!
//! On-disk caching is applied at this layer: the winning `(explorer, source)`
//! is stored under namespace `verified_source` with a 24h TTL; a sentinel
//! "miss" is stored for 5 minutes so repeated runs don't hammer explorers.

use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use basilisk_cache::Cache;
use basilisk_core::{Chain, Config};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::{
    blockscout::Blockscout,
    error::ExplorerError,
    etherscan::Etherscan,
    source_explorer::SourceExplorer,
    sourcify::Sourcify,
    types::{ExplorerAttempt, ExplorerOutcome, MatchQuality, ResolutionAttempt, VerifiedSource},
};

/// Cache namespace for verified-source payloads.
pub const CACHE_NAMESPACE: &str = "verified_source";
/// TTL for a cached verified-source hit.
pub const HIT_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// TTL for a cached "not verified anywhere" entry.
pub const MISS_TTL: Duration = Duration::from_secs(5 * 60);

/// Composed explorer chain.
pub struct ExplorerChain {
    explorers: Vec<Arc<dyn SourceExplorer>>,
    cache: Option<Cache>,
}

impl std::fmt::Debug for ExplorerChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&'static str> = self.explorers.iter().map(|e| e.name()).collect();
        f.debug_struct("ExplorerChain")
            .field("explorers", &names)
            .field("cache", &self.cache.as_ref().map(Cache::namespace))
            .finish()
    }
}

impl ExplorerChain {
    /// Build a chain from an explicit explorer list (order-sensitive).
    pub fn new(explorers: Vec<Arc<dyn SourceExplorer>>) -> Self {
        let cache = Cache::open(CACHE_NAMESPACE).ok();
        Self { explorers, cache }
    }

    /// Build the default fallback chain for the given `chain` + `config`.
    ///
    /// Order: Sourcify (no key, broad coverage) → Etherscan V2 (if key set)
    /// → Blockscout (if a host is resolvable for the target chain).
    ///
    /// Note the shape: we pass `chain` here because Blockscout needs a
    /// per-chain host. Pre-selecting the Blockscout instance at chain
    /// construction is cleaner than making every explorer accept `chain`
    /// anew in `fetch_source`.
    pub fn standard(chain: &Chain, config: &Config) -> Self {
        let mut explorers: Vec<Arc<dyn SourceExplorer>> = Vec::with_capacity(3);
        explorers.push(Arc::new(Sourcify::default()));

        if let Some(key) = config
            .etherscan_api_key
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            explorers.push(Arc::new(Etherscan::new(key)));
        }

        if let Some(host) = Blockscout::resolve_host(chain, config) {
            explorers.push(Arc::new(Blockscout::new(host)));
        }

        Self::new(explorers)
    }

    /// Disable the cache for this handle (`--no-cache`). Still writes.
    #[must_use]
    pub fn without_cache_reads(self) -> NoReadCacheChain {
        NoReadCacheChain { inner: self }
    }

    /// Run the chain. Never fails — failures per explorer are recorded as
    /// [`ExplorerOutcome`] entries in the returned attempt list.
    pub async fn resolve(&self, chain: &Chain, address: Address) -> ResolutionAttempt {
        self.resolve_inner(chain, address, true).await
    }

    async fn resolve_inner(
        &self,
        chain: &Chain,
        address: Address,
        read_cache: bool,
    ) -> ResolutionAttempt {
        let cache_key = format!("{}:{}", chain.canonical_name(), address);

        if read_cache {
            if let Some(cache) = &self.cache {
                match cache.get::<CachedEntry>(&cache_key).await {
                    Ok(Some(hit)) => match hit.value {
                        CachedEntry::Hit { explorer, source } => {
                            return ResolutionAttempt {
                                result: Some((explorer.clone(), *source)),
                                attempts: vec![ExplorerAttempt {
                                    explorer,
                                    outcome: ExplorerOutcome::Found {
                                        match_quality: MatchQuality::Full,
                                    },
                                    duration: Duration::from_millis(0),
                                }],
                            };
                        }
                        CachedEntry::Miss => {
                            return ResolutionAttempt {
                                result: None,
                                attempts: vec![ExplorerAttempt {
                                    explorer: "cache".into(),
                                    outcome: ExplorerOutcome::NotVerified,
                                    duration: Duration::from_millis(0),
                                }],
                            };
                        }
                    },
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "verified_source cache read failed");
                    }
                }
            }
        }

        let mut attempts = Vec::with_capacity(self.explorers.len());
        let mut winner: Option<(String, VerifiedSource)> = None;
        for explorer in &self.explorers {
            let name = explorer.name().to_string();
            let start = Instant::now();
            let outcome_and_source = explorer.fetch_source(chain, address).await;
            let duration = start.elapsed();
            let outcome = match &outcome_and_source {
                Ok(Some(src)) => match src.metadata.pointer("/sourcify_match") {
                    Some(serde_json::Value::String(s)) if s == "partial" => {
                        ExplorerOutcome::Found {
                            match_quality: MatchQuality::Partial,
                        }
                    }
                    _ => ExplorerOutcome::Found {
                        match_quality: MatchQuality::Full,
                    },
                },
                Ok(None) => ExplorerOutcome::NotVerified,
                Err(e) => classify_error(e),
            };
            attempts.push(ExplorerAttempt {
                explorer: name.clone(),
                outcome: outcome.clone(),
                duration,
            });
            if let Ok(Some(src)) = outcome_and_source {
                winner = Some((name, src));
                break;
            }
        }

        if let Some(cache) = &self.cache {
            let entry = match &winner {
                Some((explorer, source)) => CachedEntry::Hit {
                    explorer: explorer.clone(),
                    source: Box::new(source.clone()),
                },
                None => CachedEntry::Miss,
            };
            let ttl = if winner.is_some() { HIT_TTL } else { MISS_TTL };
            if let Err(e) = cache.put(&cache_key, &entry, ttl).await {
                tracing::warn!(error = %e, "verified_source cache write failed");
            }
        }

        ResolutionAttempt {
            result: winner,
            attempts,
        }
    }
}

/// `ExplorerChain` wrapper that bypasses cache reads (still writes on success).
pub struct NoReadCacheChain {
    inner: ExplorerChain,
}

impl NoReadCacheChain {
    pub async fn resolve(&self, chain: &Chain, address: Address) -> ResolutionAttempt {
        self.inner.resolve_inner(chain, address, false).await
    }
}

fn classify_error(e: &ExplorerError) -> ExplorerOutcome {
    match e {
        ExplorerError::Network(msg) => ExplorerOutcome::NetworkError(msg.clone()),
        ExplorerError::RateLimited => ExplorerOutcome::RateLimited,
        ExplorerError::ChainUnsupported => ExplorerOutcome::ChainUnsupported,
        ExplorerError::NoApiKey => ExplorerOutcome::NoApiKey,
        ExplorerError::MalformedResponse(msg) | ExplorerError::Other(msg) => {
            ExplorerOutcome::Other(msg.clone())
        }
    }
}

/// The on-disk cache shape: hit carries the winning explorer + source,
/// miss is a sentinel for negative caching.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
enum CachedEntry {
    Hit {
        explorer: String,
        // Boxed to keep the enum layout compact — Miss is zero-sized and
        // VerifiedSource is large.
        source: Box<VerifiedSource>,
    },
    Miss,
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use async_trait::async_trait;
    use wiremock::{
        matchers::{method, path_regex},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;
    use crate::sourcify::Sourcify;

    fn addr() -> Address {
        Address::from_str("0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359").unwrap()
    }

    fn metadata_body() -> serde_json::Value {
        serde_json::json!({
            "status": "full",
            "files": [
                {
                    "name": "metadata.json",
                    "path": "metadata.json",
                    "content": serde_json::json!({
                        "compiler": { "version": "0.8.20" },
                        "settings": { "compilationTarget": { "X.sol": "X" } },
                        "output": { "abi": [] }
                    }).to_string()
                },
                { "name": "X.sol", "path": "X.sol", "content": "// X" }
            ]
        })
    }

    /// Sink explorer for composition tests: returns a pre-programmed outcome.
    struct Canned {
        name: &'static str,
        outcome: std::sync::Mutex<Option<Result<Option<VerifiedSource>, ExplorerError>>>,
    }

    #[async_trait]
    impl SourceExplorer for Canned {
        fn name(&self) -> &'static str {
            self.name
        }
        async fn fetch_source(
            &self,
            _chain: &Chain,
            _address: Address,
        ) -> Result<Option<VerifiedSource>, ExplorerError> {
            self.outcome
                .lock()
                .unwrap()
                .take()
                .expect("canned outcome consumed twice")
        }
    }

    fn canned(name: &'static str, r: Result<Option<VerifiedSource>, ExplorerError>) -> Arc<Canned> {
        Arc::new(Canned {
            name,
            outcome: std::sync::Mutex::new(Some(r)),
        })
    }

    fn dummy_source() -> VerifiedSource {
        VerifiedSource {
            source_files: std::collections::BTreeMap::new(),
            contract_name: "X".into(),
            compiler_version: "0.8.20".into(),
            optimizer: None,
            evm_version: None,
            abi: serde_json::Value::Array(vec![]),
            constructor_args: None,
            license: None,
            proxy_hint: None,
            implementation_hint: None,
            metadata: serde_json::Value::Null,
        }
    }

    fn chain_with(explorers: Vec<Arc<dyn SourceExplorer>>) -> ExplorerChain {
        // Tests should not write to the real user cache dir.
        ExplorerChain {
            explorers,
            cache: None,
        }
    }

    #[tokio::test]
    async fn first_hit_short_circuits() {
        let src = dummy_source();
        let c = chain_with(vec![
            canned("a", Ok(Some(src))),
            canned(
                "b",
                Err(ExplorerError::Other("should not be called".into())),
            ),
        ]);
        let r = c.resolve(&Chain::EthereumMainnet, addr()).await;
        assert!(r.result.is_some());
        assert_eq!(r.attempts.len(), 1);
        assert_eq!(r.attempts[0].explorer, "a");
        assert!(matches!(
            r.attempts[0].outcome,
            ExplorerOutcome::Found { .. }
        ));
    }

    #[tokio::test]
    async fn falls_through_on_not_verified() {
        let src = dummy_source();
        let c = chain_with(vec![canned("a", Ok(None)), canned("b", Ok(Some(src)))]);
        let r = c.resolve(&Chain::EthereumMainnet, addr()).await;
        assert!(r.result.is_some());
        assert_eq!(r.attempts.len(), 2);
        assert!(matches!(
            r.attempts[0].outcome,
            ExplorerOutcome::NotVerified
        ));
        assert!(matches!(
            r.attempts[1].outcome,
            ExplorerOutcome::Found { .. }
        ));
    }

    #[tokio::test]
    async fn falls_through_on_error() {
        let c = chain_with(vec![
            canned("a", Err(ExplorerError::Network("conn refused".into()))),
            canned("b", Ok(None)),
        ]);
        let r = c.resolve(&Chain::EthereumMainnet, addr()).await;
        assert!(r.result.is_none());
        assert_eq!(r.attempts.len(), 2);
        assert!(matches!(
            r.attempts[0].outcome,
            ExplorerOutcome::NetworkError(_)
        ));
        assert!(matches!(
            r.attempts[1].outcome,
            ExplorerOutcome::NotVerified
        ));
    }

    #[tokio::test]
    async fn all_fail_returns_none_with_full_trail() {
        let c = chain_with(vec![
            canned("a", Err(ExplorerError::RateLimited)),
            canned("b", Err(ExplorerError::NoApiKey)),
        ]);
        let r = c.resolve(&Chain::EthereumMainnet, addr()).await;
        assert!(r.result.is_none());
        assert_eq!(r.attempts.len(), 2);
        assert!(matches!(
            r.attempts[0].outcome,
            ExplorerOutcome::RateLimited
        ));
        assert!(matches!(r.attempts[1].outcome, ExplorerOutcome::NoApiKey));
    }

    #[tokio::test]
    async fn standard_three_step_chain_falls_through_to_blockscout() {
        // Sourcify → 404, Etherscan → unverified, Blockscout → hit.
        let sourcify_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/files/any/\d+/0x.*$"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&sourcify_server)
            .await;

        let etherscan_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/v2/api$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "1", "message": "OK",
                "result": [{"SourceCode": "", "ABI": "", "ContractName": ""}]
            })))
            .mount(&etherscan_server)
            .await;

        let blockscout_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/api/v2/smart-contracts/0x.*$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "is_verified": true,
                "name": "Token",
                "source_code": "// Token",
                "file_path": "Token.sol",
                "compiler_version": "v0.8.20",
                "optimization_enabled": true,
                "optimization_runs": 200,
                "abi": [],
                "license_type": "mit"
            })))
            .mount(&blockscout_server)
            .await;

        let sf: Arc<dyn SourceExplorer> =
            Arc::new(crate::sourcify::Sourcify::new(sourcify_server.uri()));
        let es: Arc<dyn SourceExplorer> = Arc::new(crate::etherscan::Etherscan::new_with_base(
            etherscan_server.uri(),
            "key",
        ));
        let bs: Arc<dyn SourceExplorer> =
            Arc::new(crate::blockscout::Blockscout::new(blockscout_server.uri()));
        let c = chain_with(vec![sf, es, bs]);
        let r = c.resolve(&Chain::EthereumMainnet, addr()).await;

        assert!(r.result.is_some());
        assert_eq!(r.attempts.len(), 3);
        assert_eq!(r.attempts[0].explorer, "sourcify");
        assert!(matches!(
            r.attempts[0].outcome,
            ExplorerOutcome::NotVerified
        ));
        assert_eq!(r.attempts[1].explorer, "etherscan");
        assert!(matches!(
            r.attempts[1].outcome,
            ExplorerOutcome::NotVerified
        ));
        assert_eq!(r.attempts[2].explorer, "blockscout");
        assert!(matches!(
            r.attempts[2].outcome,
            ExplorerOutcome::Found { .. }
        ));
    }

    #[tokio::test]
    async fn standard_omits_etherscan_without_api_key() {
        // Without an etherscan key the default chain should just be
        // Sourcify + (Blockscout if a default host is known).
        let cfg = Config::default();
        let c = ExplorerChain::standard(&Chain::EthereumMainnet, &cfg);
        let debug = format!("{c:?}");
        assert!(debug.contains("sourcify"));
        assert!(!debug.contains("etherscan"));
    }

    #[tokio::test]
    async fn standard_includes_etherscan_with_api_key() {
        let cfg = Config {
            etherscan_api_key: Some("k".into()),
            ..Config::default()
        };
        let c = ExplorerChain::standard(&Chain::EthereumMainnet, &cfg);
        let debug = format!("{c:?}");
        assert!(debug.contains("sourcify"));
        assert!(debug.contains("etherscan"));
    }

    #[tokio::test]
    async fn standard_drops_blockscout_when_no_host() {
        // Chain::Other has no default Blockscout host and no user config.
        let cfg = Config::default();
        let other = Chain::Other {
            chain_id: 31_337,
            name: "anvil".into(),
        };
        let c = ExplorerChain::standard(&other, &cfg);
        let debug = format!("{c:?}");
        assert!(!debug.contains("blockscout"));
    }

    #[tokio::test]
    async fn wiremock_sourcify_composed_through_chain() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/files/any/\d+/0x.*$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(metadata_body()))
            .mount(&server)
            .await;

        let sf: Arc<dyn SourceExplorer> = Arc::new(Sourcify::new(server.uri()));
        let c = chain_with(vec![sf]);
        let r = c.resolve(&Chain::EthereumMainnet, addr()).await;
        assert!(r.result.is_some(), "result was {r:?}");
        assert_eq!(r.attempts.len(), 1);
        assert_eq!(r.attempts[0].explorer, "sourcify");
    }
}
