//! Canonical EIP-1967 storage slots.
//!
//! Each constant is the `keccak256(label) - 1` value mandated by the EIP.
//! We assert equality against the exact hex in [`tests`] so this module
//! can never drift from the specification.

use alloy_primitives::{b256, B256};

/// `keccak256("eip1967.proxy.implementation") - 1`.
pub const IMPLEMENTATION_SLOT: B256 =
    b256!("360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc");

/// `keccak256("eip1967.proxy.admin") - 1`.
pub const ADMIN_SLOT: B256 =
    b256!("b53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103");

/// `keccak256("eip1967.proxy.beacon") - 1`.
pub const BEACON_SLOT: B256 =
    b256!("a3f0ad74e5423aebfd80d3ef4346578335a9a72aeaee59ff6cb3582b35133d50");

/// `keccak256("eip1967.proxy.rollback") - 1`. Present on `OpenZeppelin`
/// UUPS proxies during an upgrade; we read it but it isn't definitive.
pub const ROLLBACK_SLOT: B256 =
    b256!("4910fdfa16fed3260ed0e7147f7cc6da11a60208b5b9406d12a635614ffd9143");

/// Extract the low-20-byte address from a 32-byte slot value. EIP-1967
/// slots always right-align the address, so we grab the final 20 bytes.
pub fn address_from_slot(value: B256) -> alloy_primitives::Address {
    let mut out = [0u8; 20];
    out.copy_from_slice(&value.as_slice()[12..]);
    alloy_primitives::Address::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // EIP-1967 spec values. Asserted here so a typo in the constants above
    // cannot silently mis-identify proxies.
    #[test]
    fn implementation_slot_matches_spec() {
        let expected = "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc";
        assert_eq!(IMPLEMENTATION_SLOT.to_string(), expected);
    }

    #[test]
    fn admin_slot_matches_spec() {
        let expected = "0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103";
        assert_eq!(ADMIN_SLOT.to_string(), expected);
    }

    #[test]
    fn beacon_slot_matches_spec() {
        let expected = "0xa3f0ad74e5423aebfd80d3ef4346578335a9a72aeaee59ff6cb3582b35133d50";
        assert_eq!(BEACON_SLOT.to_string(), expected);
    }

    #[test]
    fn rollback_slot_matches_spec() {
        let expected = "0x4910fdfa16fed3260ed0e7147f7cc6da11a60208b5b9406d12a635614ffd9143";
        assert_eq!(ROLLBACK_SLOT.to_string(), expected);
    }

    #[test]
    fn address_from_slot_takes_low_20_bytes() {
        // 32-byte slot padded with zeros above a 20-byte address.
        let slot = b256!("000000000000000000000000abcdef1234567890abcdef1234567890abcdef12");
        let addr = address_from_slot(slot);
        assert_eq!(
            addr.to_string().to_lowercase(),
            "0xabcdef1234567890abcdef1234567890abcdef12"
        );
    }

    #[test]
    fn address_from_slot_zero_yields_zero_address() {
        assert_eq!(
            address_from_slot(B256::ZERO),
            alloy_primitives::Address::ZERO
        );
    }
}
