//! Blockscout client.
//!
//! Blockscout is deployed per-chain (no unified API host). This client
//! accepts a base URL at construction and calls
//! `GET <base>/api/v2/smart-contracts/<address>`.
//!
//! A default per-chain host table is used when the user hasn't configured
//! `BLOCKSCOUT_URL_<CHAIN>` / `config.blockscout_urls`. Chains without a
//! default and without config surface [`ExplorerError::ChainUnsupported`].

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use alloy_primitives::{Address, Bytes};
use async_trait::async_trait;
use basilisk_core::{Chain, Config};
use serde::Deserialize;

use crate::{
    error::ExplorerError,
    source_explorer::{sanitize_path, SourceExplorer},
    types::{OptimizerSettings, VerifiedSource},
};

/// Blockscout client scoped to a single base URL.
#[derive(Debug, Clone)]
pub struct Blockscout {
    base: String,
    client: reqwest::Client,
}

impl Blockscout {
    /// Construct against a single explicit host.
    pub fn new(base: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(concat!("basilisk/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client build");
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            client,
        }
    }

    /// Return the built-in Blockscout host for `chain`, if one exists.
    pub fn default_host(chain: &Chain) -> Option<&'static str> {
        Some(match chain {
            Chain::EthereumMainnet => "https://eth.blockscout.com",
            Chain::Sepolia => "https://eth-sepolia.blockscout.com",
            Chain::Polygon => "https://polygon.blockscout.com",
            Chain::Optimism => "https://optimism.blockscout.com",
            Chain::OptimismSepolia => "https://optimism-sepolia.blockscout.com",
            Chain::Base => "https://base.blockscout.com",
            Chain::BaseSepolia => "https://base-sepolia.blockscout.com",
            Chain::Arbitrum => "https://arbitrum.blockscout.com",
            // No reliable public Blockscout instance for these; user must configure.
            Chain::ArbitrumSepolia | Chain::Bnb | Chain::Avalanche | Chain::Other { .. } => {
                return None
            }
        })
    }

    /// Resolve the host for `chain` from user config with a default fallback.
    pub fn resolve_host(chain: &Chain, config: &Config) -> Option<String> {
        if let Some(custom) = config.blockscout_urls.get(chain.canonical_name()) {
            return Some(custom.clone());
        }
        Self::default_host(chain).map(str::to_string)
    }
}

#[async_trait]
impl SourceExplorer for Blockscout {
    fn name(&self) -> &'static str {
        "blockscout"
    }

    #[allow(clippy::too_many_lines)]
    async fn fetch_source(
        &self,
        _chain: &Chain,
        address: Address,
    ) -> Result<Option<VerifiedSource>, ExplorerError> {
        let url = format!("{}/api/v2/smart-contracts/{}", self.base, address);
        let res = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| ExplorerError::Network(e.to_string()))?;

        let status = res.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ExplorerError::RateLimited);
        }
        if !status.is_success() {
            return Err(ExplorerError::Other(format!("HTTP {status}")));
        }

        let body: BlockscoutResponse = res
            .json()
            .await
            .map_err(|e| ExplorerError::MalformedResponse(e.to_string()))?;

        if !body.is_verified.unwrap_or(false) {
            return Ok(None);
        }

        let mut source_files: BTreeMap<PathBuf, String> = BTreeMap::new();
        // Multi-file: `additional_sources` + primary via `file_path` / `source_code`.
        if let Some(primary_path) = body.file_path.as_deref() {
            if let Some(code) = body.source_code.as_deref() {
                if let Some(p) = sanitize_path(primary_path) {
                    source_files.insert(p, code.to_string());
                }
            }
        }
        if let Some(list) = body.additional_sources.as_ref() {
            for src in list {
                let Some(path) = src.file_path.as_deref() else {
                    continue;
                };
                let Some(code) = src.source_code.as_deref() else {
                    continue;
                };
                if let Some(p) = sanitize_path(path) {
                    source_files.insert(p, code.to_string());
                }
            }
        }
        // Single-file contracts with no `file_path` — synthesize one.
        if source_files.is_empty() {
            if let Some(code) = body.source_code.as_deref() {
                if !code.trim().is_empty() {
                    if let Some(p) = sanitize_path("Contract.sol") {
                        source_files.insert(p, code.to_string());
                    }
                }
            }
        }
        if source_files.is_empty() {
            return Ok(None);
        }

        let optimizer = match (body.optimization_enabled, body.optimization_runs) {
            (Some(true), Some(runs)) => u32::try_from(runs).ok().map(|runs| OptimizerSettings {
                enabled: true,
                runs,
            }),
            (Some(false), _) => Some(OptimizerSettings {
                enabled: false,
                runs: 0,
            }),
            _ => None,
        };

        let abi = body.abi.clone().unwrap_or(serde_json::Value::Array(vec![]));

        let constructor_args = body
            .constructor_args
            .as_deref()
            .filter(|s| !s.is_empty())
            .and_then(hex_decode_maybe_prefixed)
            .map(Bytes::from);

        let implementation_hint = body
            .implementation_address
            .as_deref()
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse::<Address>().ok());

        let proxy_hint = if body.proxy_type.is_some() || implementation_hint.is_some() {
            Some(address)
        } else {
            None
        };

        let metadata = serde_json::json!({
            "explorer": "blockscout",
            "raw": serde_json::to_value(&body).unwrap_or(serde_json::Value::Null),
        });

        Ok(Some(VerifiedSource {
            source_files,
            contract_name: body.name.unwrap_or_default(),
            compiler_version: body.compiler_version.unwrap_or_default(),
            optimizer,
            evm_version: body.evm_version.filter(|s| !s.is_empty() && s != "default"),
            abi,
            constructor_args,
            license: body.license_type.filter(|s| !s.is_empty() && s != "none"),
            proxy_hint,
            implementation_hint,
            metadata,
        }))
    }
}

fn hex_decode_maybe_prefixed(s: &str) -> Option<Vec<u8>> {
    let body = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    if body.is_empty() {
        return Some(Vec::new());
    }
    if !body.len().is_multiple_of(2) {
        return None;
    }
    let mut out = vec![0u8; body.len() / 2];
    for (i, chunk) in body.as_bytes().chunks(2).enumerate() {
        let hi = hex_digit(chunk[0])?;
        let lo = hex_digit(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct BlockscoutResponse {
    is_verified: Option<bool>,
    name: Option<String>,
    source_code: Option<String>,
    file_path: Option<String>,
    compiler_version: Option<String>,
    optimization_enabled: Option<bool>,
    optimization_runs: Option<i64>,
    evm_version: Option<String>,
    abi: Option<serde_json::Value>,
    constructor_args: Option<String>,
    license_type: Option<String>,
    proxy_type: Option<String>,
    implementation_address: Option<String>,
    additional_sources: Option<Vec<AdditionalSource>>,
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct AdditionalSource {
    file_path: Option<String>,
    source_code: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use wiremock::{
        matchers::{method, path_regex},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;

    fn addr() -> Address {
        Address::from_str("0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359").unwrap()
    }

    fn verified_body() -> serde_json::Value {
        serde_json::json!({
            "is_verified": true,
            "name": "Token",
            "source_code": "// Token",
            "file_path": "contracts/Token.sol",
            "compiler_version": "v0.8.20+commit.a1b79de6",
            "optimization_enabled": true,
            "optimization_runs": 200,
            "evm_version": "paris",
            "abi": [{"type":"function","name":"name"}],
            "constructor_args": "0xdead",
            "license_type": "mit",
            "proxy_type": null,
            "implementation_address": null,
            "additional_sources": [
                { "file_path": "contracts/Lib.sol", "source_code": "// Lib" }
            ]
        })
    }

    #[tokio::test]
    async fn parses_verified_multi_file() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/api/v2/smart-contracts/0x.*$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(verified_body()))
            .mount(&server)
            .await;
        let bs = Blockscout::new(server.uri());
        let got = bs
            .fetch_source(&Chain::EthereumMainnet, addr())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.contract_name, "Token");
        assert_eq!(got.compiler_version, "v0.8.20+commit.a1b79de6");
        assert_eq!(
            got.optimizer.unwrap(),
            OptimizerSettings {
                enabled: true,
                runs: 200
            }
        );
        assert_eq!(got.evm_version.as_deref(), Some("paris"));
        assert_eq!(got.license.as_deref(), Some("mit"));
        assert!(got
            .source_files
            .contains_key(std::path::Path::new("contracts/Token.sol")));
        assert!(got
            .source_files
            .contains_key(std::path::Path::new("contracts/Lib.sol")));
        assert_eq!(
            got.constructor_args.as_ref().map(|b| b.to_vec()),
            Some(vec![0xde, 0xad])
        );
    }

    #[tokio::test]
    async fn unverified_returns_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "is_verified": false
            })))
            .mount(&server)
            .await;
        let bs = Blockscout::new(server.uri());
        let got = bs
            .fetch_source(&Chain::EthereumMainnet, addr())
            .await
            .unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn not_found_returns_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let bs = Blockscout::new(server.uri());
        let got = bs
            .fetch_source(&Chain::EthereumMainnet, addr())
            .await
            .unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn detects_proxy_via_implementation_address() {
        let server = MockServer::start().await;
        let mut body = verified_body();
        body["proxy_type"] = serde_json::Value::String("eip1967".into());
        body["implementation_address"] =
            serde_json::Value::String("0x1111111111111111111111111111111111111111".into());
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let bs = Blockscout::new(server.uri());
        let got = bs
            .fetch_source(&Chain::EthereumMainnet, addr())
            .await
            .unwrap()
            .unwrap();
        assert!(got.proxy_hint.is_some());
        assert!(got.implementation_hint.is_some());
    }

    #[tokio::test]
    async fn rate_limited_maps_to_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let bs = Blockscout::new(server.uri());
        let err = bs
            .fetch_source(&Chain::EthereumMainnet, addr())
            .await
            .unwrap_err();
        assert!(matches!(err, ExplorerError::RateLimited));
    }

    #[test]
    fn default_host_covers_common_chains() {
        assert!(Blockscout::default_host(&Chain::EthereumMainnet).is_some());
        assert!(Blockscout::default_host(&Chain::Base).is_some());
        assert!(Blockscout::default_host(&Chain::Bnb).is_none());
    }

    #[test]
    fn resolve_host_prefers_user_config() {
        let mut blockscout_urls = std::collections::HashMap::new();
        blockscout_urls.insert("ethereum".into(), "https://my.blockscout.test".into());
        let cfg = Config {
            blockscout_urls,
            ..Config::default()
        };
        assert_eq!(
            Blockscout::resolve_host(&Chain::EthereumMainnet, &cfg).as_deref(),
            Some("https://my.blockscout.test"),
        );
    }

    #[test]
    fn resolve_host_falls_back_to_default() {
        let cfg = Config::default();
        assert_eq!(
            Blockscout::resolve_host(&Chain::EthereumMainnet, &cfg).as_deref(),
            Some("https://eth.blockscout.com"),
        );
    }
}
