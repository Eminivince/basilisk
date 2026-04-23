//! Etherscan V2 client.
//!
//! V2 exposes a single host (`api.etherscan.io/v2/api`) for every chain in
//! the Etherscan family — the target chain is selected via `chainid`.
//! API key is mandatory.
//!
//! The `SourceCode` field of the response may be:
//!   - a plain Solidity source (single-file contracts),
//!   - a standard-JSON input wrapped in double braces `{{...}}` (common quirk),
//!   - standard-JSON directly.
//!
//! We handle all three.

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use alloy_primitives::{Address, Bytes};
use async_trait::async_trait;
use basilisk_core::Chain;
use serde::Deserialize;

use crate::{
    error::ExplorerError,
    source_explorer::{sanitize_path, SourceExplorer},
    types::{OptimizerSettings, VerifiedSource},
};

/// Default Etherscan V2 API host.
pub const DEFAULT_BASE: &str = "https://api.etherscan.io";

/// Etherscan V2 client.
#[derive(Debug, Clone)]
pub struct Etherscan {
    base: String,
    api_key: String,
    client: reqwest::Client,
}

impl Etherscan {
    /// Construct against the default V2 host.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::new_with_base(DEFAULT_BASE, api_key)
    }

    /// Construct against an explicit host (used by tests / mirrors).
    pub fn new_with_base(base: impl Into<String>, api_key: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(concat!("basilisk/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client build");
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            client,
        }
    }

    fn is_chain_supported(chain: &Chain) -> bool {
        matches!(
            chain,
            Chain::EthereumMainnet
                | Chain::Sepolia
                | Chain::Arbitrum
                | Chain::ArbitrumSepolia
                | Chain::Base
                | Chain::BaseSepolia
                | Chain::Optimism
                | Chain::OptimismSepolia
                | Chain::Polygon
                | Chain::Bnb
                | Chain::Avalanche
        )
    }
}

#[async_trait]
impl SourceExplorer for Etherscan {
    fn name(&self) -> &'static str {
        "etherscan"
    }

    #[allow(clippy::too_many_lines)]
    async fn fetch_source(
        &self,
        chain: &Chain,
        address: Address,
    ) -> Result<Option<VerifiedSource>, ExplorerError> {
        if !Self::is_chain_supported(chain) {
            return Err(ExplorerError::ChainUnsupported);
        }
        if self.api_key.trim().is_empty() {
            return Err(ExplorerError::NoApiKey);
        }

        let url = format!("{}/v2/api", self.base);
        let res = self
            .client
            .get(&url)
            .query(&[
                ("chainid", chain.chain_id().to_string()),
                ("module", "contract".into()),
                ("action", "getsourcecode".into()),
                ("address", address.to_string()),
                ("apikey", self.api_key.clone()),
            ])
            .send()
            .await
            .map_err(|e| ExplorerError::Network(e.to_string()))?;

        let status = res.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ExplorerError::RateLimited);
        }
        if !status.is_success() {
            return Err(ExplorerError::Other(format!("HTTP {status}")));
        }

        // Parse as a raw Value first: Etherscan returns error payloads where
        // `result` is a string (e.g. "Max rate limit reached"), which would
        // fail strict envelope parsing. We surface those as `RateLimited`.
        let raw: serde_json::Value = res
            .json()
            .await
            .map_err(|e| ExplorerError::MalformedResponse(e.to_string()))?;

        let status_str = raw.get("status").and_then(serde_json::Value::as_str);
        let message_str = raw.get("message").and_then(serde_json::Value::as_str);

        if status_str == Some("0") {
            let msg = message_str.unwrap_or("");
            let result_msg = raw
                .get("result")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let combined = format!("{msg} {result_msg}");
            if combined.contains("rate limit") || combined.contains("Max rate") {
                return Err(ExplorerError::RateLimited);
            }
            // Otherwise the result array path below either finds an unverified
            // stub or degrades to Ok(None).
        }

        // Pull the first entry out of the result array, tolerating the case
        // where `result` is a non-array (error scalar).
        let first = match raw.get("result") {
            Some(serde_json::Value::Array(a)) => a.first().cloned(),
            _ => None,
        };
        let Some(first_value) = first else {
            return Ok(None);
        };
        let first: EtherscanResult = serde_json::from_value(first_value)
            .map_err(|e| ExplorerError::MalformedResponse(e.to_string()))?;
        let raw_source = first.source_code.clone().unwrap_or_default();
        if raw_source.trim().is_empty() {
            return Ok(None);
        }

        let source_files = parse_source_code(&raw_source);
        if source_files.is_empty() {
            return Ok(None);
        }

        let optimizer = match (first.optimization_used.as_deref(), first.runs.as_deref()) {
            (Some("1"), Some(runs)) => runs.parse::<u32>().ok().map(|runs| OptimizerSettings {
                enabled: true,
                runs,
            }),
            (Some("0"), _) => Some(OptimizerSettings {
                enabled: false,
                runs: 0,
            }),
            _ => None,
        };

        let abi = first
            .abi
            .as_deref()
            .filter(|s| !s.is_empty() && *s != "Contract source code not verified")
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .unwrap_or(serde_json::Value::Array(vec![]));

        let constructor_args = first
            .constructor_arguments
            .as_deref()
            .filter(|s| !s.is_empty())
            .and_then(hex_decode_maybe_prefixed)
            .map(Bytes::from);

        let proxy_hint = match first.proxy.as_deref() {
            Some("1") => Some(address),
            _ => None,
        };
        let implementation_hint = first
            .implementation
            .as_deref()
            .filter(|s| !s.is_empty() && *s != "0x0000000000000000000000000000000000000000")
            .and_then(|s| s.parse::<Address>().ok());

        // Construct a compact metadata JSON for audit.
        let metadata = serde_json::json!({
            "explorer": "etherscan",
            "message": message_str,
            "raw": serde_json::to_value(&first).unwrap_or(serde_json::Value::Null),
        });

        Ok(Some(VerifiedSource {
            source_files,
            contract_name: first.contract_name.unwrap_or_default().trim().to_string(),
            compiler_version: first.compiler_version.unwrap_or_default(),
            optimizer,
            evm_version: first
                .evm_version
                .filter(|s| !s.is_empty() && s != "Default"),
            abi,
            constructor_args,
            license: first.license_type.filter(|s| !s.is_empty() && s != "None"),
            proxy_hint,
            implementation_hint,
            metadata,
        }))
    }
}

/// Parse Etherscan's `SourceCode` field into a path → content map.
///
/// Recognizes three shapes:
/// - `{{ ... }}` double-braced standard-JSON (the Etherscan quirk).
/// - `{ "sources": {...} }` standard-JSON directly.
/// - Plain Solidity source (single file → `"Contract.sol"` key).
fn parse_source_code(raw: &str) -> BTreeMap<PathBuf, String> {
    let trimmed = raw.trim();
    if let Some(inner) = trimmed
        .strip_prefix("{{")
        .and_then(|s| s.strip_suffix("}}"))
    {
        let candidate = format!("{{{inner}}}");
        if let Some(map) = parse_standard_json(&candidate) {
            return map;
        }
    }
    if trimmed.starts_with('{') {
        if let Some(map) = parse_standard_json(trimmed) {
            return map;
        }
    }
    let mut out = BTreeMap::new();
    if let Some(path) = sanitize_path("Contract.sol") {
        out.insert(path, raw.to_string());
    }
    out
}

fn parse_standard_json(s: &str) -> Option<BTreeMap<PathBuf, String>> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let sources = v.get("sources")?.as_object()?;
    let mut out = BTreeMap::new();
    for (path, entry) in sources {
        let content = entry.get("content").and_then(serde_json::Value::as_str)?;
        if let Some(p) = sanitize_path(path) {
            out.insert(p, content.to_string());
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
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

#[derive(Debug, Deserialize, serde::Serialize, Clone)]
#[serde(rename_all = "PascalCase")]
struct EtherscanResult {
    source_code: Option<String>,
    #[serde(rename = "ABI")]
    abi: Option<String>,
    contract_name: Option<String>,
    compiler_version: Option<String>,
    optimization_used: Option<String>,
    runs: Option<String>,
    constructor_arguments: Option<String>,
    #[serde(rename = "EVMVersion")]
    evm_version: Option<String>,
    license_type: Option<String>,
    proxy: Option<String>,
    implementation: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;

    fn addr() -> Address {
        Address::from_str("0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359").unwrap()
    }

    fn verified_body_single_file() -> serde_json::Value {
        serde_json::json!({
            "status": "1",
            "message": "OK",
            "result": [{
                "SourceCode": "// SPDX-License-Identifier: MIT\ncontract Token {}",
                "ABI": "[{\"type\":\"function\",\"name\":\"name\"}]",
                "ContractName": "Token",
                "CompilerVersion": "v0.8.20+commit.a1b79de6",
                "OptimizationUsed": "1",
                "Runs": "200",
                "ConstructorArguments": "0x1234",
                "EVMVersion": "paris",
                "LicenseType": "MIT",
                "Proxy": "0",
                "Implementation": ""
            }]
        })
    }

    fn verified_body_std_json() -> serde_json::Value {
        let inner = serde_json::json!({
            "language": "Solidity",
            "sources": {
                "contracts/Token.sol": { "content": "// Token" },
                "contracts/Lib.sol": { "content": "// Lib" }
            },
            "settings": {}
        })
        .to_string();
        // Etherscan quirk: double-braced wrapping.
        let wrapped = format!("{{{inner}}}");
        serde_json::json!({
            "status": "1",
            "message": "OK",
            "result": [{
                "SourceCode": wrapped,
                "ABI": "[]",
                "ContractName": "Token",
                "CompilerVersion": "v0.8.20",
                "OptimizationUsed": "1",
                "Runs": "200",
                "ConstructorArguments": "",
                "EVMVersion": "paris",
                "LicenseType": "MIT",
                "Proxy": "1",
                "Implementation": "0x1111111111111111111111111111111111111111"
            }]
        })
    }

    fn unverified_body() -> serde_json::Value {
        serde_json::json!({
            "status": "1",
            "message": "OK",
            "result": [{
                "SourceCode": "",
                "ABI": "Contract source code not verified",
                "ContractName": "",
                "CompilerVersion": "",
                "OptimizationUsed": "",
                "Runs": "",
                "ConstructorArguments": "",
                "EVMVersion": "",
                "LicenseType": "",
                "Proxy": "0",
                "Implementation": ""
            }]
        })
    }

    #[tokio::test]
    async fn parses_single_file_response() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/api"))
            .respond_with(ResponseTemplate::new(200).set_body_json(verified_body_single_file()))
            .mount(&server)
            .await;
        let es = Etherscan::new_with_base(server.uri(), "testkey");
        let src = es
            .fetch_source(&Chain::EthereumMainnet, addr())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(src.contract_name, "Token");
        assert_eq!(src.compiler_version, "v0.8.20+commit.a1b79de6");
        assert_eq!(
            src.optimizer.unwrap(),
            OptimizerSettings {
                enabled: true,
                runs: 200
            }
        );
        assert_eq!(src.evm_version.as_deref(), Some("paris"));
        assert_eq!(src.license.as_deref(), Some("MIT"));
        assert!(src
            .source_files
            .contains_key(std::path::Path::new("Contract.sol")));
        assert_eq!(
            src.constructor_args.as_ref().map(|b| b.to_vec()),
            Some(vec![0x12, 0x34])
        );
    }

    #[tokio::test]
    async fn parses_standard_json_double_braced() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/api"))
            .respond_with(ResponseTemplate::new(200).set_body_json(verified_body_std_json()))
            .mount(&server)
            .await;
        let es = Etherscan::new_with_base(server.uri(), "testkey");
        let src = es
            .fetch_source(&Chain::EthereumMainnet, addr())
            .await
            .unwrap()
            .unwrap();
        assert!(src
            .source_files
            .contains_key(std::path::Path::new("contracts/Token.sol")));
        assert!(src
            .source_files
            .contains_key(std::path::Path::new("contracts/Lib.sol")));
        assert!(src.proxy_hint.is_some());
        assert!(src.implementation_hint.is_some());
    }

    #[tokio::test]
    async fn unverified_returns_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/api"))
            .respond_with(ResponseTemplate::new(200).set_body_json(unverified_body()))
            .mount(&server)
            .await;
        let es = Etherscan::new_with_base(server.uri(), "testkey");
        let got = es
            .fetch_source(&Chain::EthereumMainnet, addr())
            .await
            .unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn missing_api_key_errors_fast() {
        // No server needed — short-circuits before calling.
        let es = Etherscan::new_with_base("http://127.0.0.1:1", "   ");
        let err = es
            .fetch_source(&Chain::EthereumMainnet, addr())
            .await
            .unwrap_err();
        assert!(matches!(err, ExplorerError::NoApiKey));
    }

    #[tokio::test]
    async fn unsupported_chain_short_circuits() {
        let other = Chain::Other {
            chain_id: 31337,
            name: "anvil".into(),
        };
        let es = Etherscan::new_with_base("http://127.0.0.1:1", "k");
        let err = es.fetch_source(&other, addr()).await.unwrap_err();
        assert!(matches!(err, ExplorerError::ChainUnsupported));
    }

    #[tokio::test]
    async fn rate_limited_maps_to_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let es = Etherscan::new_with_base(server.uri(), "k");
        let err = es
            .fetch_source(&Chain::EthereumMainnet, addr())
            .await
            .unwrap_err();
        assert!(matches!(err, ExplorerError::RateLimited));
    }

    #[tokio::test]
    async fn body_rate_limit_message_maps_to_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/api"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "0",
                "message": "Max rate limit reached",
                "result": "Max rate limit reached"
            })))
            .mount(&server)
            .await;
        let es = Etherscan::new_with_base(server.uri(), "k");
        let err = es
            .fetch_source(&Chain::EthereumMainnet, addr())
            .await
            .unwrap_err();
        assert!(matches!(err, ExplorerError::RateLimited));
    }

    #[test]
    fn parse_source_plain_falls_through_to_contract_sol() {
        let out = parse_source_code("contract X {}");
        assert_eq!(out.len(), 1);
        assert!(out.contains_key(std::path::Path::new("Contract.sol")));
    }

    #[test]
    fn parse_source_double_braced_standard_json() {
        let raw =
            r#"{{ "sources": { "A.sol": { "content": "a" }, "B.sol": { "content": "b" } } }}"#;
        let out = parse_source_code(raw);
        assert_eq!(out.len(), 2);
        assert_eq!(
            out.get(std::path::Path::new("A.sol")).map(String::as_str),
            Some("a")
        );
    }

    #[test]
    fn parse_source_single_braced_standard_json() {
        let raw = r#"{ "sources": { "A.sol": { "content": "a" } } }"#;
        let out = parse_source_code(raw);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn parse_source_json_with_unsafe_path_dropped() {
        let raw = r#"{"sources": {"../evil.sol": {"content":"x"}, "good.sol": {"content":"y"}}}"#;
        let out = parse_source_code(raw);
        assert!(out.contains_key(std::path::Path::new("good.sol")));
        assert!(!out.keys().any(|p| p.to_string_lossy().contains("evil")));
    }
}
