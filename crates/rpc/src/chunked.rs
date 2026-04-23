//! Chunked log fetch.
//!
//! Most RPC providers cap a single `eth_getLogs` response at 10k entries
//! and/or a narrow block range. [`fetch_logs_chunked`] splits a large
//! range into chunks of `chunk_size` blocks and concatenates the results.

use alloy_rpc_types_eth::{BlockNumberOrTag, Filter as AlloyFilter};

use crate::{
    error::RpcError,
    provider::{LogFilter, RpcProvider},
    types::RpcLog,
};

/// Default block-range chunk size used by [`fetch_logs_chunked`].
pub const DEFAULT_CHUNK_SIZE: u64 = 10_000;

/// Split a `[from_block, to_block]` range into chunks of `chunk_size` blocks
/// and concatenate the resulting log lists in order.
///
/// `base_filter` provides the address / topic constraints. Its existing
/// `from_block` / `to_block` are overridden per chunk.
pub async fn fetch_logs_chunked(
    rpc: &dyn RpcProvider,
    base_filter: LogFilter,
    from_block: u64,
    to_block: u64,
    chunk_size: u64,
) -> Result<Vec<RpcLog>, RpcError> {
    if from_block > to_block {
        return Ok(Vec::new());
    }
    let chunk = chunk_size.max(1);
    let mut out = Vec::new();

    let alloy: AlloyFilter = base_filter.inner().clone();
    let mut start = from_block;
    while start <= to_block {
        let end = (start + chunk - 1).min(to_block);
        let filter: LogFilter = alloy
            .clone()
            .from_block(BlockNumberOrTag::Number(start))
            .to_block(BlockNumberOrTag::Number(end))
            .into();
        let logs = rpc.get_logs(filter).await?;
        out.extend(logs);
        if end == u64::MAX {
            break;
        }
        start = end + 1;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bytes, B256};
    use alloy_rpc_types_eth::{Filter, Log};
    use basilisk_core::Chain;

    use super::*;
    use crate::memory::{LogMatcher, MemoryProvider};

    fn log_with_block(block: u64) -> RpcLog {
        Log { block_number: Some(block), ..Log::default() }
    }

    #[tokio::test]
    async fn empty_range_returns_empty() {
        let rpc = MemoryProvider::new(Chain::EthereumMainnet);
        let out = fetch_logs_chunked(&rpc, LogFilter::new(), 100, 50, 10)
            .await
            .unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn chunks_cover_full_range() {
        let logs = vec![log_with_block(1), log_with_block(2)];
        let rpc =
            MemoryProvider::new(Chain::EthereumMainnet).with_logs(LogMatcher::Any, logs.clone());
        // Chunk size 1 over a range of 5: 5 chunks, each returning the same logs.
        let out = fetch_logs_chunked(&rpc, LogFilter::new(), 0, 4, 1)
            .await
            .unwrap();
        assert_eq!(out.len(), 5 * logs.len());
    }

    #[tokio::test]
    async fn respects_chunk_size_zero_as_one() {
        let logs = vec![log_with_block(1)];
        let rpc = MemoryProvider::new(Chain::EthereumMainnet).with_logs(LogMatcher::Any, logs);
        // chunk_size=0 gets promoted to 1 internally.
        let out = fetch_logs_chunked(&rpc, LogFilter::new(), 0, 2, 0)
            .await
            .unwrap();
        assert_eq!(out.len(), 3);
    }

    #[tokio::test]
    async fn address_filter_limits_matches() {
        let addr = Address::from([1u8; 20]);
        let logs = vec![log_with_block(1)];
        let rpc =
            MemoryProvider::new(Chain::EthereumMainnet).with_logs(LogMatcher::Address(addr), logs);
        let filter: LogFilter = Filter::new().address(addr).into();
        let out = fetch_logs_chunked(&rpc, filter, 0, 9, 10).await.unwrap();
        assert_eq!(out.len(), 1);

        let other = Address::from([2u8; 20]);
        let filter_other: LogFilter = Filter::new().address(other).into();
        let out_none = fetch_logs_chunked(&rpc, filter_other, 0, 9, 10)
            .await
            .unwrap();
        assert!(out_none.is_empty());
    }

    // Unused-imports guard when this module is built in isolation.
    #[allow(dead_code)]
    fn _keep_imports_alive(_: Bytes, _: B256) {}
}
