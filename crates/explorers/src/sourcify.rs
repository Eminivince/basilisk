//! Sourcify client.
//!
//! Sourcify indexes verified contracts by bytecode hash. No API key required.
//! We query `GET <base>/files/any/<chain_id>/<address>` and parse the "files"
//! array into a [`VerifiedSource`], pulling compiler/optimizer/abi fields
//! out of `metadata.json` when it's present.

use std::time::Duration;

use alloy_primitives::Address;
use async_trait::async_trait;
use basilisk_core::Chain;
use serde::Deserialize;

use crate::{
    error::ExplorerError,
    source_explorer::{sanitize_path, SourceExplorer},
    types::{MatchQuality, OptimizerSettings, VerifiedSource},
};

/// Default Sourcify server base URL.
pub const DEFAULT_BASE: &str = "https://sourcify.dev/server";

/// Sourcify client.
#[derive(Debug, Clone)]
pub struct Sourcify {
    base: String,
    client: reqwest::Client,
}

impl Default for Sourcify {
    fn default() -> Self {
        Self::new(DEFAULT_BASE)
    }
}

impl Sourcify {
    /// Construct a client targeting `base` (without trailing slash).
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
}

#[async_trait]
impl SourceExplorer for Sourcify {
    fn name(&self) -> &'static str {
        "sourcify"
    }

    async fn fetch_source(
        &self,
        chain: &Chain,
        address: Address,
    ) -> Result<Option<VerifiedSource>, ExplorerError> {
        let url = format!("{}/files/any/{}/{}", self.base, chain.chain_id(), address,);
        tracing::debug!(url, "sourcify request");
        let res = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| classify_reqwest_error(&e))?;

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

        let body: SourcifyResponse = res
            .json()
            .await
            .map_err(|e| ExplorerError::MalformedResponse(e.to_string()))?;

        let quality = match body.status.as_deref() {
            Some("full") => MatchQuality::Full,
            Some("partial") => MatchQuality::Partial,
            Some(other) => {
                return Err(ExplorerError::MalformedResponse(format!(
                    "unknown status {other:?}",
                )));
            }
            None => {
                // Some deployments return files without a status; treat as partial.
                MatchQuality::Partial
            }
        };

        let Some(files) = body.files else {
            return Err(ExplorerError::MalformedResponse(
                "missing files array".into(),
            ));
        };
        if files.is_empty() {
            return Ok(None);
        }

        let mut source = parse_files(&files, address)?;
        // Sourcify reports quality via the status field; reflect it in metadata
        // so downstream consumers see full-vs-partial without another field.
        if let serde_json::Value::Object(m) = &mut source.metadata {
            m.insert(
                "sourcify_match".into(),
                serde_json::Value::String(match quality {
                    MatchQuality::Full => "full".into(),
                    MatchQuality::Partial => "partial".into(),
                }),
            );
        }
        if quality == MatchQuality::Partial {
            tracing::warn!(
                address = %address,
                chain = chain.canonical_name(),
                "sourcify returned a PARTIAL match; bytecode matches, metadata may differ",
            );
        }
        Ok(Some(source))
    }
}

fn classify_reqwest_error(e: &reqwest::Error) -> ExplorerError {
    if e.is_timeout() {
        ExplorerError::Network("request timed out".into())
    } else if e.is_connect() {
        ExplorerError::Network(format!("connection failed: {e}"))
    } else {
        ExplorerError::Network(e.to_string())
    }
}

fn parse_files(files: &[SourcifyFile], address: Address) -> Result<VerifiedSource, ExplorerError> {
    let mut source_files = std::collections::BTreeMap::new();
    let mut metadata_json: Option<serde_json::Value> = None;

    for f in files {
        let name = f.name.as_deref().unwrap_or_default();
        let path_raw = f.path.as_deref().unwrap_or(name);
        let content = f.content.clone().unwrap_or_default();
        if name == "metadata.json" {
            metadata_json = serde_json::from_str(&content).ok();
            continue;
        }
        if !name.to_ascii_lowercase().ends_with(".sol") {
            // Skip non-Solidity artifacts (e.g. immutable-references-map.json,
            // constructor-args.txt). We keep them out of source_files but
            // extract relevant fields below.
            continue;
        }
        if let Some(path) = sanitize_path(path_raw) {
            source_files.insert(path, content);
        } else {
            tracing::warn!(path = path_raw, "skipping source file with unsafe path");
        }
    }

    let metadata = metadata_json.unwrap_or(serde_json::Value::Null);
    let (contract_name, compiler_version, optimizer, evm_version, abi, license) =
        extract_metadata(&metadata, &source_files);

    Ok(VerifiedSource {
        source_files,
        contract_name,
        compiler_version,
        optimizer,
        evm_version,
        abi,
        constructor_args: None,
        license,
        proxy_hint: None,
        implementation_hint: Some(address).filter(|_| false), // Sourcify doesn't surface this.
        metadata,
    })
}

type MetadataExtract = (
    String,
    String,
    Option<OptimizerSettings>,
    Option<String>,
    serde_json::Value,
    Option<String>,
);

fn extract_metadata(
    metadata: &serde_json::Value,
    source_files: &std::collections::BTreeMap<std::path::PathBuf, String>,
) -> MetadataExtract {
    let compiler = metadata
        .pointer("/compiler/version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string();

    let evm_version = metadata
        .pointer("/settings/evmVersion")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);

    let optimizer = metadata.pointer("/settings/optimizer").and_then(|o| {
        let enabled = o.get("enabled")?.as_bool()?;
        let runs = o.get("runs")?.as_u64()?;
        Some(OptimizerSettings {
            enabled,
            runs: u32::try_from(runs).ok()?,
        })
    });

    let abi = metadata
        .pointer("/output/abi")
        .cloned()
        .unwrap_or(serde_json::Value::Array(vec![]));

    // Sourcify's metadata.settings.compilationTarget is `{ "path.sol": "ContractName" }`.
    let contract_name = metadata
        .pointer("/settings/compilationTarget")
        .and_then(serde_json::Value::as_object)
        .and_then(|m| m.values().next())
        .and_then(serde_json::Value::as_str)
        .map_or_else(
            || {
                // Fallback: largest-looking .sol filename stem.
                source_files
                    .keys()
                    .max_by_key(|p| source_files.get(*p).map_or(0, String::len))
                    .and_then(|p| p.file_stem().and_then(|s| s.to_str()))
                    .unwrap_or("Unknown")
                    .to_string()
            },
            str::to_string,
        );

    let license = metadata
        .pointer("/sources")
        .and_then(serde_json::Value::as_object)
        .and_then(|sources| {
            sources.values().find_map(|v| {
                v.get("license")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
        });

    (
        contract_name,
        compiler,
        optimizer,
        evm_version,
        abi,
        license,
    )
}

#[derive(Debug, Deserialize)]
struct SourcifyResponse {
    status: Option<String>,
    files: Option<Vec<SourcifyFile>>,
}

#[derive(Debug, Deserialize)]
struct SourcifyFile {
    name: Option<String>,
    path: Option<String>,
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use basilisk_core::Chain;
    use wiremock::{
        matchers::{method, path_regex},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;

    fn addr() -> Address {
        Address::from_str("0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359").unwrap()
    }

    fn metadata_json() -> serde_json::Value {
        serde_json::json!({
            "compiler": { "version": "0.8.20+commit.a1b79de6" },
            "settings": {
                "optimizer": { "enabled": true, "runs": 200 },
                "evmVersion": "paris",
                "compilationTarget": { "contracts/Token.sol": "Token" }
            },
            "sources": { "contracts/Token.sol": { "license": "MIT" } },
            "output": { "abi": [{"type": "function", "name": "name"}] }
        })
    }

    fn full_match_body() -> serde_json::Value {
        serde_json::json!({
            "status": "full",
            "files": [
                {
                    "name": "metadata.json",
                    "path": "metadata.json",
                    "content": metadata_json().to_string()
                },
                {
                    "name": "Token.sol",
                    "path": "contracts/Token.sol",
                    "content": "// SPDX-License-Identifier: MIT\ncontract Token {}"
                }
            ]
        })
    }

    #[tokio::test]
    async fn parses_full_match() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/files/any/\d+/0x.*$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(full_match_body()))
            .mount(&server)
            .await;

        let sf = Sourcify::new(server.uri());
        let got = sf
            .fetch_source(&Chain::EthereumMainnet, addr())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.contract_name, "Token");
        assert_eq!(got.compiler_version, "0.8.20+commit.a1b79de6");
        assert_eq!(
            got.optimizer.unwrap(),
            OptimizerSettings {
                enabled: true,
                runs: 200
            }
        );
        assert_eq!(got.evm_version.as_deref(), Some("paris"));
        assert_eq!(got.license.as_deref(), Some("MIT"));
        assert!(got
            .source_files
            .contains_key(std::path::Path::new("contracts/Token.sol")));
    }

    #[tokio::test]
    async fn parses_partial_match() {
        let server = MockServer::start().await;
        let mut body = full_match_body();
        body["status"] = serde_json::Value::String("partial".into());
        Mock::given(method("GET"))
            .and(path_regex(r"^/files/any/\d+/0x.*$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let sf = Sourcify::new(server.uri());
        let got = sf
            .fetch_source(&Chain::EthereumMainnet, addr())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            got.metadata
                .pointer("/sourcify_match")
                .and_then(|v| v.as_str()),
            Some("partial"),
        );
    }

    #[tokio::test]
    async fn not_found_returns_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let sf = Sourcify::new(server.uri());
        let got = sf
            .fetch_source(&Chain::EthereumMainnet, addr())
            .await
            .unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn rate_limited_maps_to_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let sf = Sourcify::new(server.uri());
        let err = sf
            .fetch_source(&Chain::EthereumMainnet, addr())
            .await
            .unwrap_err();
        assert!(matches!(err, ExplorerError::RateLimited));
    }

    #[tokio::test]
    async fn malformed_response_maps_to_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let sf = Sourcify::new(server.uri());
        let err = sf
            .fetch_source(&Chain::EthereumMainnet, addr())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ExplorerError::MalformedResponse(_)),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn empty_files_array_is_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "full",
                "files": []
            })))
            .mount(&server)
            .await;

        let sf = Sourcify::new(server.uri());
        let got = sf
            .fetch_source(&Chain::EthereumMainnet, addr())
            .await
            .unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn unsafe_paths_are_dropped() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "status": "full",
            "files": [
                { "name": "metadata.json", "path": "metadata.json", "content": metadata_json().to_string() },
                { "name": "Token.sol", "path": "../../etc/passwd", "content": "pwn" },
                { "name": "Good.sol", "path": "src/Good.sol", "content": "good" }
            ]
        });
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let sf = Sourcify::new(server.uri());
        let got = sf
            .fetch_source(&Chain::EthereumMainnet, addr())
            .await
            .unwrap()
            .unwrap();
        assert!(got
            .source_files
            .contains_key(std::path::Path::new("src/Good.sol")));
        assert!(!got
            .source_files
            .keys()
            .any(|p| p.to_string_lossy().contains("passwd")));
    }
}
