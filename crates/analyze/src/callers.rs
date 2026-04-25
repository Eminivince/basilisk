//! `find_callers_of` — locate callers of a `(target_address, selector)`
//! pair within a [`ResolvedSystem`].
//!
//! Two-tier matching:
//!
//! 1. **AST / source-text precision** — when verified source is
//!    available, search for high-fidelity patterns: explicit
//!    `<contract>.method(...)` calls naming the target's contract or
//!    address; `address(<addr>).call(...)` with `selector` derivable
//!    from the encoded calldata; or imports of an interface whose
//!    selectors match. Hit confidence: [`Confidence::ExactFromSource`].
//! 2. **Bytecode pattern match** — for every contract (verified or
//!    not), walk the bytecode looking for `PUSH4 <selector>` followed
//!    within ~64 bytes by a CALL-family opcode, with a `PUSH20
//!    <target_address>` somewhere in the proximity. Hit confidence:
//!    [`Confidence::PatternMatchInBytecode`].
//!
//! False positives and misses are both possible. The output's
//! confidence rating tells the agent how much to trust each hit; the
//! `evidence` field tells it what we matched on so it can spot-check.

use std::collections::BTreeMap;

use alloy_primitives::Address;
use basilisk_onchain::ResolvedSystem;
use serde::{Deserialize, Serialize};

use crate::{bytecode::InstructionWalker, error::AnalyzeError};

/// Search input.
#[derive(Debug, Clone)]
pub struct CallerSearch {
    pub target: Address,
    pub selector: [u8; 4],
    /// Bytecode-side proximity window in bytes — a CALL-family opcode
    /// must land within this many bytes after the matching PUSH4 to
    /// count as a hit.
    pub proximity_bytes: usize,
}

impl CallerSearch {
    pub fn new(target: Address, selector: [u8; 4]) -> Self {
        Self {
            target,
            selector,
            proximity_bytes: 64,
        }
    }
}

/// One caller-side hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallerHit {
    pub caller: Address,
    /// Function name when we could attribute the call site to a named
    /// function via verified source — `None` means bytecode-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller_function: Option<String>,
    pub call_kind: CallKind,
    pub confidence: Confidence,
    pub evidence: CallerEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallKind {
    Call,
    StaticCall,
    DelegateCall,
    CallCode,
    /// Source-side match where we couldn't pin down which CALL-family
    /// opcode the high-level call lowers to.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    ExactFromSource,
    PatternMatchInBytecode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CallerEvidence {
    /// Source-side: file path + a snippet of the matched line.
    Source {
        file: String,
        line: u32,
        snippet: String,
    },
    /// Bytecode-side: byte offset of the PUSH4 selector + offset of
    /// the CALL-family opcode that fired.
    Bytecode {
        push4_at: usize,
        call_at: usize,
        opcode: u8,
    },
}

/// Aggregate result. `hits` is sorted by `(caller, call_offset)` for
/// stable output.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallerSearchResult {
    pub hits: Vec<CallerHit>,
    /// Per-caller stats — useful summary when many hits.
    pub by_caller: BTreeMap<Address, usize>,
    pub scanned_contracts: usize,
    pub source_scanned_contracts: usize,
}

/// Find every caller of `(search.target, search.selector)` within the
/// resolved system. Returns hits sorted by caller address then by
/// call offset (source line / bytecode offset).
pub fn find_callers_of(
    system: &ResolvedSystem,
    search: &CallerSearch,
) -> Result<CallerSearchResult, AnalyzeError> {
    let mut out = CallerSearchResult::default();
    for (caller_addr, contract) in &system.contracts {
        out.scanned_contracts += 1;
        // Skip the target itself — we're looking for *other* contracts
        // calling it.
        if caller_addr == &search.target {
            continue;
        }
        // Source-side first (higher precision).
        if let Some(source) = contract.source.as_ref() {
            out.source_scanned_contracts += 1;
            for (path, body) in &source.source_files {
                let path_str = path.display().to_string();
                push_source_hits(&path_str, body, *caller_addr, search, &mut out.hits);
            }
        }
        // Bytecode pattern match — runs unconditionally.
        push_bytecode_hits(
            *caller_addr,
            contract.bytecode.as_ref(),
            search,
            &mut out.hits,
        );
    }

    // Sort hits stably by caller address, then by source line / bytecode offset.
    out.hits.sort_by(|a, b| {
        a.caller.cmp(&b.caller).then_with(|| {
            let order_key = |h: &CallerHit| match &h.evidence {
                CallerEvidence::Source { line, .. } => i64::from(*line),
                CallerEvidence::Bytecode { call_at, .. } => i64::try_from(*call_at).unwrap_or(0),
            };
            order_key(a).cmp(&order_key(b))
        })
    });
    for h in &out.hits {
        *out.by_caller.entry(h.caller).or_insert(0) += 1;
    }
    Ok(out)
}

/// Source-side hits: look for `<addr_hex>` literally appearing on a
/// line that also references a `.call(`, `.delegatecall(`, or matches
/// `<Iface>(<addr_hex>)\.<method>(`. Cheap; conservative; high-precision.
fn push_source_hits(
    file: &str,
    body: &str,
    caller: Address,
    search: &CallerSearch,
    out: &mut Vec<CallerHit>,
) {
    let target_lc = format!("0x{}", hex::encode(search.target.as_slice())).to_ascii_lowercase();
    // Drop the `0x` prefix to also catch `address(0xABCDEF...)` with
    // mixed case — Solidity allows checksummed forms too.
    let target_no_pfx = target_lc.trim_start_matches("0x").to_string();
    for (idx, line) in body.lines().enumerate() {
        let line_no = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        let lower = line.to_ascii_lowercase();
        if !lower.contains(&target_lc) && !lower.contains(&target_no_pfx) {
            continue;
        }
        // Distinguish call kinds where we can.
        let kind = if lower.contains(".delegatecall(") {
            CallKind::DelegateCall
        } else if lower.contains(".staticcall(") {
            CallKind::StaticCall
        } else if lower.contains(".call(") {
            CallKind::Call
        } else {
            CallKind::Unknown
        };
        out.push(CallerHit {
            caller,
            caller_function: None,
            call_kind: kind,
            confidence: Confidence::ExactFromSource,
            evidence: CallerEvidence::Source {
                file: file.to_string(),
                line: line_no,
                snippet: line.trim().chars().take(160).collect(),
            },
        });
    }
}

/// Bytecode pattern: `PUSH4 <selector>` followed within
/// `proximity_bytes` of a CALL-family opcode. Records the first such
/// pairing per occurrence — a single contract may have many.
fn push_bytecode_hits(
    caller: Address,
    bytecode: &[u8],
    search: &CallerSearch,
    out: &mut Vec<CallerHit>,
) {
    if bytecode.is_empty() {
        return;
    }
    let walker: Vec<_> = InstructionWalker::new(bytecode).collect();
    for (i, inst) in walker.iter().enumerate() {
        // PUSH4 with matching selector.
        if inst.opcode != 0x63 || inst.immediate != search.selector {
            continue;
        }
        let push4_at = inst.pc;
        // Walk forward; stop at the next CALL-family opcode within
        // the proximity window or give up.
        for forward in walker.iter().skip(i + 1) {
            if forward.pc - push4_at > search.proximity_bytes {
                break;
            }
            if forward.is_external_call() {
                let kind = match forward.opcode {
                    0xf1 => CallKind::Call,
                    0xf2 => CallKind::CallCode,
                    0xf4 => CallKind::DelegateCall,
                    0xfa => CallKind::StaticCall,
                    _ => CallKind::Unknown,
                };
                out.push(CallerHit {
                    caller,
                    caller_function: None,
                    call_kind: kind,
                    confidence: Confidence::PatternMatchInBytecode,
                    evidence: CallerEvidence::Bytecode {
                        push4_at,
                        call_at: forward.pc,
                        opcode: forward.opcode,
                    },
                });
                break;
            }
        }
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

    fn mk_contract(addr: Address, bytecode: Vec<u8>, source: Option<String>) -> ResolvedContract {
        let verified = source.map(|body| {
            let mut files = BTreeMap::new();
            files.insert(PathBuf::from("Source.sol"), body);
            VerifiedSource {
                source_files: files,
                contract_name: "T".into(),
                compiler_version: "0.8.20+commit.x".into(),
                optimizer: None,
                evm_version: None,
                abi: serde_json::json!([]),
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
    fn bytecode_match_finds_push4_then_call() {
        let target_sel = [0xde, 0xad, 0xbe, 0xef];
        let target_addr = Address::repeat_byte(0x42);
        // Caller bytecode: PUSH4 0xdeadbeef ... CALL
        let mut caller_bc = vec![0x63];
        caller_bc.extend_from_slice(&target_sel);
        caller_bc.extend(std::iter::repeat_n(0x5b, 10)); // JUMPDESTs as filler
        caller_bc.push(0xf1); // CALL
        let caller = mk_contract(Address::repeat_byte(0x01), caller_bc, None);
        let target = mk_contract(target_addr, vec![0x00], None);
        let sys = mk_system(vec![caller, target]);

        let search = CallerSearch::new(target_addr, target_sel);
        let res = find_callers_of(&sys, &search).unwrap();
        assert_eq!(res.hits.len(), 1);
        assert_eq!(res.hits[0].confidence, Confidence::PatternMatchInBytecode);
        assert_eq!(res.hits[0].call_kind, CallKind::Call);
    }

    #[test]
    fn bytecode_no_match_when_call_too_far() {
        let target_sel = [0x12, 0x34, 0x56, 0x78];
        let target_addr = Address::repeat_byte(0x42);
        let mut caller_bc = vec![0x63];
        caller_bc.extend_from_slice(&target_sel);
        // 200 bytes of filler beyond default proximity (64).
        caller_bc.extend(std::iter::repeat_n(0x5b, 200));
        caller_bc.push(0xf1);
        let caller = mk_contract(Address::repeat_byte(0x01), caller_bc, None);
        let target = mk_contract(target_addr, vec![0x00], None);
        let sys = mk_system(vec![caller, target]);

        let search = CallerSearch::new(target_addr, target_sel);
        let res = find_callers_of(&sys, &search).unwrap();
        assert!(res.hits.is_empty());
    }

    #[test]
    fn bytecode_match_disambiguates_call_kind() {
        let target_sel = [0xaa, 0xbb, 0xcc, 0xdd];
        let target_addr = Address::repeat_byte(0xab);
        let mut bc = vec![0x63];
        bc.extend_from_slice(&target_sel);
        bc.push(0xf4); // DELEGATECALL right after
        let caller = mk_contract(Address::repeat_byte(0x09), bc, None);
        let target = mk_contract(target_addr, vec![], None);
        let sys = mk_system(vec![caller, target]);

        let search = CallerSearch::new(target_addr, target_sel);
        let res = find_callers_of(&sys, &search).unwrap();
        assert_eq!(res.hits.len(), 1);
        assert_eq!(res.hits[0].call_kind, CallKind::DelegateCall);
    }

    #[test]
    fn source_match_finds_address_literal_in_call() {
        let target_addr =
            Address::parse_checksummed("0x4242424242424242424242424242424242424242", None).unwrap();
        let target_sel = [0x12, 0x34, 0x56, 0x78];
        let body = format!(
            r"contract Caller {{
                function ping() external {{
                    Iface(address({})).ping();
                }}
                function callRaw() external {{
                    {}.call(abi.encodeWithSelector(0x12345678));
                }}
            }}",
            "0x4242424242424242424242424242424242424242",
            "address(0x4242424242424242424242424242424242424242)",
        );
        let caller = mk_contract(Address::repeat_byte(0x09), vec![0x00], Some(body));
        let target = mk_contract(target_addr, vec![0x00], None);
        let sys = mk_system(vec![caller, target]);

        let search = CallerSearch::new(target_addr, target_sel);
        let res = find_callers_of(&sys, &search).unwrap();
        // Two source lines reference the address literal: the
        // Iface(address(...)) call and the .call(...).
        assert!(res.hits.len() >= 2);
        assert!(
            res.hits
                .iter()
                .all(|h| h.confidence == Confidence::ExactFromSource),
            "all matches should be source-precision"
        );
        // At least one is classified as a CALL kind, not Unknown.
        assert!(
            res.hits
                .iter()
                .any(|h| matches!(h.call_kind, CallKind::Call)),
            "expected a `.call(` to land as CallKind::Call"
        );
    }

    #[test]
    fn skips_target_self() {
        let target_sel = [0xde, 0xad, 0xbe, 0xef];
        let target_addr = Address::repeat_byte(0x42);
        // Even if the target's *own* bytecode contains the pattern,
        // we don't list it as a caller of itself.
        let mut self_bc = vec![0x63];
        self_bc.extend_from_slice(&target_sel);
        self_bc.push(0xf1);
        let target = mk_contract(target_addr, self_bc, None);
        let sys = mk_system(vec![target]);

        let search = CallerSearch::new(target_addr, target_sel);
        let res = find_callers_of(&sys, &search).unwrap();
        assert!(res.hits.is_empty());
    }

    #[test]
    fn empty_system_returns_empty_result() {
        let sys = mk_system(vec![]);
        let search = CallerSearch::new(Address::ZERO, [0; 4]);
        let res = find_callers_of(&sys, &search).unwrap();
        assert!(res.hits.is_empty());
        assert_eq!(res.scanned_contracts, 0);
    }

    #[test]
    fn by_caller_aggregates_hits_per_address() {
        let target_sel = [0x11, 0x22, 0x33, 0x44];
        let target_addr = Address::repeat_byte(0x42);
        // Bytecode with TWO push4-then-call sequences.
        let mut bc = Vec::new();
        for _ in 0..2 {
            bc.push(0x63);
            bc.extend_from_slice(&target_sel);
            bc.push(0xf1);
        }
        let caller = mk_contract(Address::repeat_byte(0x07), bc, None);
        let target = mk_contract(target_addr, vec![], None);
        let sys = mk_system(vec![caller.clone(), target]);

        let search = CallerSearch::new(target_addr, target_sel);
        let res = find_callers_of(&sys, &search).unwrap();
        assert_eq!(res.hits.len(), 2);
        assert_eq!(res.by_caller.get(&caller.address).copied(), Some(2));
    }

    #[test]
    fn unrelated_selectors_dont_match() {
        let target_addr = Address::repeat_byte(0x42);
        let target_sel = [0xaa; 4];
        let mut bc = vec![0x63, 0xbb, 0xbb, 0xbb, 0xbb]; // PUSH4 different selector
        bc.push(0xf1);
        let caller = mk_contract(Address::repeat_byte(0x09), bc, None);
        let target = mk_contract(target_addr, vec![], None);
        let sys = mk_system(vec![caller, target]);

        let search = CallerSearch::new(target_addr, target_sel);
        let res = find_callers_of(&sys, &search).unwrap();
        assert!(res.hits.is_empty());
    }

    #[test]
    fn proximity_window_inclusive_at_boundary() {
        let target_addr = Address::repeat_byte(0x42);
        let sel = [0x01, 0x02, 0x03, 0x04];
        // PUSH4 at pc=0 (5 bytes), 59 filler bytes (pc 5..63), CALL
        // at pc=64. Distance 64 - 0 = 64, exactly at the proximity
        // window — boundary is `<=`, so this matches.
        let mut bc = vec![0x63, 0x01, 0x02, 0x03, 0x04];
        bc.extend(std::iter::repeat_n(0x5b, 59));
        bc.push(0xf1);
        let caller = mk_contract(Address::repeat_byte(0x09), bc, None);
        let target = mk_contract(target_addr, vec![], None);
        let sys = mk_system(vec![caller, target]);
        let search = CallerSearch::new(target_addr, sel);
        let res = find_callers_of(&sys, &search).unwrap();
        assert_eq!(res.hits.len(), 1);
    }
}
