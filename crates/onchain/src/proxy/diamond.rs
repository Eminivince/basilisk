//! EIP-2535 Diamond detection.
//!
//! Detection strategy:
//! 1. `eth_call` `facets()` — selector `0x7a0ed627`.
//! 2. Decode the return as `Facet[]` where `Facet { address; bytes4[] }`.
//! 3. If decoding succeeds and the list is non-empty, we call it a diamond.
//!
//! Pure bytecode heuristics aren't reliable for diamonds (the loupe
//! contract is upgradeable at runtime), so the `eth_call` is definitive.

use alloy_primitives::{Bytes, FixedBytes};
use alloy_sol_types::{sol, SolValue};

use super::types::DiamondFacet;

sol! {
    /// Matches the EIP-2535 `IDiamondLoupe.Facet` struct.
    struct Facet {
        address facetAddress;
        bytes4[] functionSelectors;
    }
}

/// 4-byte selector for `facets()` — `keccak256("facets()")[0..4]`.
pub const FACETS_SELECTOR: [u8; 4] = [0x7a, 0x0e, 0xd6, 0x27];

/// Returns the calldata for a zero-argument `facets()` call.
pub fn facets_calldata() -> Bytes {
    Bytes::from_static(&FACETS_SELECTOR)
}

/// Decode the return data of an EIP-2535 `facets()` call into our internal shape.
/// `None` if decoding fails or the list is empty.
pub fn decode_facets(return_data: &[u8]) -> Option<Vec<DiamondFacet>> {
    if return_data.is_empty() {
        return None;
    }
    let facets: Vec<Facet> = Vec::<Facet>::abi_decode(return_data, true).ok()?;
    if facets.is_empty() {
        return None;
    }
    Some(
        facets
            .into_iter()
            .map(|f| DiamondFacet {
                facet_address: f.facetAddress,
                selectors: f
                    .functionSelectors
                    .into_iter()
                    .map(|b: FixedBytes<4>| b.0)
                    .collect(),
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use alloy_sol_types::SolValue;

    use super::*;

    fn facet(addr_byte: u8, selectors: &[[u8; 4]]) -> Facet {
        let mut addr = [0u8; 20];
        addr[19] = addr_byte;
        Facet {
            facetAddress: Address::from(addr),
            functionSelectors: selectors.iter().map(|s| FixedBytes::<4>(*s)).collect(),
        }
    }

    #[test]
    fn facets_selector_matches_eip_value() {
        assert_eq!(FACETS_SELECTOR, [0x7a, 0x0e, 0xd6, 0x27]);
    }

    #[test]
    fn decodes_single_facet() {
        let f = facet(1, &[[0xaa; 4]]);
        let encoded = Vec::<Facet>::abi_encode(&vec![f]);
        let got = decode_facets(&encoded).expect("decode");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].selectors, vec![[0xaa; 4]]);
    }

    #[test]
    fn decodes_multiple_facets_with_multiple_selectors() {
        let a = facet(1, &[[1, 2, 3, 4], [5, 6, 7, 8]]);
        let b = facet(2, &[[9, 10, 11, 12]]);
        let encoded = Vec::<Facet>::abi_encode(&vec![a, b]);
        let got = decode_facets(&encoded).expect("decode");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].selectors.len(), 2);
        assert_eq!(got[1].selectors, vec![[9, 10, 11, 12]]);
    }

    #[test]
    fn empty_bytes_decodes_to_none() {
        assert!(decode_facets(&[]).is_none());
    }

    #[test]
    fn empty_array_decodes_to_none() {
        let encoded = Vec::<Facet>::abi_encode(&Vec::<Facet>::new());
        assert!(decode_facets(&encoded).is_none());
    }

    #[test]
    fn garbage_decodes_to_none() {
        assert!(decode_facets(&[0xde, 0xad, 0xbe, 0xef]).is_none());
    }

    #[test]
    fn calldata_is_just_the_selector() {
        let cd = facets_calldata();
        assert_eq!(cd.as_ref(), &[0x7a, 0x0e, 0xd6, 0x27]);
    }
}
