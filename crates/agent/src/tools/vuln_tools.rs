//! Tool wrappers for the Set 9 vulnerability-reasoning toolchain.
//!
//! Four tools:
//!
//!   - [`FindCallersOfTool`] wraps `basilisk_analyze::find_callers_of`.
//!   - [`TraceStateDependenciesTool`] wraps
//!     `basilisk_analyze::trace_state_dependencies`.
//!   - [`SimulateCallChainTool`] wraps
//!     `basilisk_analyze::simulate_call_chain`.
//!   - [`BuildAndRunFoundryTestTool`] scaffolds a minimal Foundry
//!     project from the agent's Solidity source and shells out to
//!     `forge test --json --fork-url`.
//!
//! The first two depend on a previously-resolved system stored in
//! `ToolContext.resolved_systems` — the agent must call
//! `resolve_onchain_system` with the relevant root before these
//! tools can run. The latter two depend on `ToolContext.exec`.

use alloy_primitives::{Address, Bytes};
use async_trait::async_trait;
use basilisk_analyze::{
    find_callers_of, simulate_call_chain, trace_state_dependencies, CallStep, CallerSearch,
    SimulationInput,
};
use basilisk_exec::{ForkChain, ForgeProject};
use serde::Deserialize;
use tempfile::tempdir;

use crate::tool::{Tool, ToolContext, ToolResult};

pub const FIND_CALLERS_OF_NAME: &str = "find_callers_of";
pub const TRACE_STATE_DEPENDENCIES_NAME: &str = "trace_state_dependencies";
pub const SIMULATE_CALL_CHAIN_NAME: &str = "simulate_call_chain";
pub const BUILD_AND_RUN_FOUNDRY_TEST_NAME: &str = "build_and_run_foundry_test";

// ---------- find_callers_of ------------------------------------------

pub struct FindCallersOfTool;

#[derive(Deserialize)]
struct FindCallersInput {
    root_address: String,
    target_address: String,
    /// 4-byte function selector as `"0x12345678"`.
    selector: String,
    #[serde(default)]
    proximity_bytes: Option<usize>,
}

#[async_trait]
impl Tool for FindCallersOfTool {
    fn name(&self) -> &'static str {
        FIND_CALLERS_OF_NAME
    }

    fn description(&self) -> &'static str {
        "Find every caller of `(target_address, selector)` within a previously-resolved system. \
         You must call `resolve_onchain_system` with the relevant `root_address` first. Returns \
         two-tier hits: `exact_from_source` (verified-source text match) and \
         `pattern_match_in_bytecode` (PUSH4 selector near CALL-family opcode). Use to understand \
         the blast radius of a function — who can reach it, with what call kind (CALL / \
         DELEGATECALL / STATICCALL)?"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["root_address", "target_address", "selector"],
            "properties": {
                "root_address": {"type": "string", "description": "Root address of the previously-resolved system."},
                "target_address": {"type": "string", "description": "Address whose callers you want to find."},
                "selector": {"type": "string", "description": "4-byte function selector, e.g. '0xa9059cbb' for transfer(address,uint256)."},
                "proximity_bytes": {"type": "integer", "description": "Bytecode proximity window between PUSH4 and CALL (default 64)."}
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let input: FindCallersInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return ToolResult::err(format!("invalid input: {e}"), false),
        };
        let root: Address = match input.root_address.parse() {
            Ok(a) => a,
            Err(e) => return ToolResult::err(format!("bad root_address: {e}"), false),
        };
        let target: Address = match input.target_address.parse() {
            Ok(a) => a,
            Err(e) => return ToolResult::err(format!("bad target_address: {e}"), false),
        };
        let sel_bytes = match parse_selector(&input.selector) {
            Ok(s) => s,
            Err(m) => return ToolResult::err(m, false),
        };

        let Ok(systems) = ctx.resolved_systems.lock() else {
            return ToolResult::err("resolved_systems lock poisoned", false);
        };
        let Some(system) = systems.get(&root) else {
            return ToolResult::err(
                format!(
                    "system not resolved for root {root}; call resolve_onchain_system first"
                ),
                false,
            );
        };
        let mut search = CallerSearch::new(target, sel_bytes);
        if let Some(p) = input.proximity_bytes {
            search.proximity_bytes = p;
        }
        match find_callers_of(system, &search) {
            Ok(r) => ToolResult::ok(r),
            Err(e) => ToolResult::err(format!("find_callers_of: {e}"), false),
        }
    }
}

// ---------- trace_state_dependencies ---------------------------------

pub struct TraceStateDependenciesTool;

#[derive(Deserialize)]
struct TraceInput {
    root_address: String,
    contract_address: String,
    selector: String,
}

#[async_trait]
impl Tool for TraceStateDependenciesTool {
    fn name(&self) -> &'static str {
        TRACE_STATE_DEPENDENCIES_NAME
    }

    fn description(&self) -> &'static str {
        "Identify storage slots read, slots written, and external calls made by a specific \
         function. Returns a bytecode-static view (whole contract) plus a source-narrowed view \
         (just that function's body) when verified source and ABI are available. The `precision` \
         field tells you which view you got. Critical for reentrancy reasoning: pairs storage \
         effects with external calls so you can see the state-mutation-before-call pattern."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["root_address", "contract_address", "selector"],
            "properties": {
                "root_address": {"type": "string", "description": "Root of the resolved system."},
                "contract_address": {"type": "string", "description": "Contract whose function you want to inspect."},
                "selector": {"type": "string", "description": "4-byte function selector."}
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let input: TraceInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return ToolResult::err(format!("invalid input: {e}"), false),
        };
        let root: Address = match input.root_address.parse() {
            Ok(a) => a,
            Err(e) => return ToolResult::err(format!("bad root_address: {e}"), false),
        };
        let contract: Address = match input.contract_address.parse() {
            Ok(a) => a,
            Err(e) => return ToolResult::err(format!("bad contract_address: {e}"), false),
        };
        let sel = match parse_selector(&input.selector) {
            Ok(s) => s,
            Err(m) => return ToolResult::err(m, false),
        };

        let Ok(systems) = ctx.resolved_systems.lock() else {
            return ToolResult::err("resolved_systems lock poisoned", false);
        };
        let Some(system) = systems.get(&root) else {
            return ToolResult::err(
                "system not resolved; call resolve_onchain_system first",
                false,
            );
        };
        match trace_state_dependencies(system, contract, sel) {
            Ok(r) => ToolResult::ok(r),
            Err(e) => ToolResult::err(format!("trace_state_dependencies: {e}"), false),
        }
    }
}

// ---------- simulate_call_chain --------------------------------------

pub struct SimulateCallChainTool;

#[derive(Deserialize)]
struct SimulateInput {
    chain: String,
    fork_block: u64,
    steps: Vec<SerdeCallStep>,
    #[serde(default)]
    watch_storage: Vec<StorageWatch>,
    #[serde(default)]
    watch_balances: Vec<String>,
}

#[derive(Deserialize)]
struct SerdeCallStep {
    from: String,
    to: String,
    #[serde(default)]
    calldata: Option<String>, // hex
    #[serde(default)]
    value: Option<String>, // hex
    #[serde(default)]
    as_call: bool,
}

#[derive(Deserialize)]
struct StorageWatch {
    address: String,
    slot: String, // hex
}

#[async_trait]
impl Tool for SimulateCallChainTool {
    fn name(&self) -> &'static str {
        SIMULATE_CALL_CHAIN_NAME
    }

    fn description(&self) -> &'static str {
        "Run an ordered sequence of calls against a forked mainnet state and observe the \
         outcomes. Cheaper than a full Foundry PoC — use this when you want to spot-check a \
         hypothesis before committing to a test. Each step is `(from, to, calldata, value, \
         as_call)` with `as_call=true` for read-only and `as_call=false` for state-modifying \
         (auto-impersonated). Specify `watch_storage` / `watch_balances` for addresses you want \
         to read back after the chain completes."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["chain", "fork_block", "steps"],
            "properties": {
                "chain": {"type": "string", "enum": ["ethereum", "optimism", "arbitrum", "polygon", "base", "bnb"]},
                "fork_block": {"type": "integer", "description": "Block to fork from."},
                "steps": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["from", "to"],
                        "properties": {
                            "from": {"type": "string"},
                            "to": {"type": "string"},
                            "calldata": {"type": "string", "description": "Hex calldata (e.g. 0xa9059cbb...)."},
                            "value": {"type": "string", "description": "Hex wei value."},
                            "as_call": {"type": "boolean", "description": "true=eth_call; false=eth_sendTransaction. Default false."}
                        }
                    }
                },
                "watch_storage": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "address": {"type": "string"},
                            "slot": {"type": "string"}
                        }
                    }
                },
                "watch_balances": {"type": "array", "items": {"type": "string"}}
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let Some(exec) = ctx.exec.clone() else {
            return ToolResult::err(
                "execution backend not configured for this session — simulate_call_chain unavailable",
                false,
            );
        };
        let input: SimulateInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return ToolResult::err(format!("invalid input: {e}"), false),
        };
        let chain = match parse_fork_chain(&input.chain) {
            Ok(c) => c,
            Err(m) => return ToolResult::err(m, false),
        };
        let upstream = match basilisk_exec::resolve_rpc_url(&ctx.config, chain) {
            Ok(u) => Some(u),
            Err(e) => return ToolResult::err(format!("{e}"), false),
        };
        let mut steps = Vec::with_capacity(input.steps.len());
        for s in input.steps {
            let from: Address = match s.from.parse() {
                Ok(a) => a,
                Err(e) => return ToolResult::err(format!("bad from: {e}"), false),
            };
            let to: Address = match s.to.parse() {
                Ok(a) => a,
                Err(e) => return ToolResult::err(format!("bad to: {e}"), false),
            };
            let calldata = match s.calldata.as_deref() {
                Some(hex) => match parse_hex_bytes(hex) {
                    Ok(b) => b,
                    Err(m) => return ToolResult::err(m, false),
                },
                None => Bytes::new(),
            };
            let value = match s.value.as_deref() {
                Some(v) => match parse_hex_u256(v) {
                    Ok(x) => Some(x),
                    Err(m) => return ToolResult::err(m, false),
                },
                None => None,
            };
            steps.push(CallStep {
                from,
                to,
                calldata,
                value,
                as_call: s.as_call,
            });
        }
        let mut watch_storage = Vec::with_capacity(input.watch_storage.len());
        for w in input.watch_storage {
            let addr: Address = match w.address.parse() {
                Ok(a) => a,
                Err(e) => return ToolResult::err(format!("bad watch address: {e}"), false),
            };
            let slot = match parse_hex_b256(&w.slot) {
                Ok(b) => b,
                Err(m) => return ToolResult::err(m, false),
            };
            watch_storage.push((addr, slot));
        }
        let mut watch_balances = Vec::with_capacity(input.watch_balances.len());
        for s in input.watch_balances {
            match s.parse() {
                Ok(a) => watch_balances.push(a),
                Err(e) => return ToolResult::err(format!("bad watch balance: {e}"), false),
            }
        }

        let sim_input = SimulationInput {
            chain,
            fork_block: input.fork_block,
            upstream_rpc_url: upstream,
            steps,
            watch_storage,
            watch_balances,
        };
        match simulate_call_chain(exec, sim_input).await {
            Ok(r) => ToolResult::ok(r),
            Err(e) => {
                let retryable = matches!(
                    &e,
                    basilisk_analyze::AnalyzeError::Exec(inner) if inner.is_retryable()
                );
                ToolResult::err(format!("simulate_call_chain: {e}"), retryable)
            }
        }
    }
}

// ---------- build_and_run_foundry_test --------------------------------

pub struct BuildAndRunFoundryTestTool;

#[derive(Deserialize)]
struct ForgeTestInput {
    chain: String,
    fork_block: u64,
    /// Full Solidity source of the test file. Should `pragma` and
    /// import `forge-std/Test.sol` — if it doesn't, forge will fail
    /// compilation and we'll surface that.
    test_source: String,
    #[serde(default)]
    test_file_name: Option<String>,
    #[serde(default)]
    solc_version: Option<String>,
    #[serde(default)]
    match_test: Option<String>,
    /// Extra remappings lines (e.g. `forge-std/=lib/forge-std/src/`).
    #[serde(default)]
    remappings: Vec<String>,
}

#[async_trait]
impl Tool for BuildAndRunFoundryTestTool {
    fn name(&self) -> &'static str {
        BUILD_AND_RUN_FOUNDRY_TEST_NAME
    }

    fn description(&self) -> &'static str {
        "Compile and run a Foundry test against a forked mainnet block. Supply the full \
         Solidity test source (with pragma, imports of forge-std/Test.sol, your test contract). \
         The runner scaffolds a minimal Foundry project, points forge at the upstream RPC at \
         the given fork_block, and returns pass/fail + forge traces. Use to *prove* a finding. \
         If forge fails to compile, the `setup_failed` field carries the diagnostic — fix and \
         retry. If tests fail, `failed[].reason` explains why (assertion failed, revert reason, \
         etc.)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["chain", "fork_block", "test_source"],
            "properties": {
                "chain": {"type": "string", "enum": ["ethereum", "optimism", "arbitrum", "polygon", "base", "bnb"]},
                "fork_block": {"type": "integer"},
                "test_source": {"type": "string", "description": "Full Solidity source with pragma, imports, and your test contract."},
                "test_file_name": {"type": "string", "description": "Defaults to 'PoC.t.sol'."},
                "solc_version": {"type": "string", "description": "e.g. '0.8.20'. When omitted, forge auto-detects."},
                "match_test": {"type": "string", "description": "Optional forge --match-test regex."},
                "remappings": {"type": "array", "items": {"type": "string"}}
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let input: ForgeTestInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return ToolResult::err(format!("invalid input: {e}"), false),
        };
        let chain = match parse_fork_chain(&input.chain) {
            Ok(c) => c,
            Err(m) => return ToolResult::err(m, false),
        };
        let upstream = match basilisk_exec::resolve_rpc_url(&ctx.config, chain) {
            Ok(u) => u,
            Err(e) => return ToolResult::err(format!("{e}"), false),
        };
        // Scaffold into a tempdir; forge build+test runs there.
        let dir = match tempdir() {
            Ok(d) => d,
            Err(e) => return ToolResult::err(format!("tempdir: {e}"), false),
        };
        let file_name = input
            .test_file_name
            .unwrap_or_else(|| "PoC.t.sol".to_string());
        if let Err(e) = basilisk_exec::scaffold_minimal_project(
            dir.path(),
            &input.test_source,
            &file_name,
            input.solc_version.as_deref(),
            &input.remappings,
        )
        .await
        {
            return ToolResult::err(format!("scaffold: {e}"), false);
        }
        let project = ForgeProject {
            root: dir.path().to_path_buf(),
            solc_version: input.solc_version,
            remappings: input.remappings,
            fork_url: upstream,
            fork_block: input.fork_block,
            match_test: input.match_test,
        };
        match basilisk_exec::run_forge_test(&project, None).await {
            Ok(r) => {
                // Keep the scaffold around briefly to aid debugging
                // via paths in stderr; the OS reaps it on process
                // exit (tempdir::TempDir auto-cleans on drop, but
                // we explicitly `forget` on a few-MB project to let
                // the agent's next tool inspect if needed).
                std::mem::forget(dir);
                ToolResult::ok(r)
            }
            Err(e) => ToolResult::err(format!("run_forge_test: {e}"), e.is_retryable()),
        }
    }
}

// ---------- helpers --------------------------------------------------

fn parse_selector(s: &str) -> Result<[u8; 4], String> {
    let clean = s.trim().trim_start_matches("0x");
    let bytes = hex::decode(clean).map_err(|e| format!("selector hex decode: {e}"))?;
    if bytes.len() != 4 {
        return Err(format!(
            "selector must be 4 bytes; got {} bytes",
            bytes.len()
        ));
    }
    let mut out = [0u8; 4];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn parse_hex_bytes(s: &str) -> Result<Bytes, String> {
    let clean = s.trim().trim_start_matches("0x");
    hex::decode(clean)
        .map(Bytes::from)
        .map_err(|e| format!("hex decode: {e}"))
}

fn parse_hex_b256(s: &str) -> Result<alloy_primitives::B256, String> {
    let clean = s.trim().trim_start_matches("0x");
    let bytes = hex::decode(clean).map_err(|e| format!("hex decode: {e}"))?;
    if bytes.len() > 32 {
        return Err("b256 hex exceeds 32 bytes".into());
    }
    let mut padded = [0u8; 32];
    padded[32 - bytes.len()..].copy_from_slice(&bytes);
    Ok(alloy_primitives::B256::from_slice(&padded))
}

fn parse_hex_u256(s: &str) -> Result<alloy_primitives::U256, String> {
    alloy_primitives::U256::from_str_radix(s.trim_start_matches("0x"), 16)
        .map_err(|e| format!("u256 decode: {e}"))
}

fn parse_fork_chain(s: &str) -> Result<ForkChain, String> {
    match s.to_ascii_lowercase().as_str() {
        "ethereum" | "eth" | "mainnet" => Ok(ForkChain::Ethereum),
        "optimism" | "op" => Ok(ForkChain::Optimism),
        "arbitrum" | "arb" => Ok(ForkChain::Arbitrum),
        "polygon" | "matic" => Ok(ForkChain::Polygon),
        "base" => Ok(ForkChain::Base),
        "bnb" | "bsc" => Ok(ForkChain::Bnb),
        other => Err(format!("unknown chain: {other}")),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolContext {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::test(dir.path().to_path_buf());
        std::mem::forget(dir);
        ctx
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Runtime::new().unwrap().block_on(f)
    }

    #[test]
    fn selector_parser_accepts_0x_and_bare() {
        assert_eq!(parse_selector("0xa9059cbb").unwrap(), [0xa9, 0x05, 0x9c, 0xbb]);
        assert_eq!(parse_selector("a9059cbb").unwrap(), [0xa9, 0x05, 0x9c, 0xbb]);
    }

    #[test]
    fn selector_parser_rejects_wrong_length() {
        assert!(parse_selector("0xab").is_err());
    }

    #[test]
    fn chain_parser_accepts_aliases() {
        assert_eq!(parse_fork_chain("ethereum").unwrap(), ForkChain::Ethereum);
        assert_eq!(parse_fork_chain("mainnet").unwrap(), ForkChain::Ethereum);
        assert_eq!(parse_fork_chain("OP").unwrap(), ForkChain::Optimism);
        assert!(parse_fork_chain("sepolia").is_err());
    }

    #[test]
    fn find_callers_without_resolved_system_errors_clearly() {
        let c = ctx();
        let r = block_on(FindCallersOfTool.execute(
            serde_json::json!({
                "root_address": "0x0000000000000000000000000000000000000001",
                "target_address": "0x0000000000000000000000000000000000000002",
                "selector": "0xa9059cbb"
            }),
            &c,
        ));
        match r {
            ToolResult::Err { message, retryable } => {
                assert!(message.contains("system not resolved"));
                assert!(!retryable);
            }
            ToolResult::Ok(v) => panic!("expected error, got {v:?}"),
        }
    }

    #[test]
    fn trace_state_deps_without_resolved_system_errors_clearly() {
        let c = ctx();
        let r = block_on(TraceStateDependenciesTool.execute(
            serde_json::json!({
                "root_address": "0x0000000000000000000000000000000000000001",
                "contract_address": "0x0000000000000000000000000000000000000002",
                "selector": "0xa9059cbb"
            }),
            &c,
        ));
        assert!(matches!(r, ToolResult::Err { .. }));
    }

    #[test]
    fn simulate_without_exec_backend_errors_clearly() {
        let c = ctx();
        let r = block_on(SimulateCallChainTool.execute(
            serde_json::json!({
                "chain": "ethereum",
                "fork_block": 18_000_000,
                "steps": []
            }),
            &c,
        ));
        match r {
            ToolResult::Err { message, .. } => {
                assert!(message.contains("execution backend"));
            }
            ToolResult::Ok(v) => panic!("expected error, got {v:?}"),
        }
    }

    #[test]
    fn foundry_test_without_rpc_url_errors() {
        let c = ctx(); // default config has no RPC / ALCHEMY_API_KEY
        let r = block_on(BuildAndRunFoundryTestTool.execute(
            serde_json::json!({
                "chain": "ethereum",
                "fork_block": 18_000_000,
                "test_source": "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.0;\nimport \"forge-std/Test.sol\";\ncontract T is Test { function testFoo() public {} }"
            }),
            &c,
        ));
        // Should fail on RPC resolution, not panic.
        match r {
            ToolResult::Err { .. } => {}
            ToolResult::Ok(v) => panic!("unexpected success: {v:?}"),
        }
    }

    #[test]
    fn hex_u256_parser() {
        assert_eq!(
            parse_hex_u256("0xff").unwrap(),
            alloy_primitives::U256::from(255),
        );
    }

    #[test]
    fn hex_b256_parser_pads_short_input() {
        let b = parse_hex_b256("0x01").unwrap();
        let bytes = b.0;
        assert_eq!(bytes[31], 0x01);
        assert!(bytes[..31].iter().all(|b| *b == 0));
    }

    #[test]
    fn names_match_constants() {
        assert_eq!(FindCallersOfTool.name(), FIND_CALLERS_OF_NAME);
        assert_eq!(TraceStateDependenciesTool.name(), TRACE_STATE_DEPENDENCIES_NAME);
        assert_eq!(SimulateCallChainTool.name(), SIMULATE_CALL_CHAIN_NAME);
        assert_eq!(
            BuildAndRunFoundryTestTool.name(),
            BUILD_AND_RUN_FOUNDRY_TEST_NAME,
        );
    }

    #[test]
    fn all_schemas_are_object_with_required() {
        for schema in [
            FindCallersOfTool.input_schema(),
            TraceStateDependenciesTool.input_schema(),
            SimulateCallChainTool.input_schema(),
            BuildAndRunFoundryTestTool.input_schema(),
        ] {
            assert_eq!(schema["type"], "object");
            assert!(schema["required"].is_array());
        }
    }

}
