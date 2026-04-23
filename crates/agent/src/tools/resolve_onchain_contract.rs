//! `resolve_onchain_contract` — single-contract resolution (no graph expansion).

use std::time::Duration;

use alloy_primitives::Address;
use async_trait::async_trait;
use basilisk_core::Chain;
use basilisk_onchain::OnchainIngester;
use serde::Deserialize;

use crate::tool::{Tool, ToolContext, ToolResult};

pub struct ResolveOnchainContract;

#[derive(Deserialize)]
struct Input {
    address: String,
    chain: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[async_trait]
impl Tool for ResolveOnchainContract {
    fn name(&self) -> &'static str {
        "resolve_onchain_contract"
    }

    fn description(&self) -> &'static str {
        "Fetch one contract's full profile: bytecode, verified source (via Sourcify → Etherscan \
         → Blockscout), proxy detection (EIP-1967 / EIP-1167 / diamond), and constructor args \
         where recoverable. Returns a `ResolvedContract`. Use when you want ONE contract in \
         depth — NOT when you need the whole system (proxies + implementations + libraries). \
         For that, use `resolve_onchain_system`."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["address", "chain"],
            "properties": {
                "address": { "type": "string", "description": "Contract address (0x-prefixed)." },
                "chain": { "type": "string", "description": "Chain name." },
                "timeout_secs": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 300,
                    "description": "Per-contract timeout override. Default 60s."
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::err(format!("invalid input: {e}"), false),
        };

        let chain: Chain = match input.chain.parse() {
            Ok(c) => c,
            Err(e) => {
                return ToolResult::err(format!("unknown chain {:?}: {e}", input.chain), false)
            }
        };
        let address: Address = match input.address.parse() {
            Ok(a) => a,
            Err(e) => return ToolResult::err(format!("bad address: {e}"), false),
        };

        let mut ingester = match OnchainIngester::new(&chain, &ctx.config) {
            Ok(i) => i,
            Err(e) => return ToolResult::err(format!("initialising ingester: {e}"), false),
        };
        if let Some(secs) = input.timeout_secs {
            ingester = ingester.with_timeout(Duration::from_secs(secs));
        }

        match ingester.resolve(address).await {
            Ok(c) => ToolResult::ok(c),
            Err(e) => {
                let retryable = ingest_is_retryable(&e);
                ToolResult::err(format!("resolve failed: {e}"), retryable)
            }
        }
    }
}

fn ingest_is_retryable(e: &basilisk_onchain::IngestError) -> bool {
    match e {
        basilisk_onchain::IngestError::Rpc(inner) => inner.is_transient(),
        _ => false,
    }
}

#[cfg(test)]
#[allow(clippy::match_wildcard_for_single_variants)]
mod tests {
    use super::*;

    async fn run(input: serde_json::Value) -> ToolResult {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::test(dir.path().to_path_buf());
        std::mem::forget(dir);
        ResolveOnchainContract.execute(input, &ctx).await
    }

    #[tokio::test]
    async fn unknown_chain_is_non_retryable() {
        let res = run(serde_json::json!({
            "address": "0x0000000000000000000000000000000000000000",
            "chain": "nonesuch",
        }))
        .await;
        match res {
            ToolResult::Err { retryable, .. } => assert!(!retryable),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bad_address_is_non_retryable() {
        let res = run(serde_json::json!({
            "address": "0xnot-an-address",
            "chain": "ethereum",
        }))
        .await;
        match res {
            ToolResult::Err { retryable, .. } => assert!(!retryable),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bad_input_shape_is_non_retryable() {
        let res = run(serde_json::json!({ "address": 1 })).await;
        match res {
            ToolResult::Err { retryable, .. } => assert!(!retryable),
            other => panic!("got {other:?}"),
        }
    }
}
