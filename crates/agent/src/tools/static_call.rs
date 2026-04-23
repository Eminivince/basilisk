//! `static_call` — one-shot `eth_call` for contract-interface probing.

use alloy_primitives::{Address, Bytes};
use async_trait::async_trait;
use basilisk_core::Chain;
use basilisk_rpc::{AlloyProvider, RpcProvider};
use serde::Deserialize;

use crate::tool::{Tool, ToolContext, ToolResult};

pub struct StaticCall;

#[derive(Deserialize)]
struct Input {
    address: String,
    chain: String,
    data: String,
}

#[async_trait]
impl Tool for StaticCall {
    fn name(&self) -> &'static str {
        "static_call"
    }

    fn description(&self) -> &'static str {
        "Execute a single read-only `eth_call` against a contract. Use to probe interfaces at \
         runtime — call `owner()` / `paused()` / `facets()` / custom getters when verified \
         source isn't available or you want to confirm live state. Input `data` is the raw \
         calldata as 0x-hex (selector + ABI-encoded args). Returns the return data as hex and a \
         success flag. The call is never broadcast; this is pure simulation at the latest block."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["address", "chain", "data"],
            "properties": {
                "address": {
                    "type": "string",
                    "description": "Contract address (0x-prefixed)."
                },
                "chain": {
                    "type": "string",
                    "description": "Chain name (`ethereum`, `arbitrum`, etc.)."
                },
                "data": {
                    "type": "string",
                    "description": "Raw calldata as 0x-prefixed hex. For `owner()` that's `0x8da5cb5b` (4-byte selector, no args)."
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
        let data = match parse_bytes(&input.data) {
            Ok(b) => b,
            Err(e) => return ToolResult::err(format!("bad data: {e}"), false),
        };

        let provider = match AlloyProvider::for_chain(&chain, &ctx.config) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(format!("no RPC for chain: {e}"), false),
        };
        match provider.call(address, data).await {
            Ok(bytes) => ToolResult::ok(serde_json::json!({
                "result": format!("0x{}", hex_encode(&bytes)),
                "length_bytes": bytes.len(),
                "success": true,
            })),
            Err(e) => {
                let retryable = e.is_transient();
                ToolResult::err(format!("call reverted or errored: {e}"), retryable)
            }
        }
    }
}

fn parse_bytes(s: &str) -> Result<Bytes, String> {
    let stripped = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    if !stripped.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("non-hex characters".into());
    }
    if !stripped.len().is_multiple_of(2) {
        return Err(format!("odd hex length: {}", stripped.len()));
    }
    let mut out = Vec::with_capacity(stripped.len() / 2);
    for chunk in stripped.as_bytes().chunks(2) {
        let hi = hex_digit(chunk[0])?;
        let lo = hex_digit(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(Bytes::from(out))
}

fn hex_digit(c: u8) -> Result<u8, String> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(format!("non-hex byte: {c:#x}")),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
#[allow(clippy::match_wildcard_for_single_variants)]
mod tests {
    use super::*;

    async fn run(input: serde_json::Value) -> ToolResult {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::test(dir.path().to_path_buf());
        std::mem::forget(dir);
        StaticCall.execute(input, &ctx).await
    }

    #[test]
    fn parse_bytes_accepts_owner_selector() {
        let b = parse_bytes("0x8da5cb5b").unwrap();
        assert_eq!(b.len(), 4);
        assert_eq!(b[..], [0x8d, 0xa5, 0xcb, 0x5b]);
    }

    #[test]
    fn parse_bytes_rejects_odd_length() {
        assert!(parse_bytes("0xabc").is_err());
    }

    #[test]
    fn parse_bytes_rejects_non_hex() {
        assert!(parse_bytes("0xzz").is_err());
    }

    #[tokio::test]
    async fn bad_data_is_non_retryable() {
        let res = run(serde_json::json!({
            "address": "0x0000000000000000000000000000000000000000",
            "chain": "ethereum",
            "data": "not-hex",
        }))
        .await;
        match res {
            ToolResult::Err { retryable, .. } => assert!(!retryable),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_chain_is_non_retryable() {
        let res = run(serde_json::json!({
            "address": "0x0000000000000000000000000000000000000000",
            "chain": "zork",
            "data": "0x",
        }))
        .await;
        match res {
            ToolResult::Err { retryable, .. } => assert!(!retryable),
            other => panic!("got {other:?}"),
        }
    }
}
