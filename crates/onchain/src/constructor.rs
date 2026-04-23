//! Constructor-argument recovery from the creation transaction.
//!
//! Strategy:
//!   1. Ask the explorer chain for `(tx_hash, creator, creation_block)`.
//!   2. Fetch the creation transaction via RPC.
//!   3. Match the contract's runtime bytecode as a substring of the tx
//!      input. Everything after the runtime is treated as constructor args.
//!   4. ABI decoding is left to the caller / a future checkpoint — this
//!      function returns the raw args and lets downstream layers decide.
//!
//! Known limitations (documented rather than fixed):
//! - Contracts with Solidity `immutable`s have a runtime-template embedded
//!   in their init code with zero-placeholders, so the stored runtime may
//!   differ. Those contracts fall into the "no match" branch and surface
//!   as `Some(ConstructorArgs { raw: empty, ... })`.
//! - Factory-deployed contracts (CREATE2 from a factory) may have the
//!   runtime generated programmatically; the input-tail heuristic won't
//!   reconstruct those cleanly.

use alloy_primitives::{Address, Bytes};
use basilisk_core::Chain;
use basilisk_explorers::ExplorerChain;
use basilisk_rpc::RpcProvider;

use crate::{enrichment::ConstructorArgs, error::IngestError};

/// Recover constructor arguments for `contract` by matching its `runtime`
/// bytecode against the creation transaction's `input` field.
///
/// Returns `Ok(None)` if no explorer surfaced creation info (we don't fall
/// back to block scanning in this checkpoint — see the module docs for
/// scope boundaries).
pub async fn recover_constructor_args(
    rpc: &dyn RpcProvider,
    explorers: &ExplorerChain,
    chain: &Chain,
    contract: Address,
    runtime: &Bytes,
) -> Result<Option<ConstructorArgs>, IngestError> {
    let info = match explorers.resolve_creation(chain, contract).await {
        Ok(Some(info)) => info,
        Ok(None) => {
            tracing::debug!(address = %contract, "no explorer returned creation info");
            return Ok(None);
        }
        Err(e) => {
            tracing::debug!(error = %e, "creation info lookup failed");
            return Ok(None);
        }
    };

    let tx = match rpc.get_transaction(info.tx_hash).await {
        Ok(Some(tx)) => tx,
        Ok(None) => {
            tracing::warn!(tx = %info.tx_hash, "creation tx not found on RPC");
            return Ok(Some(ConstructorArgs {
                raw: Bytes::new(),
                decoded: None,
                creation_tx: info.tx_hash,
                creator: info.creator,
                creation_block: info.block_number.unwrap_or(0),
            }));
        }
        Err(e) => return Err(IngestError::Rpc(e)),
    };

    let input = extract_tx_input(&tx);
    let args = match_args_from_input(input.as_ref(), runtime.as_ref());

    Ok(Some(ConstructorArgs {
        raw: Bytes::from(args),
        decoded: None, // ABI decoding deferred to future checkpoint.
        creation_tx: info.tx_hash,
        creator: info.creator,
        creation_block: info.block_number.unwrap_or(0),
    }))
}

/// Pull the `input` bytes out of an alloy `Transaction`. We go through
/// JSON rather than a direct field access so we don't couple this crate
/// to the exact shape of alloy's `Transaction` type, which changes
/// between major versions.
fn extract_tx_input(tx: &basilisk_rpc::RpcTransaction) -> Bytes {
    serde_json::to_value(tx)
        .ok()
        .and_then(|v| v.get("input").and_then(|i| i.as_str()).map(str::to_string))
        .and_then(|hex_str| {
            let body = hex_str.strip_prefix("0x").unwrap_or(&hex_str);
            hex_decode(body).map(Bytes::from)
        })
        .unwrap_or_default()
}

fn hex_decode(body: &str) -> Option<Vec<u8>> {
    if !body.len().is_multiple_of(2) {
        return None;
    }
    let mut out = vec![0u8; body.len() / 2];
    for (i, chunk) in body.as_bytes().chunks(2).enumerate() {
        out[i] = (hex_digit(chunk[0])? << 4) | hex_digit(chunk[1])?;
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

/// Find the runtime bytes inside the creation input, return everything
/// after. Empty vec if no match.
fn match_args_from_input(input: &[u8], runtime: &[u8]) -> Vec<u8> {
    if runtime.is_empty() {
        return Vec::new();
    }
    // Search for runtime as a substring of input.
    if let Some(pos) = find_subslice(input, runtime) {
        let tail_start = pos + runtime.len();
        if tail_start <= input.len() {
            return input[tail_start..].to_vec();
        }
    }
    // Fallback: try matching a metadata-stripped prefix. Solidity appends
    // a CBOR metadata blob; if the stored runtime ends with 0xa2 0x64 0x69
    // 0x70 ... or similar, strip the last 53 bytes (typical size) and
    // retry. We keep this best-effort.
    if runtime.len() > 60 {
        let stripped = &runtime[..runtime.len() - 53];
        if let Some(pos) = find_subslice(input, stripped) {
            // Use the last-match tail. Ambiguous for contracts with
            // duplicated template fragments — log a note.
            tracing::debug!("constructor-args extraction used metadata-stripped prefix match",);
            let tail_start = pos + stripped.len();
            if tail_start + 53 <= input.len() {
                return input[tail_start + 53..].to_vec();
            }
        }
    }
    Vec::new()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    for i in 0..=(haystack.len() - needle.len()) {
        if &haystack[i..i + needle.len()] == needle {
            return Some(i);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_args_after_runtime_match() {
        let runtime = vec![0x60u8, 0x80, 0x60, 0x40, 0x52];
        let args: &[u8] = &[0xaa, 0xbb, 0xcc, 0xdd];
        // init_code (arbitrary) + runtime + args
        let mut input = vec![0x00u8, 0x11, 0x22];
        input.extend_from_slice(&runtime);
        input.extend_from_slice(args);
        let got = match_args_from_input(&input, &runtime);
        assert_eq!(got, args);
    }

    #[test]
    fn no_match_returns_empty() {
        let runtime = vec![0x60u8, 0x80, 0x60, 0x40, 0x52];
        let input = vec![0x11u8, 0x22, 0x33];
        assert!(match_args_from_input(&input, &runtime).is_empty());
    }

    #[test]
    fn empty_runtime_returns_empty() {
        let input = vec![0xaa, 0xbb];
        assert!(match_args_from_input(&input, &[]).is_empty());
    }

    #[test]
    fn exact_match_yields_no_args() {
        let runtime = vec![0x60u8, 0x80, 0x60, 0x40];
        // input = just the runtime — no args tail.
        let got = match_args_from_input(&runtime, &runtime);
        assert!(got.is_empty());
    }

    #[test]
    fn find_subslice_basic() {
        assert_eq!(find_subslice(b"hello world", b"world"), Some(6));
        assert_eq!(find_subslice(b"hello world", b"xxxx"), None);
        assert_eq!(find_subslice(b"hello", b""), None);
        assert_eq!(find_subslice(b"hi", b"hello"), None);
    }
}
