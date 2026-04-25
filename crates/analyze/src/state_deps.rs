//! `trace_state_dependencies` — for one function in one contract,
//! identify storage reads, storage writes, and external calls.
//!
//! Two layers of precision are surfaced; the `precision` field on
//! the result tells the agent which it got:
//!
//! - **`bytecode_static`** (always available): walk the whole
//!   contract's bytecode, recording PUSH-immediate→SLOAD,
//!   PUSH-immediate→SSTORE, and CALL-family targets. Scope is the
//!   whole contract — we don't isolate one function's basic blocks
//!   because that requires CFG construction we don't ship in Set 9.
//! - **`mixed`** (when verified source + ABI is available): also scan
//!   the matching function's body in source, returning a narrower
//!   set tagged with file/line. The `bytecode_static` set is still
//!   returned so the agent can compare.
//!
//! For reentrancy reasoning the agent typically wants both: the
//! source-narrow set tells "what does *this* function touch;" the
//! whole-contract set tells "what else might fire on a callback."

use alloy_primitives::{Address, B256};
use basilisk_onchain::ResolvedSystem;
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};

use crate::{bytecode::InstructionWalker, error::AnalyzeError};

/// Result of [`trace_state_dependencies`]. All fields default-empty
/// so an agent on a contract with no source / no matching function /
/// empty bytecode still gets a well-formed reply.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateDeps {
    pub reads: Vec<SlotRef>,
    pub writes: Vec<SlotRef>,
    pub external_calls: Vec<ExternalCall>,
    /// `"bytecode_static"` | `"mixed"` | `"none"` (when contract has
    /// neither bytecode nor source).
    pub precision: Precision,
    /// When `precision == Mixed`, the function's resolved name from
    /// the ABI. Useful for the agent to confirm we matched the right
    /// function.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_function_name: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Precision {
    #[default]
    None,
    BytecodeStatic,
    Mixed,
}

/// A storage slot referenced by the contract. When the slot value
/// was PUSH'd as a constant immediately before the SLOAD/SSTORE,
/// `slot` is populated; for dynamically-computed slots (from
/// keccak256 etc.), `slot` is `None` and we record the bytecode
/// offset of the SLOAD/SSTORE for human follow-up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotRef {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<B256>,
    pub bytecode_offset: usize,
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Bytecode,
    SourceText { file: String, line: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalCall {
    /// Target address when literally pushed before the CALL; `None`
    /// when the address came from storage / calldata / dynamic
    /// computation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Address>,
    pub kind: super::callers::CallKind,
    pub bytecode_offset: usize,
    pub source: Source,
}

/// The headline entry point.
pub fn trace_state_dependencies(
    system: &ResolvedSystem,
    contract_address: Address,
    selector: [u8; 4],
) -> Result<StateDeps, AnalyzeError> {
    let contract = system
        .contracts
        .get(&contract_address)
        .ok_or_else(|| AnalyzeError::UnknownAddress(format!("0x{}", hex::encode(contract_address.as_slice()))))?;

    let mut out = StateDeps::default();
    if contract.bytecode.is_empty() && contract.source.is_none() {
        return Ok(out);
    }

    // Bytecode pass — always runs when bytecode present.
    if !contract.bytecode.is_empty() {
        out = scan_bytecode(contract.bytecode.as_ref());
        out.precision = Precision::BytecodeStatic;
    }

    // Source pass — narrows to one function when ABI + source align.
    if let Some(source) = &contract.source {
        if let Some(name) = find_function_name_for_selector(&source.abi, selector) {
            // Walk source files for `function <name>(...) { ... }`.
            for (path, body) in &source.source_files {
                if let Some(extra) = scan_source_function(&path.display().to_string(), body, &name) {
                    // Append source-tagged refs without removing the
                    // bytecode set — the agent gets both views.
                    out.reads.extend(extra.reads);
                    out.writes.extend(extra.writes);
                    out.external_calls.extend(extra.external_calls);
                }
            }
            out.matched_function_name = Some(name);
            out.precision = Precision::Mixed;
        }
    }

    // Stable ordering for diffs.
    out.reads.sort_by_key(|r| r.bytecode_offset);
    out.writes.sort_by_key(|r| r.bytecode_offset);
    out.external_calls.sort_by_key(|c| c.bytecode_offset);

    Ok(out)
}

/// Walk the bytecode, tracking the most-recently-PUSH'd value so
/// SLOAD / SSTORE / CALL-family ops can pull the slot or address
/// from it. This is a peephole heuristic: anything more sophisticated
/// (DUP / SWAP-aware, keccak-tracking) is real CFG analysis.
fn scan_bytecode(bytecode: &[u8]) -> StateDeps {
    let mut out = StateDeps::default();
    // Last PUSH immediate seen, paired with the opcode width so we
    // can downgrade on partial pushes.
    let mut last_push: Option<Vec<u8>> = None;
    for inst in InstructionWalker::new(bytecode) {
        let op = inst.opcode;
        if (0x60..=0x7f).contains(&op) {
            last_push = Some(inst.immediate.to_vec());
            continue;
        }
        match op {
            0x54 => {
                // SLOAD
                let slot = last_push
                    .as_ref()
                    .and_then(|v| if v.len() <= 32 { Some(pad_b256(v)) } else { None });
                out.reads.push(SlotRef {
                    slot,
                    bytecode_offset: inst.pc,
                    source: Source::Bytecode,
                });
                last_push = None;
            }
            0x55 => {
                // SSTORE — top-of-stack is the value, but the slot
                // was PUSH'd before the value. The peephole here is
                // imperfect (stack reorderings via DUP/SWAP escape
                // detection); we still record the SSTORE position
                // even when we can't pin the slot.
                out.writes.push(SlotRef {
                    slot: None, // pessimistic: writes' slot lives below value on stack
                    bytecode_offset: inst.pc,
                    source: Source::Bytecode,
                });
                last_push = None;
            }
            0xf1 | 0xf2 | 0xf4 | 0xfa => {
                let kind = match op {
                    0xf1 => super::callers::CallKind::Call,
                    0xf2 => super::callers::CallKind::CallCode,
                    0xf4 => super::callers::CallKind::DelegateCall,
                    0xfa => super::callers::CallKind::StaticCall,
                    _ => super::callers::CallKind::Unknown,
                };
                // Try to pull the destination address from a recent
                // PUSH20 immediate. We don't track stack precisely;
                // peephole only.
                let to = last_push.as_ref().and_then(|v| {
                    if v.len() == 20 {
                        Some(Address::from_slice(v))
                    } else {
                        None
                    }
                });
                out.external_calls.push(ExternalCall {
                    to,
                    kind,
                    bytecode_offset: inst.pc,
                    source: Source::Bytecode,
                });
                last_push = None;
            }
            _ => {
                // Most non-PUSH opcodes consume stack and clobber any
                // notion of "the last push lined up for the next
                // sensitive op." Stay conservative: clear when we
                // see an opcode that pops or transforms.
                last_push = None;
            }
        }
    }
    out
}

fn pad_b256(bytes: &[u8]) -> B256 {
    let mut padded = [0u8; 32];
    let n = bytes.len().min(32);
    padded[32 - n..].copy_from_slice(&bytes[..n]);
    B256::from_slice(&padded)
}

/// Walk the ABI for a function whose selector matches `target`.
/// Returns the function name (just the bare identifier, no
/// parentheses) on hit.
pub(crate) fn find_function_name_for_selector(
    abi: &serde_json::Value,
    target: [u8; 4],
) -> Option<String> {
    let entries = abi.as_array()?;
    for e in entries {
        if e.get("type").and_then(|v| v.as_str()) != Some("function") {
            continue;
        }
        let name = e.get("name").and_then(|v| v.as_str())?;
        let inputs = e.get("inputs").and_then(|v| v.as_array());
        let sig_params = inputs
            .map(|arr| {
                arr.iter()
                    .filter_map(|i| i.get("type").and_then(|v| v.as_str()))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        let signature = format!("{name}({sig_params})");
        let mut hasher = Keccak256::new();
        hasher.update(signature.as_bytes());
        let digest = hasher.finalize();
        if digest[..4] == target {
            return Some(name.to_string());
        }
    }
    None
}

/// Scan source for `function <name>(...) { ... }` and pull
/// SLOAD/SSTORE/CALL references from the body. Crude — we don't
/// parse Solidity; we look for telltale patterns line by line.
fn scan_source_function(file: &str, body: &str, fn_name: &str) -> Option<StateDeps> {
    let mut out = StateDeps::default();
    let pattern = format!("function {fn_name}(");
    let mut depth: i32 = 0;
    let mut in_fn = false;
    for (idx, line) in body.lines().enumerate() {
        let line_no = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        if !in_fn && line.contains(&pattern) {
            in_fn = true;
        }
        if !in_fn {
            continue;
        }
        // Track brace depth so we know when the function ends.
        let opens = i32::try_from(line.matches('{').count()).unwrap_or(0);
        let closes = i32::try_from(line.matches('}').count()).unwrap_or(0);
        depth += opens - closes;

        // Heuristic patterns: storage write (`<ident> = `), storage
        // read (`<ident>` on RHS — too noisy to track reliably; skip
        // unless inside a `return` or a comparison), external call
        // (`.call(`, `.delegatecall(`, `.staticcall(`, `<contract>.<method>(`).
        let trimmed = line.trim();
        if trimmed.contains(".delegatecall(") {
            out.external_calls.push(ExternalCall {
                to: None,
                kind: super::callers::CallKind::DelegateCall,
                bytecode_offset: 0,
                source: Source::SourceText {
                    file: file.into(),
                    line: line_no,
                },
            });
        } else if trimmed.contains(".staticcall(") {
            out.external_calls.push(ExternalCall {
                to: None,
                kind: super::callers::CallKind::StaticCall,
                bytecode_offset: 0,
                source: Source::SourceText {
                    file: file.into(),
                    line: line_no,
                },
            });
        } else if trimmed.contains(".call(") {
            out.external_calls.push(ExternalCall {
                to: None,
                kind: super::callers::CallKind::Call,
                bytecode_offset: 0,
                source: Source::SourceText {
                    file: file.into(),
                    line: line_no,
                },
            });
        }
        // Storage write heuristic: simple `name = ` at start (not `==`).
        if let Some(stripped) = trimmed.strip_suffix(';') {
            if let Some(eq_pos) = stripped.find('=') {
                let before = &stripped[..eq_pos];
                let after = &stripped[eq_pos + 1..];
                let next = stripped.as_bytes().get(eq_pos + 1).copied();
                let prev = if eq_pos == 0 { 0u8 } else { stripped.as_bytes()[eq_pos - 1] };
                // Filter `==`, `!=`, `<=`, `>=`, etc. Need a single `=`.
                if next != Some(b'=') && prev != b'!' && prev != b'<' && prev != b'>' && prev != b'=' {
                    // Looks like an assignment. Heuristic for storage
                    // (vs local): if the LHS doesn't start with a
                    // type keyword or `var`, treat as storage write.
                    let lhs = before.trim();
                    let _ = after;
                    let local_kw = ["uint", "int", "bool", "address", "bytes", "string", "var", "mapping", "struct"];
                    let is_local = local_kw.iter().any(|kw| lhs.starts_with(kw));
                    if !is_local && !lhs.is_empty() {
                        out.writes.push(SlotRef {
                            slot: None,
                            bytecode_offset: 0,
                            source: Source::SourceText {
                                file: file.into(),
                                line: line_no,
                            },
                        });
                    }
                }
            }
        }

        if depth == 0 && in_fn {
            break;
        }
    }
    if out.reads.is_empty() && out.writes.is_empty() && out.external_calls.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::SystemTime;

    use alloy_primitives::Bytes;
    use basilisk_core::Chain;
    use basilisk_explorers::VerifiedSource;
    use basilisk_graph::ContractGraph;
    use basilisk_onchain::{ResolutionSources, ResolvedContract, SystemResolutionStats};

    fn mk_contract(
        addr: Address,
        bytecode: Vec<u8>,
        source: Option<(String, serde_json::Value)>,
    ) -> ResolvedContract {
        let verified = source.map(|(body, abi)| {
            let mut files = BTreeMap::new();
            files.insert(PathBuf::from("Source.sol"), body);
            VerifiedSource {
                source_files: files,
                contract_name: "T".into(),
                compiler_version: "0.8.20+commit.x".into(),
                optimizer: None,
                evm_version: None,
                abi,
                constructor_args: None,
                license: None,
                proxy_hint: None,
                implementation_hint: None,
                metadata: serde_json::json!({}),
            }
        });
        ResolvedContract {
            address: addr,
            chain: Chain::EthereumMainnet,
            bytecode: Bytes::from(bytecode),
            bytecode_hash: alloy_primitives::B256::ZERO,
            is_contract: true,
            source: verified,
            proxy: None,
            implementation: None,
            fetched_at: SystemTime::UNIX_EPOCH,
            resolution: ResolutionSources::new("rpc"),
            constructor_args: None,
            storage_layout: None,
            referenced_addresses: Vec::new(),
        }
    }

    fn mk_system(contracts: Vec<ResolvedContract>) -> ResolvedSystem {
        let mut map = BTreeMap::new();
        let root = contracts.first().map_or(Address::ZERO, |c| c.address);
        for c in contracts {
            map.insert(c.address, c);
        }
        ResolvedSystem {
            root,
            chain: Chain::EthereumMainnet,
            contracts: map,
            graph: ContractGraph::default(),
            stats: SystemResolutionStats::default(),
            resolved_at: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn unknown_address_errors() {
        let sys = mk_system(vec![]);
        let err = trace_state_dependencies(&sys, Address::repeat_byte(0xff), [0; 4]).unwrap_err();
        assert!(matches!(err, AnalyzeError::UnknownAddress(_)));
    }

    #[test]
    fn empty_contract_returns_none_precision() {
        let c = mk_contract(Address::repeat_byte(0x1), vec![], None);
        let sys = mk_system(vec![c]);
        let r = trace_state_dependencies(&sys, Address::repeat_byte(0x1), [0; 4]).unwrap();
        assert_eq!(r.precision, Precision::None);
        assert!(r.reads.is_empty());
        assert!(r.writes.is_empty());
    }

    #[test]
    fn bytecode_pass_finds_sload_with_pushed_slot() {
        // PUSH1 0x07 SLOAD STOP
        let bc = vec![0x60, 0x07, 0x54, 0x00];
        let c = mk_contract(Address::repeat_byte(0x1), bc, None);
        let sys = mk_system(vec![c]);
        let r = trace_state_dependencies(&sys, Address::repeat_byte(0x1), [0; 4]).unwrap();
        assert_eq!(r.precision, Precision::BytecodeStatic);
        assert_eq!(r.reads.len(), 1);
        let slot = r.reads[0].slot.unwrap();
        // 32 zeros except last byte which is 0x07.
        let mut expected = [0u8; 32];
        expected[31] = 0x07;
        assert_eq!(slot.0, expected);
    }

    #[test]
    fn bytecode_pass_records_sstore_offset() {
        // PUSH1 value PUSH1 slot SSTORE
        let bc = vec![0x60, 0x05, 0x60, 0x09, 0x55];
        let c = mk_contract(Address::repeat_byte(0x1), bc, None);
        let sys = mk_system(vec![c]);
        let r = trace_state_dependencies(&sys, Address::repeat_byte(0x1), [0; 4]).unwrap();
        assert_eq!(r.writes.len(), 1);
        assert_eq!(r.writes[0].bytecode_offset, 4);
    }

    #[test]
    fn bytecode_pass_pulls_address_from_push20_before_call() {
        // PUSH20 <addr> CALL
        let addr = Address::repeat_byte(0x42);
        let mut bc = vec![0x73];
        bc.extend_from_slice(addr.as_slice());
        bc.push(0xf1);
        let c = mk_contract(Address::repeat_byte(0x1), bc, None);
        let sys = mk_system(vec![c]);
        let r = trace_state_dependencies(&sys, Address::repeat_byte(0x1), [0; 4]).unwrap();
        assert_eq!(r.external_calls.len(), 1);
        assert_eq!(r.external_calls[0].to, Some(addr));
        assert_eq!(r.external_calls[0].kind, super::super::callers::CallKind::Call);
    }

    #[test]
    fn selector_lookup_finds_function_in_abi() {
        let abi = serde_json::json!([
            {
                "type": "function",
                "name": "transfer",
                "inputs": [
                    {"name": "to", "type": "address"},
                    {"name": "amount", "type": "uint256"}
                ]
            }
        ]);
        // keccak256("transfer(address,uint256)") = 0xa9059cbb...
        let sel = [0xa9, 0x05, 0x9c, 0xbb];
        let n = find_function_name_for_selector(&abi, sel);
        assert_eq!(n.as_deref(), Some("transfer"));
    }

    #[test]
    fn selector_lookup_misses_when_unknown() {
        let abi = serde_json::json!([
            {"type": "function", "name": "foo", "inputs": []}
        ]);
        assert!(find_function_name_for_selector(&abi, [0; 4]).is_none());
    }

    #[test]
    fn source_pass_narrows_when_function_matches() {
        let body = r#"contract T {
            uint public x;
            function bump() external {
                x = x + 1;
                target.call("");
            }
        }"#;
        let abi = serde_json::json!([
            {"type": "function", "name": "bump", "inputs": []}
        ]);
        // Derive the selector dynamically so the test stays correct
        // even if our hash algorithm changes.
        let mut hasher = Keccak256::new();
        hasher.update(b"bump()");
        let digest = hasher.finalize();
        let sel = [digest[0], digest[1], digest[2], digest[3]];
        let c = mk_contract(Address::repeat_byte(0x1), vec![], Some((body.into(), abi)));
        let sys = mk_system(vec![c]);
        let r = trace_state_dependencies(&sys, Address::repeat_byte(0x1), sel).unwrap();
        assert_eq!(r.precision, Precision::Mixed);
        assert_eq!(r.matched_function_name.as_deref(), Some("bump"));
        // At least one source-text record on each side.
        assert!(r.writes.iter().any(|w| matches!(w.source, Source::SourceText { .. })));
        assert!(r
            .external_calls
            .iter()
            .any(|c| matches!(c.source, Source::SourceText { .. })));
    }

    #[test]
    fn source_pass_skips_when_no_function_matches_selector() {
        let body = r"contract T { function foo() external {} }";
        let abi = serde_json::json!([{"type": "function", "name": "foo", "inputs": []}]);
        let c = mk_contract(Address::repeat_byte(0x1), vec![0x00], Some((body.into(), abi)));
        let sys = mk_system(vec![c]);
        // selector for `bump()` doesn't match `foo()`.
        let r = trace_state_dependencies(&sys, Address::repeat_byte(0x1), [0; 4]).unwrap();
        assert!(r.matched_function_name.is_none());
        // Bytecode pass still runs, so precision is bytecode_static.
        assert_eq!(r.precision, Precision::BytecodeStatic);
    }

    #[test]
    fn pad_b256_left_pads_short_input() {
        let v = [0x01, 0x02];
        let b = pad_b256(&v);
        let mut expected = [0u8; 32];
        expected[30] = 0x01;
        expected[31] = 0x02;
        assert_eq!(b.0, expected);
    }

    #[test]
    fn results_are_sorted_by_bytecode_offset() {
        // Two SLOADs at different positions.
        let bc = vec![
            0x60, 0x01, 0x54, // SLOAD at 2
            0x60, 0x02, 0x54, // SLOAD at 5
        ];
        let c = mk_contract(Address::repeat_byte(0x1), bc, None);
        let sys = mk_system(vec![c]);
        let r = trace_state_dependencies(&sys, Address::repeat_byte(0x1), [0; 4]).unwrap();
        assert_eq!(r.reads.len(), 2);
        assert!(r.reads[0].bytecode_offset < r.reads[1].bytecode_offset);
    }

    #[test]
    fn external_call_kind_disambiguates() {
        // PUSH20 + DELEGATECALL
        let addr = Address::repeat_byte(0x33);
        let mut bc = vec![0x73];
        bc.extend_from_slice(addr.as_slice());
        bc.push(0xf4);
        let c = mk_contract(Address::repeat_byte(0x1), bc, None);
        let sys = mk_system(vec![c]);
        let r = trace_state_dependencies(&sys, Address::repeat_byte(0x1), [0; 4]).unwrap();
        assert_eq!(
            r.external_calls[0].kind,
            super::super::callers::CallKind::DelegateCall
        );
    }
}
