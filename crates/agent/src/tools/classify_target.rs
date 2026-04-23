//! `classify_target` — turn an arbitrary input string into a typed `Target`.

use async_trait::async_trait;
use basilisk_core::{detect, Chain};
use serde::Deserialize;

use crate::tool::{Tool, ToolContext, ToolResult};

pub struct ClassifyTarget;

#[derive(Deserialize)]
struct Input {
    input: String,
    #[serde(default)]
    chain_hint: Option<String>,
}

#[async_trait]
impl Tool for ClassifyTarget {
    fn name(&self) -> &'static str {
        "classify_target"
    }

    fn description(&self) -> &'static str {
        "Classify an arbitrary user-supplied input into a typed target: a GitHub repository URL, \
         an on-chain address, a local filesystem path, or an unknown input with a structured \
         reason. This is almost always your first tool call when given an opaque input. Never \
         fails — unrecognisable inputs return the `unknown` variant with a hint."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["input"],
            "properties": {
                "input": {
                    "type": "string",
                    "description": "The raw string the user supplied. URLs, addresses, paths, free-form text all accepted."
                },
                "chain_hint": {
                    "type": "string",
                    "description": "Optional chain name to associate with the target when it resolves to an on-chain address. Examples: `ethereum`, `sepolia`, `arbitrum`, `base`, `polygon`, `bnb`. Ignored for non-address inputs."
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::err(format!("invalid input: {e}"), false),
        };
        let chain = input
            .chain_hint
            .as_deref()
            .and_then(|s| s.parse::<Chain>().ok());
        let target = detect(&input.input, chain);
        ToolResult::ok(target)
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
        ClassifyTarget.execute(input, &ctx).await
    }

    #[tokio::test]
    async fn classifies_github_url() {
        let res = run(serde_json::json!({
            "input": "https://github.com/foundry-rs/foundry",
        }))
        .await;
        match res {
            ToolResult::Ok(v) => {
                assert!(v.to_string().contains("Github"));
                assert!(v.to_string().contains("foundry-rs"));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn classifies_address_with_chain_hint() {
        let res = run(serde_json::json!({
            "input": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
            "chain_hint": "ethereum",
        }))
        .await;
        match res {
            ToolResult::Ok(v) => {
                // Target::OnChain is tagged externally; chain serialises as
                // its canonical variant identifier (EthereumMainnet).
                let s = v.to_string();
                assert!(s.contains("OnChain"), "got: {s}");
                assert!(
                    s.contains("EthereumMainnet") || s.contains("ethereum"),
                    "got: {s}",
                );
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_input_returns_unknown_variant_not_error() {
        let res = run(serde_json::json!({ "input": "gibberish not a target" })).await;
        match res {
            ToolResult::Ok(v) => {
                assert!(v.to_string().contains("Unknown"));
            }
            other => panic!("expected Ok with Unknown target, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bad_input_shape_is_non_retryable() {
        let res = run(serde_json::json!({ "not_input": 1 })).await;
        match res {
            ToolResult::Err { retryable, .. } => assert!(!retryable),
            other => panic!("got {other:?}"),
        }
    }
}
