//! The narrow [`RpcProvider`] trait every consumer codes against.

use alloy_primitives::{Address, Bytes, B256};
use alloy_rpc_types_eth::Filter as AlloyFilter;
use async_trait::async_trait;
use basilisk_core::Chain;

use crate::{
    error::RpcError,
    types::{RpcLog, RpcTransaction},
};

/// Opaque wrapper around `alloy_rpc_types_eth::Filter` so downstream callers
/// can build filters without pulling alloy directly.
#[derive(Debug, Clone, Default)]
pub struct LogFilter(pub AlloyFilter);

impl LogFilter {
    #[must_use]
    pub fn new() -> Self {
        Self(AlloyFilter::default())
    }

    /// Underlying alloy filter.
    #[must_use]
    pub fn inner(&self) -> &AlloyFilter {
        &self.0
    }

    /// Consume and produce the alloy filter.
    #[must_use]
    pub fn into_inner(self) -> AlloyFilter {
        self.0
    }
}

impl From<AlloyFilter> for LogFilter {
    fn from(f: AlloyFilter) -> Self {
        Self(f)
    }
}

/// Minimal RPC surface for contract introspection.
///
/// Implementations must be cheap to clone and `Send + Sync` so orchestrators
/// can share a single provider across parallel tasks.
#[async_trait]
pub trait RpcProvider: Send + Sync {
    /// The chain this provider talks to.
    fn chain(&self) -> &Chain;

    /// The endpoint URL, possibly redacted for display. Implementations must
    /// not expose raw API keys here.
    fn endpoint(&self) -> String;

    /// Fetch deployed bytecode at `address`. Empty `Bytes` means EOA or
    /// self-destructed contract.
    async fn get_code(&self, address: Address) -> Result<Bytes, RpcError>;

    /// Read a single 32-byte storage slot.
    async fn get_storage_at(&self, address: Address, slot: B256) -> Result<B256, RpcError>;

    /// Perform an `eth_call` with `to` and `data`; returns the raw return bytes.
    async fn call(&self, to: Address, data: Bytes) -> Result<Bytes, RpcError>;

    /// Return the chain ID reported by the endpoint.
    async fn chain_id(&self) -> Result<u64, RpcError>;

    /// Fetch event logs matching `filter`.
    async fn get_logs(&self, filter: LogFilter) -> Result<Vec<RpcLog>, RpcError>;

    /// Fetch a transaction by hash. `None` if unknown.
    async fn get_transaction(&self, hash: B256) -> Result<Option<RpcTransaction>, RpcError>;

    /// Fetch the timestamp (UNIX seconds) for a block. `None` if the block
    /// is pruned or unknown.
    async fn get_block_timestamp(&self, block: u64) -> Result<Option<u64>, RpcError>;

    /// Fetch the current head block number.
    async fn get_block_number(&self) -> Result<u64, RpcError>;

    /// Returns `true` iff the address has non-empty runtime bytecode.
    async fn is_contract(&self, address: Address) -> Result<bool, RpcError>;
}
