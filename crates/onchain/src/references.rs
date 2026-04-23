//! Three extractors that populate [`crate::AddressReference`] entries:
//!
//! - [`scan_storage_for_addresses`] reads the first `depth` storage slots
//!   via the RPC and surfaces any whose low-20 bytes correspond to a
//!   contract (`is_contract == true`).
//! - [`scan_bytecode_for_addresses`] (pure) walks the runtime bytecode
//!   looking for `PUSH20 <addr>` sequences and returns the raw
//!   candidates with their offsets.
//! - [`verify_bytecode_address_references`] takes those candidates and
//!   filters them through `is_contract`, returning a full
//!   `AddressReference` per surviving candidate.
//! - [`extract_immutable_addresses`] regex-parses verified source for
//!   `address constant`/`immutable` declarations. See the function doc
//!   for what's and isn't extractable.

use std::sync::OnceLock;

use alloy_primitives::{Address, B256};
use basilisk_explorers::VerifiedSource;
use basilisk_rpc::RpcProvider;
use regex::Regex;

use crate::{
    enrichment::{AddressReference, ReferenceSource},
    error::IngestError,
};

/// `PUSH20` opcode. Followed by 20 immediate bytes.
const PUSH20: u8 = 0x73;
/// Precompile addresses reserved by the EVM (0x01–0x09). We skip these
/// because they aren't user contracts and would clutter the graph.
const MAX_PRECOMPILE_BYTE: u8 = 0x09;

/// Read slots `0..depth`. For each non-zero slot whose low-20 bytes form
/// an address that looks like a contract, emit an [`AddressReference`].
///
/// Contract-ness is checked via [`RpcProvider::is_contract`] (which
/// reuses the bytecode cache), so repeated expansions of the same
/// addresses don't re-hit the RPC.
pub async fn scan_storage_for_addresses(
    rpc: &dyn RpcProvider,
    contract: Address,
    depth: usize,
) -> Result<Vec<AddressReference>, IngestError> {
    let mut out = Vec::new();
    for i in 0..depth {
        let slot = slot_from_index(i);
        let value = match rpc.get_storage_at(contract, slot).await {
            Ok(v) => v,
            Err(e) => return Err(IngestError::Rpc(e)),
        };
        if value == B256::ZERO {
            continue;
        }
        // Addresses are right-aligned in a 32-byte slot. The top 12 bytes
        // must all be zero for the value to plausibly be an address; if
        // they aren't, it's a larger integer or packed data.
        if !value.as_slice()[..12].iter().all(|b| *b == 0) {
            continue;
        }
        let mut raw = [0u8; 20];
        raw.copy_from_slice(&value.as_slice()[12..]);
        if is_precompile_or_zero(&raw) {
            continue;
        }
        let addr = Address::from(raw);
        match rpc.is_contract(addr).await {
            Ok(true) => {
                out.push(AddressReference {
                    address: addr,
                    source: ReferenceSource::Storage { slot },
                    context: format!("storage slot {slot:#x}"),
                });
            }
            Ok(false) => {}
            Err(e) => return Err(IngestError::Rpc(e)),
        }
    }
    Ok(out)
}

/// Pure bytecode scan: find every `PUSH20 <addr>` and return
/// `(address_bytes, instruction_offset)`. Skips precompiles + zero.
#[must_use]
pub fn scan_bytecode_for_addresses(bytecode: &[u8]) -> Vec<(Address, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytecode.len() {
        let op = bytecode[i];
        if op == PUSH20 && i + 1 + 20 <= bytecode.len() {
            let mut raw = [0u8; 20];
            raw.copy_from_slice(&bytecode[i + 1..i + 1 + 20]);
            if !is_precompile_or_zero(&raw) {
                out.push((Address::from(raw), i));
            }
            i += 21;
            continue;
        }
        // PUSH1..PUSH32 skip their N immediate bytes.
        if (0x60..=0x7f).contains(&op) {
            let n = (op - 0x5f) as usize;
            i += 1 + n;
            continue;
        }
        i += 1;
    }
    out
}

/// Filter a batch of [`scan_bytecode_for_addresses`] candidates through
/// `is_contract` and build the resulting [`AddressReference`] list. The
/// `candidates` and `offsets` vectors must have matching length.
pub async fn verify_bytecode_address_references(
    rpc: &dyn RpcProvider,
    candidates: Vec<(Address, usize)>,
) -> Result<Vec<AddressReference>, IngestError> {
    let mut out = Vec::with_capacity(candidates.len());
    for (addr, offset) in candidates {
        match rpc.is_contract(addr).await {
            Ok(true) => {
                out.push(AddressReference {
                    address: addr,
                    source: ReferenceSource::Bytecode { offset },
                    context: format!("bytecode offset {offset:#x}"),
                });
            }
            Ok(false) => {}
            Err(e) => return Err(IngestError::Rpc(e)),
        }
    }
    Ok(out)
}

/// Extract `address constant` and `address immutable` declarations from
/// verified source. Regex-based rather than a full AST walk:
///
/// - **Constants**: `address (public|private|internal)? constant NAME =
///   0x…;` — fully captured as a [`ReferenceSource::VerifiedConstant`].
/// - **Immutables**: the name is captured as a
///   [`ReferenceSource::Immutable`] with `address = Address::ZERO` as a
///   placeholder, because the runtime value lives in the contract's
///   storage/bytecode, not the source text. Callers should then cross-
///   reference against the storage / bytecode scanners to materialize
///   the actual address.
#[must_use]
pub fn extract_immutable_addresses(source: &VerifiedSource) -> Vec<AddressReference> {
    let mut out = Vec::new();
    for content in source.source_files.values() {
        for (name, addr) in find_constant_addresses(content) {
            out.push(AddressReference {
                address: addr,
                source: ReferenceSource::VerifiedConstant { name: name.clone() },
                context: format!("constant {name}"),
            });
        }
        for name in find_immutable_names(content) {
            out.push(AddressReference {
                address: Address::ZERO,
                source: ReferenceSource::Immutable { name: name.clone() },
                context: format!("immutable {name} (value not recoverable from source)"),
            });
        }
    }
    out
}

fn find_constant_addresses(source: &str) -> Vec<(String, Address)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    // address (public|private|internal|external)? constant NAME = 0x<40>;
    let re = RE.get_or_init(|| {
        Regex::new(
            r"(?x)
            address\s+
            (?:(?:public|private|internal|external)\s+)?
            constant\s+
            (?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*
            =\s*
            (?P<addr>0x[0-9a-fA-F]{40})
            ",
        )
        .expect("constant-address regex")
    });
    let mut out = Vec::new();
    for caps in re.captures_iter(source) {
        let name = caps.name("name").map(|m| m.as_str().to_string());
        let addr_str = caps.name("addr").map(|m| m.as_str());
        if let (Some(name), Some(a)) = (name, addr_str) {
            if let Ok(addr) = a.parse::<Address>() {
                out.push((name, addr));
            }
        }
    }
    out
}

fn find_immutable_names(source: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    // address (public|private|internal|external)? immutable NAME
    let re = RE.get_or_init(|| {
        Regex::new(
            r"(?x)
            address\s+
            (?:(?:public|private|internal|external)\s+)?
            immutable\s+
            (?P<name>[A-Za-z_][A-Za-z0-9_]*)
            ",
        )
        .expect("immutable-address regex")
    });
    re.captures_iter(source)
        .filter_map(|c| c.name("name").map(|m| m.as_str().to_string()))
        .collect()
}

fn slot_from_index(i: usize) -> B256 {
    let mut buf = [0u8; 32];
    let bytes = (i as u128).to_be_bytes();
    buf[16..].copy_from_slice(&bytes);
    B256::from(buf)
}

fn is_precompile_or_zero(raw: &[u8; 20]) -> bool {
    if raw == &[0u8; 20] {
        return true;
    }
    // Precompiles occupy 0x1..=0x9 with all upper bytes zero.
    if raw[..19].iter().all(|b| *b == 0) && raw[19] <= MAX_PRECOMPILE_BYTE {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use basilisk_core::Chain;
    use basilisk_rpc::MemoryProvider;

    use super::*;

    fn addr(byte: u8) -> Address {
        let mut a = [0u8; 20];
        a[19] = byte;
        Address::from(a)
    }

    fn stored_address(address: Address) -> B256 {
        let mut buf = [0u8; 32];
        buf[12..].copy_from_slice(address.as_slice());
        B256::from(buf)
    }

    #[tokio::test]
    async fn storage_scan_surfaces_contract_at_slot() {
        let target = addr(0x11);
        let rpc = MemoryProvider::new(Chain::EthereumMainnet)
            .with_code(target, alloy_primitives::Bytes::from_static(&[0xde, 0xad]))
            .with_slot(addr(1), slot_from_index(0), stored_address(target));
        let refs = scan_storage_for_addresses(&rpc, addr(1), 4).await.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].address, target);
        match refs[0].source {
            ReferenceSource::Storage { slot } => assert_eq!(slot, slot_from_index(0)),
            _ => panic!("wrong source"),
        }
    }

    #[tokio::test]
    async fn storage_scan_skips_eoa_addresses() {
        // The slot points at an address with no bytecode (EOA).
        let eoa = addr(0x22);
        let rpc = MemoryProvider::new(Chain::EthereumMainnet).with_slot(
            addr(1),
            slot_from_index(0),
            stored_address(eoa),
        );
        let refs = scan_storage_for_addresses(&rpc, addr(1), 4).await.unwrap();
        assert!(refs.is_empty());
    }

    #[tokio::test]
    async fn storage_scan_skips_non_address_high_bits() {
        // Pack a large integer into the slot — not an address.
        let mut value = [0xffu8; 32];
        value[12..].copy_from_slice(&[0u8; 20]);
        // Wait, that'd be zero bytes in the address region. Instead set a
        // real non-zero value with bits in the top 12 bytes.
        let mut big = [0u8; 32];
        big[10] = 0xaa;
        big[31] = 0x01;
        let rpc = MemoryProvider::new(Chain::EthereumMainnet).with_slot(
            addr(1),
            slot_from_index(0),
            B256::from(big),
        );
        let refs = scan_storage_for_addresses(&rpc, addr(1), 4).await.unwrap();
        assert!(refs.is_empty());
    }

    #[tokio::test]
    async fn storage_scan_skips_precompiles() {
        // 0x0000...0001 (ecrecover) isn't a user contract.
        let precompile = addr(0x01);
        let rpc = MemoryProvider::new(Chain::EthereumMainnet).with_slot(
            addr(1),
            slot_from_index(0),
            stored_address(precompile),
        );
        let refs = scan_storage_for_addresses(&rpc, addr(1), 4).await.unwrap();
        assert!(refs.is_empty());
    }

    #[test]
    fn bytecode_scan_finds_push20_addresses() {
        // PUSH20 <addr1>  STOP  PUSH1 <0x00>  PUSH20 <addr2>
        let addr1 = [0xaa; 20];
        let addr2 = [0xbb; 20];
        let mut code = vec![PUSH20];
        code.extend_from_slice(&addr1);
        code.push(0x00); // STOP
        code.push(0x60); // PUSH1
        code.push(0x00);
        code.push(PUSH20);
        code.extend_from_slice(&addr2);
        let got = scan_bytecode_for_addresses(&code);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0.as_slice(), &addr1);
        assert_eq!(got[0].1, 0);
        assert_eq!(got[1].0.as_slice(), &addr2);
        // Offset of second PUSH20: 1 (first PUSH20 op) + 20 (addr1) + 1 (STOP)
        //                         + 2 (PUSH1 + byte) = 24
        assert_eq!(got[1].1, 24);
    }

    #[test]
    fn bytecode_scan_skips_truncated_push20() {
        // PUSH20 with only 5 bytes after it → ignore (end-of-code).
        let mut code = vec![PUSH20];
        code.extend_from_slice(&[0xaa; 5]);
        assert!(scan_bytecode_for_addresses(&code).is_empty());
    }

    #[test]
    fn bytecode_scan_skips_precompiles_and_zero() {
        let mut code = vec![PUSH20];
        code.extend_from_slice(&[0u8; 20]); // zero addr
        code.push(PUSH20);
        let mut precompile = [0u8; 20];
        precompile[19] = 0x05; // precompile 5
        code.extend_from_slice(&precompile);
        assert!(scan_bytecode_for_addresses(&code).is_empty());
    }

    #[tokio::test]
    async fn bytecode_verification_filters_non_contracts() {
        let contract = addr(0x11);
        let eoa = addr(0x22);
        let candidates = vec![(contract, 0), (eoa, 42)];
        let rpc = MemoryProvider::new(Chain::EthereumMainnet)
            .with_code(contract, alloy_primitives::Bytes::from_static(&[0x00]));
        let refs = verify_bytecode_address_references(&rpc, candidates)
            .await
            .unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].address, contract);
        match refs[0].source {
            ReferenceSource::Bytecode { offset } => assert_eq!(offset, 0),
            _ => panic!("wrong source"),
        }
    }

    fn verified_source_with(files: &[(&str, &str)]) -> VerifiedSource {
        let mut map = BTreeMap::new();
        for (path, body) in files {
            map.insert(std::path::PathBuf::from(path), (*body).to_string());
        }
        VerifiedSource {
            source_files: map,
            contract_name: "Target".into(),
            compiler_version: "0.8.20".into(),
            optimizer: None,
            evm_version: None,
            abi: serde_json::Value::Array(vec![]),
            constructor_args: None,
            license: None,
            proxy_hint: None,
            implementation_hint: None,
            metadata: serde_json::Value::Null,
        }
    }

    #[test]
    fn extracts_public_address_constant() {
        let src = verified_source_with(&[(
            "X.sol",
            "address public constant WETH = 0x1111111111111111111111111111111111111111;",
        )]);
        let refs = extract_immutable_addresses(&src);
        let expected: Address = "0x1111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        let hit = refs
            .iter()
            .find(|r| r.address == expected)
            .expect("constant not found");
        assert!(
            matches!(&hit.source, ReferenceSource::VerifiedConstant { name } if name == "WETH")
        );
    }

    #[test]
    fn extracts_private_constant_without_visibility_keyword() {
        let src = verified_source_with(&[(
            "X.sol",
            "address constant FOO = 0xaAaAaAaaAaAaAaaAaAAAAAAAAaaaAaAaAaaAaaAa;",
        )]);
        let refs = extract_immutable_addresses(&src);
        assert_eq!(refs.len(), 1);
        assert!(
            matches!(&refs[0].source, ReferenceSource::VerifiedConstant { name } if name == "FOO")
        );
    }

    #[test]
    fn extracts_immutable_name_without_value() {
        let src =
            verified_source_with(&[("X.sol", "contract X { address public immutable ORACLE; }")]);
        let refs = extract_immutable_addresses(&src);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].address, Address::ZERO);
        assert!(matches!(&refs[0].source, ReferenceSource::Immutable { name } if name == "ORACLE"));
    }

    #[test]
    fn extracts_multiple_declarations_across_files() {
        let src = verified_source_with(&[
            (
                "A.sol",
                "address constant A_ONE = 0x1111111111111111111111111111111111111111;",
            ),
            (
                "B.sol",
                "contract B {\n address immutable B_IMMUT;\n address internal constant B_TWO = 0x2222222222222222222222222222222222222222;\n}",
            ),
        ]);
        let refs = extract_immutable_addresses(&src);
        // 2 constants + 1 immutable.
        assert_eq!(refs.len(), 3);
    }

    #[test]
    fn ignores_non_address_declarations() {
        let src = verified_source_with(&[(
            "X.sol",
            "uint256 constant Z = 1; address blah; // no constant keyword",
        )]);
        let refs = extract_immutable_addresses(&src);
        assert!(refs.is_empty());
    }
}
