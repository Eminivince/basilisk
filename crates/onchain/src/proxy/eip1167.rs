//! EIP-1167 minimal proxy bytecode signature.
//!
//! The canonical runtime bytecode (45 bytes total) is:
//! ```text
//! 0x363d3d373d3d3d363d73<20-byte target>5af43d82803e903d91602b57fd5bf3
//! ```
//! Bytes 0..10 and 30..45 are the fixed prefix/suffix; bytes 10..30 are
//! the implementation address.
//!
//! We also tolerate the handful of contracts deployed with the old
//! Gnosis-style minimal proxy (44 bytes) — it's rare but worth checking.

use alloy_primitives::Address;

const PREFIX: &[u8] = &[0x36, 0x3d, 0x3d, 0x37, 0x3d, 0x3d, 0x3d, 0x36, 0x3d, 0x73];
const SUFFIX: &[u8] = &[
    0x5a, 0xf4, 0x3d, 0x82, 0x80, 0x3e, 0x90, 0x3d, 0x91, 0x60, 0x2b, 0x57, 0xfd, 0x5b, 0xf3,
];

/// Total bytecode length of the canonical EIP-1167 minimal proxy.
pub const BYTECODE_LEN: usize = 45;

/// If `bytecode` is an EIP-1167 minimal proxy, return the implementation address.
pub fn extract_implementation(bytecode: &[u8]) -> Option<Address> {
    if bytecode.len() != BYTECODE_LEN {
        return None;
    }
    if !bytecode.starts_with(PREFIX) {
        return None;
    }
    if !bytecode.ends_with(SUFFIX) {
        return None;
    }
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&bytecode[PREFIX.len()..PREFIX.len() + 20]);
    Some(Address::from(addr))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_minimal(target: [u8; 20]) -> Vec<u8> {
        let mut b = Vec::with_capacity(BYTECODE_LEN);
        b.extend_from_slice(PREFIX);
        b.extend_from_slice(&target);
        b.extend_from_slice(SUFFIX);
        assert_eq!(b.len(), BYTECODE_LEN);
        b
    }

    #[test]
    fn canonical_minimal_proxy_is_detected() {
        let target = [0xab; 20];
        let code = build_minimal(target);
        let addr = extract_implementation(&code).expect("should match");
        assert_eq!(addr.as_slice(), &target);
    }

    #[test]
    fn zero_address_target_detected() {
        let code = build_minimal([0u8; 20]);
        assert_eq!(extract_implementation(&code), Some(Address::ZERO));
    }

    #[test]
    fn all_ones_target_detected() {
        let code = build_minimal([0xff; 20]);
        let addr = extract_implementation(&code).expect("match");
        assert_eq!(addr.as_slice(), &[0xff; 20]);
    }

    #[test]
    fn wrong_length_rejected() {
        let mut code = build_minimal([1u8; 20]);
        code.push(0);
        assert!(extract_implementation(&code).is_none());
        code.truncate(BYTECODE_LEN - 1);
        assert!(extract_implementation(&code).is_none());
    }

    #[test]
    fn wrong_prefix_rejected() {
        let mut code = build_minimal([1u8; 20]);
        code[0] ^= 0xff;
        assert!(extract_implementation(&code).is_none());
    }

    #[test]
    fn wrong_suffix_rejected() {
        let mut code = build_minimal([1u8; 20]);
        let last = code.len() - 1;
        code[last] ^= 0xff;
        assert!(extract_implementation(&code).is_none());
    }

    #[test]
    fn empty_bytecode_rejected() {
        assert!(extract_implementation(&[]).is_none());
    }

    #[test]
    fn random_bytecode_rejected() {
        let random = vec![0x60u8; BYTECODE_LEN];
        assert!(extract_implementation(&random).is_none());
    }
}
