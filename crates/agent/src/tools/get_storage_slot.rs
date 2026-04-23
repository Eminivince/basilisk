//! `get_storage_slot` — direct `eth_getStorageAt`.

use alloy_primitives::{Address, B256};
use async_trait::async_trait;
use basilisk_core::Chain;
use basilisk_rpc::{AlloyProvider, RpcProvider};
use serde::Deserialize;

use crate::tool::{Tool, ToolContext, ToolResult};

pub struct GetStorageSlot;

#[derive(Deserialize)]
struct Input {
    address: String,
    chain: String,
    slot: String,
}

#[async_trait]
impl Tool for GetStorageSlot {
    fn name(&self) -> &'static str {
        "get_storage_slot"
    }

    fn description(&self) -> &'static str {
        "Read one 32-byte storage slot from a contract via `eth_getStorageAt`. Use for ad-hoc \
         proxy-slot introspection (EIP-1967 implementation/admin/beacon slots, custom proxy \
         slots, diamond storage positions). Returns the raw 32-byte value as a hex string. \
         Prefer `resolve_onchain_contract` when you want proxy detection + source + \
         implementation resolution together."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["address", "chain", "slot"],
            "properties": {
                "address": {
                    "type": "string",
                    "description": "Contract address (0x-prefixed 40 hex chars)."
                },
                "chain": {
                    "type": "string",
                    "description": "Chain name: `ethereum`, `sepolia`, `arbitrum`, `base`, `optimism`, `polygon`, `bnb`, `avalanche`, etc."
                },
                "slot": {
                    "type": "string",
                    "description": "Slot index as 0x-prefixed hex (32 bytes; shorter values are left-padded)."
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
        let slot: B256 = match parse_b256(&input.slot) {
            Ok(s) => s,
            Err(e) => return ToolResult::err(format!("bad slot: {e}"), false),
        };

        let provider = match AlloyProvider::for_chain(&chain, &ctx.config) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(format!("no RPC for chain: {e}"), false),
        };
        match provider.get_storage_at(address, slot).await {
            Ok(value) => ToolResult::ok(serde_json::json!({
                "value": format!("{value:#x}"),
            })),
            Err(e) => {
                let retryable = e.is_transient();
                ToolResult::err(format!("rpc error: {e}"), retryable)
            }
        }
    }
}

/// Parse a hex string into a 32-byte value. Accepts 0x-prefix, case-
/// insensitive hex; shorter values are zero-padded on the left (so
/// `"0x0"` becomes `0x0000…0000`).
fn parse_b256(s: &str) -> Result<B256, String> {
    let stripped = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    if stripped.is_empty() {
        return Err("empty slot value".into());
    }
    if stripped.len() > 64 {
        return Err(format!(
            "slot too long: {} hex chars (max 64)",
            stripped.len()
        ));
    }
    if !stripped.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("non-hex characters in slot".into());
    }
    let padded = format!("{stripped:0>64}");
    let mut bytes = [0u8; 32];
    for (i, chunk) in padded.as_bytes().chunks(2).enumerate() {
        let hi = hex_digit(chunk[0])?;
        let lo = hex_digit(chunk[1])?;
        bytes[i] = (hi << 4) | lo;
    }
    Ok(B256::from(bytes))
}

fn hex_digit(c: u8) -> Result<u8, String> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(format!("non-hex byte: {c:#x}")),
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
        GetStorageSlot.execute(input, &ctx).await
    }

    #[test]
    fn parse_b256_pads_short_values() {
        let s = parse_b256("0x7").unwrap();
        assert_eq!(format!("{s:#x}"), format!("0x{:0>64}", "7"));
    }

    #[test]
    fn parse_b256_rejects_non_hex() {
        assert!(parse_b256("0xZZ").is_err());
    }

    #[test]
    fn parse_b256_rejects_too_long() {
        let too = format!("0x{:A>66}", "");
        assert!(parse_b256(&too).is_err());
    }

    #[tokio::test]
    async fn unknown_chain_is_non_retryable() {
        let res = run(serde_json::json!({
            "address": "0x0000000000000000000000000000000000000000",
            "chain": "cosmic-ray",
            "slot": "0x0",
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
            "address": "0xnotanaddress",
            "chain": "ethereum",
            "slot": "0x0",
        }))
        .await;
        match res {
            ToolResult::Err { retryable, .. } => assert!(!retryable),
            other => panic!("got {other:?}"),
        }
    }
}
