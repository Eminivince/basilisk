//! Proxy detection.
//!
//! [`detect_proxy`] runs the three detectors and assembles a
//! [`ProxyInfo`] when any signal fires. Returns `None` for a non-proxy.
//!
//! Detector order:
//! 1. EIP-1167 bytecode signature (pure; no RPC).
//! 2. EIP-1967 slot reads (impl / admin / beacon / rollback).
//! 3. EIP-2535 diamond via `facets()` static call.
//!
//! A contract can plausibly match more than one (e.g. an older proxy
//! that uses both EIP-1967 slots and a diamond loupe). We report the
//! *most specific* kind we saw; every signal still appears in
//! [`ProxyInfo::detection_evidence`] for audit.

pub mod diamond;
pub mod eip1167;
pub mod slots;
pub mod types;

use alloy_primitives::{Address, Bytes, B256};
use basilisk_rpc::RpcProvider;

pub use types::{DiamondFacet, ProxyEvidence, ProxyInfo, ProxyKind};

/// Inspect a contract for proxy patterns.
///
/// `bytecode` is the deployed runtime bytecode at `address`. `rpc` is used
/// for storage-slot reads and the diamond `facets()` call.
#[allow(clippy::too_many_lines)]
pub async fn detect_proxy(
    bytecode: &Bytes,
    address: Address,
    rpc: &dyn RpcProvider,
) -> Option<ProxyInfo> {
    // 1. EIP-1167 — pure, cheapest.
    if let Some(target) = eip1167::extract_implementation(bytecode.as_ref()) {
        return Some(ProxyInfo {
            kind: ProxyKind::Eip1167Minimal,
            implementation_address: Some(target),
            admin_address: None,
            beacon_address: None,
            facets: Vec::new(),
            detection_evidence: vec![ProxyEvidence::new(
                "EIP-1167 minimal proxy bytecode signature",
                format!("0x{}", hex_lower(bytecode.as_ref())),
            )],
        });
    }

    // 2. EIP-1967 slot reads.
    let impl_slot = read_slot(rpc, address, slots::IMPLEMENTATION_SLOT).await;
    let admin_slot = read_slot(rpc, address, slots::ADMIN_SLOT).await;
    let beacon_slot = read_slot(rpc, address, slots::BEACON_SLOT).await;
    let rollback_slot = read_slot(rpc, address, slots::ROLLBACK_SLOT).await;

    let impl_addr = impl_slot
        .filter(|v| *v != B256::ZERO)
        .map(slots::address_from_slot);
    let admin_addr = admin_slot
        .filter(|v| *v != B256::ZERO)
        .map(slots::address_from_slot);
    let beacon_addr = beacon_slot
        .filter(|v| *v != B256::ZERO)
        .map(slots::address_from_slot);

    let mut evidence = Vec::new();
    if let Some(addr) = impl_addr {
        evidence.push(ProxyEvidence::new(
            "EIP-1967 implementation slot",
            addr.to_string(),
        ));
    }
    if let Some(addr) = admin_addr {
        evidence.push(ProxyEvidence::new("EIP-1967 admin slot", addr.to_string()));
    }
    if let Some(addr) = beacon_addr {
        evidence.push(ProxyEvidence::new("EIP-1967 beacon slot", addr.to_string()));
    }
    if rollback_slot.is_some_and(|v| v != B256::ZERO) {
        evidence.push(ProxyEvidence::new(
            "EIP-1967 rollback slot populated",
            format!("0x{}", hex_lower(rollback_slot.unwrap().as_slice())),
        ));
    }

    if beacon_addr.is_some() && impl_addr.is_none() {
        return Some(ProxyInfo {
            kind: ProxyKind::Eip1967Beacon,
            implementation_address: None,
            admin_address: admin_addr,
            beacon_address: beacon_addr,
            facets: Vec::new(),
            detection_evidence: evidence,
        });
    }
    if let Some(impl_addr) = impl_addr {
        let kind = if admin_addr.is_some() {
            ProxyKind::Eip1967Transparent
        } else {
            ProxyKind::Eip1967Uups
        };
        return Some(ProxyInfo {
            kind,
            implementation_address: Some(impl_addr),
            admin_address: admin_addr,
            beacon_address: beacon_addr,
            facets: Vec::new(),
            detection_evidence: evidence,
        });
    }

    // 3. EIP-2535 diamond: call facets() and decode.
    let call_data = diamond::facets_calldata();
    match rpc.call(address, call_data).await {
        Ok(ret) if !ret.is_empty() => {
            if let Some(facets) = diamond::decode_facets(ret.as_ref()) {
                evidence.push(ProxyEvidence::new(
                    "EIP-2535 facets() returned list",
                    format!("{} facets", facets.len()),
                ));
                return Some(ProxyInfo {
                    kind: ProxyKind::Eip2535Diamond,
                    implementation_address: None,
                    admin_address: None,
                    beacon_address: None,
                    facets,
                    detection_evidence: evidence,
                });
            }
        }
        Ok(_) => {}
        Err(e) => {
            tracing::debug!(error = %e, "diamond facets() call failed; skipping");
        }
    }

    // Unknown-proxy bucket: some storage signal fired but nothing classical matched.
    if !evidence.is_empty() {
        return Some(ProxyInfo {
            kind: ProxyKind::UnknownProxyPattern,
            implementation_address: impl_addr,
            admin_address: admin_addr,
            beacon_address: beacon_addr,
            facets: Vec::new(),
            detection_evidence: evidence,
        });
    }

    None
}

async fn read_slot(rpc: &dyn RpcProvider, address: Address, slot: B256) -> Option<B256> {
    match rpc.get_storage_at(address, slot).await {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(error = %e, slot = %slot, "storage read failed");
            None
        }
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bytes, B256};
    use alloy_sol_types::SolValue;
    use basilisk_core::Chain;
    use basilisk_rpc::MemoryProvider;

    use super::{diamond::Facet, *};

    fn contract() -> Address {
        let mut a = [0u8; 20];
        a[19] = 0xaa;
        Address::from(a)
    }

    fn impl_addr() -> Address {
        let mut a = [0u8; 20];
        a[19] = 0x11;
        Address::from(a)
    }

    fn admin_addr() -> Address {
        let mut a = [0u8; 20];
        a[19] = 0x22;
        Address::from(a)
    }

    fn beacon_addr() -> Address {
        let mut a = [0u8; 20];
        a[19] = 0x33;
        Address::from(a)
    }

    fn slot_for(addr: Address) -> B256 {
        let mut buf = [0u8; 32];
        buf[12..].copy_from_slice(addr.as_slice());
        B256::from(buf)
    }

    #[tokio::test]
    async fn detect_none_on_plain_non_proxy() {
        let code = Bytes::from(vec![0x60u8, 0x80, 0x60, 0x40]); // random runtime bytecode
        let rpc = MemoryProvider::new(Chain::EthereumMainnet);
        let got = detect_proxy(&code, contract(), &rpc).await;
        assert!(got.is_none(), "got {got:?}");
    }

    #[tokio::test]
    async fn detect_1167_from_bytecode() {
        // Build canonical minimal proxy bytecode pointing at impl_addr.
        let mut code = vec![0x36, 0x3d, 0x3d, 0x37, 0x3d, 0x3d, 0x3d, 0x36, 0x3d, 0x73];
        code.extend_from_slice(impl_addr().as_slice());
        code.extend_from_slice(&[
            0x5a, 0xf4, 0x3d, 0x82, 0x80, 0x3e, 0x90, 0x3d, 0x91, 0x60, 0x2b, 0x57, 0xfd, 0x5b,
            0xf3,
        ]);
        let rpc = MemoryProvider::new(Chain::EthereumMainnet);
        let info = detect_proxy(&Bytes::from(code), contract(), &rpc)
            .await
            .unwrap();
        assert_eq!(info.kind, ProxyKind::Eip1167Minimal);
        assert_eq!(info.implementation_address, Some(impl_addr()));
    }

    #[tokio::test]
    async fn detect_1967_transparent() {
        let code = Bytes::from(vec![0x60u8, 0x80]);
        let rpc = MemoryProvider::new(Chain::EthereumMainnet)
            .with_slot(
                contract(),
                slots::IMPLEMENTATION_SLOT,
                slot_for(impl_addr()),
            )
            .with_slot(contract(), slots::ADMIN_SLOT, slot_for(admin_addr()));
        let info = detect_proxy(&code, contract(), &rpc).await.unwrap();
        assert_eq!(info.kind, ProxyKind::Eip1967Transparent);
        assert_eq!(info.implementation_address, Some(impl_addr()));
        assert_eq!(info.admin_address, Some(admin_addr()));
    }

    #[tokio::test]
    async fn detect_1967_uups_when_no_admin_slot() {
        let code = Bytes::from(vec![0x60u8, 0x80]);
        let rpc = MemoryProvider::new(Chain::EthereumMainnet).with_slot(
            contract(),
            slots::IMPLEMENTATION_SLOT,
            slot_for(impl_addr()),
        );
        let info = detect_proxy(&code, contract(), &rpc).await.unwrap();
        assert_eq!(info.kind, ProxyKind::Eip1967Uups);
        assert_eq!(info.implementation_address, Some(impl_addr()));
        assert!(info.admin_address.is_none());
    }

    #[tokio::test]
    async fn detect_1967_beacon_when_only_beacon_slot_populated() {
        let code = Bytes::from(vec![0x60u8, 0x80]);
        let rpc = MemoryProvider::new(Chain::EthereumMainnet).with_slot(
            contract(),
            slots::BEACON_SLOT,
            slot_for(beacon_addr()),
        );
        let info = detect_proxy(&code, contract(), &rpc).await.unwrap();
        assert_eq!(info.kind, ProxyKind::Eip1967Beacon);
        assert_eq!(info.beacon_address, Some(beacon_addr()));
        assert!(info.implementation_address.is_none());
    }

    #[tokio::test]
    async fn beacon_plus_impl_slot_prefers_transparent_uups_classification() {
        // Rare but possible: both populated. Impl+admin → transparent.
        let code = Bytes::from(vec![0x60u8, 0x80]);
        let rpc = MemoryProvider::new(Chain::EthereumMainnet)
            .with_slot(
                contract(),
                slots::IMPLEMENTATION_SLOT,
                slot_for(impl_addr()),
            )
            .with_slot(contract(), slots::ADMIN_SLOT, slot_for(admin_addr()))
            .with_slot(contract(), slots::BEACON_SLOT, slot_for(beacon_addr()));
        let info = detect_proxy(&code, contract(), &rpc).await.unwrap();
        assert_eq!(info.kind, ProxyKind::Eip1967Transparent);
        assert_eq!(info.beacon_address, Some(beacon_addr()));
    }

    #[tokio::test]
    async fn detect_2535_diamond_via_facets_call() {
        let code = Bytes::from(vec![0x60u8, 0x80]);
        // Construct encoded facets() response: one facet with two selectors.
        let mut addr_bytes = [0u8; 20];
        addr_bytes[19] = 0x77;
        let facet = Facet {
            facetAddress: Address::from(addr_bytes),
            functionSelectors: vec![
                alloy_primitives::FixedBytes::<4>([0x01, 0x02, 0x03, 0x04]),
                alloy_primitives::FixedBytes::<4>([0x05, 0x06, 0x07, 0x08]),
            ],
        };
        let encoded = Vec::<Facet>::abi_encode(&vec![facet]);

        let rpc = MemoryProvider::new(Chain::EthereumMainnet).with_call_response(
            contract(),
            diamond::facets_calldata(),
            Bytes::from(encoded),
        );
        let info = detect_proxy(&code, contract(), &rpc).await.unwrap();
        assert_eq!(info.kind, ProxyKind::Eip2535Diamond);
        assert_eq!(info.facets.len(), 1);
        assert_eq!(info.facets[0].selectors.len(), 2);
    }

    #[tokio::test]
    async fn diamond_with_empty_facets_array_is_not_a_diamond() {
        let code = Bytes::from(vec![0x60u8, 0x80]);
        let encoded = Vec::<Facet>::abi_encode(&Vec::<Facet>::new());
        let rpc = MemoryProvider::new(Chain::EthereumMainnet).with_call_response(
            contract(),
            diamond::facets_calldata(),
            Bytes::from(encoded),
        );
        assert!(detect_proxy(&code, contract(), &rpc).await.is_none());
    }

    #[tokio::test]
    async fn storage_signal_without_impl_or_beacon_is_unknown() {
        // Only admin slot populated — weird but possible. Should surface as unknown.
        let code = Bytes::from(vec![0x60u8, 0x80]);
        let rpc = MemoryProvider::new(Chain::EthereumMainnet).with_slot(
            contract(),
            slots::ADMIN_SLOT,
            slot_for(admin_addr()),
        );
        let info = detect_proxy(&code, contract(), &rpc).await.unwrap();
        assert_eq!(info.kind, ProxyKind::UnknownProxyPattern);
        assert_eq!(info.admin_address, Some(admin_addr()));
    }

    #[tokio::test]
    async fn evidence_is_populated_for_each_signal() {
        let code = Bytes::from(vec![0x60u8, 0x80]);
        let rpc = MemoryProvider::new(Chain::EthereumMainnet)
            .with_slot(
                contract(),
                slots::IMPLEMENTATION_SLOT,
                slot_for(impl_addr()),
            )
            .with_slot(contract(), slots::ADMIN_SLOT, slot_for(admin_addr()));
        let info = detect_proxy(&code, contract(), &rpc).await.unwrap();
        assert!(info
            .detection_evidence
            .iter()
            .any(|e| e.signal.contains("implementation slot")));
        assert!(info
            .detection_evidence
            .iter()
            .any(|e| e.signal.contains("admin slot")));
    }

    #[tokio::test]
    async fn diamond_preferred_over_nothing_but_not_over_1967() {
        // Both a diamond response AND 1967 impl slot: 1967 check runs first and wins.
        let code = Bytes::from(vec![0x60u8, 0x80]);
        let encoded = Vec::<Facet>::abi_encode(&vec![Facet {
            facetAddress: Address::ZERO,
            functionSelectors: vec![alloy_primitives::FixedBytes::<4>([0u8; 4])],
        }]);
        let rpc = MemoryProvider::new(Chain::EthereumMainnet)
            .with_slot(
                contract(),
                slots::IMPLEMENTATION_SLOT,
                slot_for(impl_addr()),
            )
            .with_call_response(contract(), diamond::facets_calldata(), Bytes::from(encoded));
        let info = detect_proxy(&code, contract(), &rpc).await.unwrap();
        assert_eq!(info.kind, ProxyKind::Eip1967Uups);
    }
}
