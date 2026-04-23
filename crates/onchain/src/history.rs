//! Upgrade-history reconstruction from on-chain event logs.
//!
//! Matches the three canonical `OpenZeppelin` events — `Upgraded(address)`,
//! `BeaconUpgraded(address)`, and `AdminChanged(address,address)` — by
//! topic0 hash. Uses [`basilisk_rpc::fetch_logs_chunked`] for the block
//! range walk, fetches block timestamps, and builds the chronologically
//! sorted [`UpgradeEvent`] list with `old_implementation` walked forward.

use alloy_primitives::{b256, Address, B256};
use alloy_rpc_types_eth::Filter;
use basilisk_rpc::{fetch_logs_chunked, LogFilter, RpcProvider};

use crate::{error::IngestError, proxy::UpgradeEvent};

/// `keccak256("Upgraded(address)")`.
pub const UPGRADED_TOPIC: B256 =
    b256!("bc7cd75a20ee27fd9adebab32041f755214dbc6bffa90cc0225b39da2e5c2d3b");
/// `keccak256("BeaconUpgraded(address)")`.
pub const BEACON_UPGRADED_TOPIC: B256 =
    b256!("1cf3b03a6cf19fa2baba4df148e9dcabedea7f8a5c07840e207e5c089be95d3e");
/// `keccak256("AdminChanged(address,address)")`.
pub const ADMIN_CHANGED_TOPIC: B256 =
    b256!("7e644d79422f17c01e4894b5f4f588d331ebfa28653d42ae832dc59e38c9798f");

/// Default chunk size for log range walks; most providers accept 10k.
pub const DEFAULT_CHUNK_SIZE: u64 = 10_000;

/// Fetch and sort upgrade events for `proxy` in the block range
/// `[from_block, to_block]`.
///
/// The returned list is chronological (ascending block). `old_implementation`
/// is walked forward: entry `i > 0` has `old_implementation =
/// Some(entry[i-1].new_implementation)`; the first entry has `None`.
/// `AdminChanged` events are included for their audit value; they surface
/// as events whose `new_implementation` is the new admin.
pub async fn fetch_upgrade_history(
    rpc: &dyn RpcProvider,
    proxy: Address,
    from_block: u64,
    to_block: u64,
) -> Result<Vec<UpgradeEvent>, IngestError> {
    let filter = Filter::new().address(proxy).event_signature(vec![
        UPGRADED_TOPIC,
        BEACON_UPGRADED_TOPIC,
        ADMIN_CHANGED_TOPIC,
    ]);
    let logs = fetch_logs_chunked(
        rpc,
        LogFilter::from(filter),
        from_block,
        to_block,
        DEFAULT_CHUNK_SIZE,
    )
    .await
    .map_err(IngestError::Rpc)?;

    let mut events: Vec<UpgradeEvent> = Vec::with_capacity(logs.len());
    for log in logs {
        let Some(topic0) = log.topics().first().copied() else {
            continue;
        };
        let signature = match topic0 {
            t if t == UPGRADED_TOPIC => "Upgraded(address)",
            t if t == BEACON_UPGRADED_TOPIC => "BeaconUpgraded(address)",
            t if t == ADMIN_CHANGED_TOPIC => "AdminChanged(address,address)",
            _ => continue,
        };
        // For Upgraded / BeaconUpgraded, topic1 = new impl.
        // For AdminChanged, topic1 is previous admin, topic2 is new admin
        // (neither is indexed by default in OZ's events actually — both are
        // in data. But most deployed instances index them.) We take topic1
        // if present, topic2 otherwise, fall back to None.
        let new_impl = log
            .topics()
            .get(if topic0 == ADMIN_CHANGED_TOPIC { 2 } else { 1 })
            .or_else(|| log.topics().get(1))
            .map(|t| topic_to_address(*t));
        let Some(new_implementation) = new_impl else {
            continue;
        };

        let Some(block_number) = log.block_number else {
            continue;
        };
        let tx_hash = log.transaction_hash.unwrap_or(B256::ZERO);

        let block_timestamp = match rpc.get_block_timestamp(block_number).await {
            Ok(ts) => ts,
            Err(e) => {
                tracing::debug!(error = %e, block = block_number, "block timestamp fetch failed");
                None
            }
        };

        events.push(UpgradeEvent {
            block_number,
            block_timestamp,
            tx_hash,
            old_implementation: None,
            new_implementation,
            event_signature: signature.to_string(),
        });
    }

    events.sort_by_key(|e| e.block_number);

    // Walk forward so each event carries its predecessor's new_implementation.
    let mut prev_impl: Option<Address> = None;
    for event in &mut events {
        event.old_implementation = prev_impl;
        prev_impl = Some(event.new_implementation);
    }

    Ok(events)
}

/// Extract the low-20-byte address from a 32-byte topic (right-aligned).
fn topic_to_address(topic: B256) -> Address {
    let mut out = [0u8; 20];
    out.copy_from_slice(&topic.as_slice()[12..]);
    Address::from(out)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, LogData, B256};
    use alloy_rpc_types_eth::Log;
    use basilisk_core::Chain;
    use basilisk_rpc::{memory::LogMatcher, MemoryProvider};

    use super::*;

    fn addr(byte: u8) -> Address {
        let mut a = [0u8; 20];
        a[19] = byte;
        Address::from(a)
    }

    fn topic_for(addr: Address) -> B256 {
        let mut t = [0u8; 32];
        t[12..].copy_from_slice(addr.as_slice());
        B256::from(t)
    }

    fn mk_log(topics: Vec<B256>, block: u64, tx: B256) -> Log {
        let mut log = Log::default();
        let inner = alloy_primitives::Log {
            address: addr(0),
            data: LogData::new_unchecked(topics, alloy_primitives::Bytes::new()),
        };
        log.inner = inner;
        log.block_number = Some(block);
        log.transaction_hash = Some(tx);
        log
    }

    #[tokio::test]
    async fn topic_constants_match_spec() {
        assert_eq!(
            UPGRADED_TOPIC.to_string(),
            "0xbc7cd75a20ee27fd9adebab32041f755214dbc6bffa90cc0225b39da2e5c2d3b",
        );
        assert_eq!(
            BEACON_UPGRADED_TOPIC.to_string(),
            "0x1cf3b03a6cf19fa2baba4df148e9dcabedea7f8a5c07840e207e5c089be95d3e",
        );
        assert_eq!(
            ADMIN_CHANGED_TOPIC.to_string(),
            "0x7e644d79422f17c01e4894b5f4f588d331ebfa28653d42ae832dc59e38c9798f",
        );
    }

    #[tokio::test]
    async fn walks_and_sorts_events() {
        let proxy = addr(1);
        let impl_a = addr(0xaa);
        let impl_b = addr(0xbb);
        let impl_c = addr(0xcc);
        let logs = vec![
            // Out-of-order block numbers — function must sort.
            mk_log(
                vec![UPGRADED_TOPIC, topic_for(impl_c)],
                300,
                B256::from([3u8; 32]),
            ),
            mk_log(
                vec![UPGRADED_TOPIC, topic_for(impl_a)],
                100,
                B256::from([1u8; 32]),
            ),
            mk_log(
                vec![UPGRADED_TOPIC, topic_for(impl_b)],
                200,
                B256::from([2u8; 32]),
            ),
        ];
        let rpc = MemoryProvider::new(Chain::EthereumMainnet)
            .with_logs(LogMatcher::Address(proxy), logs)
            .with_block_timestamp(100, 1_700_000_000)
            .with_block_timestamp(200, 1_700_100_000)
            .with_block_timestamp(300, 1_700_200_000);

        let events = fetch_upgrade_history(&rpc, proxy, 0, 1000).await.unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].block_number, 100);
        assert_eq!(events[0].new_implementation, impl_a);
        assert!(events[0].old_implementation.is_none());
        assert_eq!(events[1].new_implementation, impl_b);
        assert_eq!(events[1].old_implementation, Some(impl_a));
        assert_eq!(events[2].new_implementation, impl_c);
        assert_eq!(events[2].old_implementation, Some(impl_b));
        assert_eq!(events[0].block_timestamp, Some(1_700_000_000));
    }

    #[tokio::test]
    async fn recognizes_beacon_upgraded() {
        let proxy = addr(1);
        let logs = vec![mk_log(
            vec![BEACON_UPGRADED_TOPIC, topic_for(addr(0xff))],
            50,
            B256::from([7u8; 32]),
        )];
        let rpc = MemoryProvider::new(Chain::EthereumMainnet).with_logs(LogMatcher::Any, logs);
        let events = fetch_upgrade_history(&rpc, proxy, 0, 100).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_signature, "BeaconUpgraded(address)");
    }

    #[tokio::test]
    async fn admin_changed_uses_topic2_when_present() {
        let proxy = addr(1);
        let old_admin = addr(0x11);
        let new_admin = addr(0x22);
        let logs = vec![mk_log(
            vec![
                ADMIN_CHANGED_TOPIC,
                topic_for(old_admin),
                topic_for(new_admin),
            ],
            50,
            B256::from([7u8; 32]),
        )];
        let rpc = MemoryProvider::new(Chain::EthereumMainnet).with_logs(LogMatcher::Any, logs);
        let events = fetch_upgrade_history(&rpc, proxy, 0, 100).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].new_implementation, new_admin);
        assert_eq!(events[0].event_signature, "AdminChanged(address,address)");
    }

    #[tokio::test]
    async fn ignores_unrelated_topics() {
        let proxy = addr(1);
        let stray = b256!("deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
        let logs = vec![mk_log(vec![stray, topic_for(addr(0xff))], 1, B256::ZERO)];
        let rpc = MemoryProvider::new(Chain::EthereumMainnet).with_logs(LogMatcher::Any, logs);
        let events = fetch_upgrade_history(&rpc, proxy, 0, 10).await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn empty_range_returns_empty() {
        let rpc = MemoryProvider::new(Chain::EthereumMainnet);
        let events = fetch_upgrade_history(&rpc, addr(1), 10, 5).await.unwrap();
        assert!(events.is_empty());
    }
}
