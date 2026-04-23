//! The narrow [`RpcProvider`] trait every consumer codes against.

use alloy_primitives::{Address, Bytes, B256};
use async_trait::async_trait;
use basilisk_core::Chain;

use crate::error::RpcError;

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
}
