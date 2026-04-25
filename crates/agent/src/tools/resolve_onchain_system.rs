//! `resolve_onchain_system` — full contract-graph expansion from a root address.

use std::time::Duration;

use alloy_primitives::Address;
use async_trait::async_trait;
use basilisk_core::Chain;
use basilisk_onchain::{ExpansionLimits, OnchainIngester};
use serde::Deserialize;

use crate::tool::{Tool, ToolContext, ToolResult};

pub struct ResolveOnchainSystem;

#[allow(clippy::struct_excessive_bools)] // Every bool is an independent expansion toggle.
#[derive(Deserialize)]
struct Input {
    address: String,
    chain: String,
    #[serde(default)]
    max_depth: Option<usize>,
    #[serde(default)]
    max_contracts: Option<usize>,
    #[serde(default)]
    max_duration_secs: Option<u64>,
    #[serde(default)]
    expand_storage: Option<bool>,
    #[serde(default)]
    expand_bytecode: Option<bool>,
    #[serde(default)]
    expand_immutables: Option<bool>,
    #[serde(default)]
    fetch_history: Option<bool>,
    #[serde(default)]
    fetch_constructor_args: Option<bool>,
    #[serde(default)]
    fetch_storage_layout: Option<bool>,
    #[serde(default)]
    storage_scan_depth: Option<usize>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[async_trait]
impl Tool for ResolveOnchainSystem {
    fn name(&self) -> &'static str {
        "resolve_onchain_system"
    }

    fn description(&self) -> &'static str {
        "Expand a full contract system from a root address. BFS over proxy impls, diamond \
         facets, and typed references (storage slots, bytecode PUSH20s, verified-source \
         immutables). Returns every reachable contract plus a typed graph. Use when you need \
         the whole picture (proxies + implementations + libraries). For a single contract, \
         use `resolve_onchain_contract` — it's cheaper. \n\n\
         The defaults are conservative (depth 3, 50 contracts, 5 min budget); widen them when \
         the system is known-large (Aave, Compound, a diamond). Narrow them when the budget is \
         tight. A `TruncationReason` in the response's `stats.expansion_truncated` tells you \
         which budget was hit — widen accordingly and retry if you want more coverage."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["address", "chain"],
            "properties": {
                "address": { "type": "string", "description": "Root contract address." },
                "chain": { "type": "string", "description": "Chain name." },
                "max_depth": { "type": "integer", "description": "BFS depth cap. Default 3." },
                "max_contracts": { "type": "integer", "description": "Total contracts cap. Default 50." },
                "max_duration_secs": { "type": "integer", "description": "Wall-clock cap in seconds. Default 300." },
                "expand_storage": { "type": "boolean", "description": "Follow storage-slot PUSH20s. Default true." },
                "expand_bytecode": { "type": "boolean", "description": "Follow bytecode PUSH20s. Default true." },
                "expand_immutables": { "type": "boolean", "description": "Follow verified-source immutables/constants. Default true." },
                "fetch_history": { "type": "boolean", "description": "Walk upgrade-history logs. Default true; degrades on RPC range limits." },
                "fetch_constructor_args": { "type": "boolean", "description": "Recover constructor args. Default true." },
                "fetch_storage_layout": { "type": "boolean", "description": "Recover storage layout from verified source. Default true (stubbed)." },
                "storage_scan_depth": { "type": "integer", "description": "How many storage slots to scan per contract. Default 64." },
                "timeout_secs": { "type": "integer", "description": "Per-contract timeout override." }
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

        let mut limits = ExpansionLimits::default();
        if let Some(v) = input.max_depth {
            limits.max_depth = v;
        }
        if let Some(v) = input.max_contracts {
            limits.max_contracts = v;
        }
        if let Some(v) = input.max_duration_secs {
            limits.max_duration = Duration::from_secs(v);
        }
        if let Some(v) = input.expand_storage {
            limits.expand_storage = v;
        }
        if let Some(v) = input.expand_bytecode {
            limits.expand_bytecode = v;
        }
        if let Some(v) = input.expand_immutables {
            limits.expand_immutables = v;
        }
        if let Some(v) = input.fetch_history {
            limits.fetch_history = v;
        }
        if let Some(v) = input.fetch_constructor_args {
            limits.fetch_constructor_args = v;
        }
        if let Some(v) = input.fetch_storage_layout {
            limits.fetch_storage_layout = v;
        }
        if let Some(v) = input.storage_scan_depth {
            limits.storage_scan_depth = v;
        }

        let mut ingester = match OnchainIngester::new(&chain, &ctx.config) {
            Ok(i) => i,
            Err(e) => return ToolResult::err(format!("initialising ingester: {e}"), false),
        };
        if let Some(secs) = input.timeout_secs {
            ingester = ingester.with_timeout(Duration::from_secs(secs));
        }

        match ingester.resolve_system(address, limits).await {
            Ok(s) => {
                // Cache the resolved system so vuln-reasoning analytical
                // tools (find_callers_of, trace_state_dependencies) can
                // operate on it without re-resolving.
                if let Ok(mut guard) = ctx.resolved_systems.lock() {
                    guard.insert(address, s.clone());
                }
                ToolResult::ok(s)
            }
            Err(e) => {
                let retryable = matches!(
                    &e,
                    basilisk_onchain::IngestError::Rpc(inner) if inner.is_transient()
                );
                ToolResult::err(format!("resolve_system failed: {e}"), retryable)
            }
        }
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
        ResolveOnchainSystem.execute(input, &ctx).await
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
            "address": "0xgarbage",
            "chain": "ethereum",
        }))
        .await;
        match res {
            ToolResult::Err { retryable, .. } => assert!(!retryable),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn input_schema_declares_every_limit() {
        let schema = ResolveOnchainSystem.input_schema();
        let props = &schema["properties"];
        for required in [
            "address",
            "chain",
            "max_depth",
            "max_contracts",
            "max_duration_secs",
            "expand_storage",
            "expand_bytecode",
            "expand_immutables",
            "fetch_history",
            "fetch_constructor_args",
            "fetch_storage_layout",
            "storage_scan_depth",
            "timeout_secs",
        ] {
            assert!(props.get(required).is_some(), "schema missing `{required}`",);
        }
    }
}
