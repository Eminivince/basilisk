//! In-memory [`RpcProvider`] for tests in this crate and its downstream
//! consumers (e.g. `basilisk-onchain`).
//!
//! Storage/call responses are programmable via `.with_code`, `.with_slot`,
//! `.with_call_response`. Unconfigured reads return sensible defaults
//! (empty bytes / zero slot) so a caller that only cares about bytecode
//! doesn't have to pre-populate the full storage map.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use alloy_primitives::{Address, Bytes, B256};
use async_trait::async_trait;
use basilisk_core::Chain;

use crate::{error::RpcError, provider::RpcProvider};

/// Programmable in-memory provider. Cheap to clone (shared state).
#[derive(Debug, Clone)]
pub struct MemoryProvider {
    chain: Chain,
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
    code: HashMap<Address, Bytes>,
    storage: HashMap<(Address, B256), B256>,
    calls: HashMap<(Address, Bytes), Result<Bytes, RpcError>>,
    chain_id_override: Option<u64>,
    call_count: u64,
}

impl MemoryProvider {
    /// Build a provider pinned to `chain`. All lookups return defaults until configured.
    pub fn new(chain: Chain) -> Self {
        Self {
            chain,
            inner: Arc::new(Mutex::new(Inner::default())),
        }
    }

    /// Register bytecode for an address.
    #[must_use]
    pub fn with_code(self, address: Address, code: impl Into<Bytes>) -> Self {
        self.inner.lock().unwrap().code.insert(address, code.into());
        self
    }

    /// Register a storage slot value.
    #[must_use]
    pub fn with_slot(self, address: Address, slot: B256, value: B256) -> Self {
        self.inner
            .lock()
            .unwrap()
            .storage
            .insert((address, slot), value);
        self
    }

    /// Register a successful `eth_call` response.
    #[must_use]
    pub fn with_call_response(
        self,
        to: Address,
        data: impl Into<Bytes>,
        out: impl Into<Bytes>,
    ) -> Self {
        self.inner
            .lock()
            .unwrap()
            .calls
            .insert((to, data.into()), Ok(out.into()));
        self
    }

    /// Register a call that should fail.
    #[must_use]
    pub fn with_call_error(self, to: Address, data: impl Into<Bytes>, err: RpcError) -> Self {
        self.inner
            .lock()
            .unwrap()
            .calls
            .insert((to, data.into()), Err(err));
        self
    }

    /// Override the reported chain ID; defaults to `chain.chain_id()`.
    #[must_use]
    pub fn with_chain_id(self, id: u64) -> Self {
        self.inner.lock().unwrap().chain_id_override = Some(id);
        self
    }

    /// Total calls observed across all RPC methods.
    pub fn call_count(&self) -> u64 {
        self.inner.lock().unwrap().call_count
    }
}

#[async_trait]
impl RpcProvider for MemoryProvider {
    fn chain(&self) -> &Chain {
        &self.chain
    }

    fn endpoint(&self) -> String {
        "memory://test".to_string()
    }

    async fn get_code(&self, address: Address) -> Result<Bytes, RpcError> {
        let mut i = self.inner.lock().unwrap();
        i.call_count += 1;
        Ok(i.code.get(&address).cloned().unwrap_or_default())
    }

    async fn get_storage_at(&self, address: Address, slot: B256) -> Result<B256, RpcError> {
        let mut i = self.inner.lock().unwrap();
        i.call_count += 1;
        Ok(i.storage.get(&(address, slot)).copied().unwrap_or_default())
    }

    async fn call(&self, to: Address, data: Bytes) -> Result<Bytes, RpcError> {
        let mut i = self.inner.lock().unwrap();
        i.call_count += 1;
        match i.calls.get(&(to, data.clone())) {
            Some(Ok(out)) => Ok(out.clone()),
            Some(Err(e)) => Err(clone_rpc_error(e)),
            None => Ok(Bytes::new()),
        }
    }

    async fn chain_id(&self) -> Result<u64, RpcError> {
        let mut i = self.inner.lock().unwrap();
        i.call_count += 1;
        Ok(i.chain_id_override.unwrap_or_else(|| self.chain.chain_id()))
    }
}

// RpcError doesn't derive Clone (thiserror on std::io::Error etc. — not
// applicable here, but kept consistent). For the memory provider's purposes,
// we only reuse error variants that carry String payloads.
fn clone_rpc_error(e: &RpcError) -> RpcError {
    match e {
        RpcError::NoProviderConfigured { chain, suggestion } => RpcError::NoProviderConfigured {
            chain: chain.clone(),
            suggestion: suggestion.clone(),
        },
        RpcError::InvalidUrl { url, detail } => RpcError::InvalidUrl {
            url: url.clone(),
            detail: detail.clone(),
        },
        RpcError::Transient(s) => RpcError::Transient(s.clone()),
        RpcError::RateLimited => RpcError::RateLimited,
        RpcError::Timeout { secs } => RpcError::Timeout { secs: *secs },
        RpcError::Server(s) => RpcError::Server(s.clone()),
        RpcError::Cache(s) => RpcError::Cache(s.clone()),
        RpcError::Other(s) => RpcError::Other(s.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(byte: u8) -> Address {
        let mut b = [0u8; 20];
        b[19] = byte;
        Address::from(b)
    }

    #[tokio::test]
    async fn defaults_to_empty_code_and_zero_slot() {
        let p = MemoryProvider::new(Chain::EthereumMainnet);
        assert_eq!(p.get_code(addr(1)).await.unwrap(), Bytes::new());
        assert_eq!(
            p.get_storage_at(addr(1), B256::ZERO).await.unwrap(),
            B256::ZERO
        );
    }

    #[tokio::test]
    async fn configured_code_returned() {
        let p = MemoryProvider::new(Chain::EthereumMainnet)
            .with_code(addr(1), Bytes::from_static(&[0xde, 0xad]));
        assert_eq!(
            p.get_code(addr(1)).await.unwrap(),
            Bytes::from_static(&[0xde, 0xad])
        );
    }

    #[tokio::test]
    async fn configured_call_returned_and_unconfigured_is_empty() {
        let p = MemoryProvider::new(Chain::EthereumMainnet).with_call_response(
            addr(1),
            Bytes::from_static(&[0x01]),
            Bytes::from_static(&[0x02]),
        );
        let out = p.call(addr(1), Bytes::from_static(&[0x01])).await.unwrap();
        assert_eq!(out, Bytes::from_static(&[0x02]));
        assert_eq!(p.call(addr(2), Bytes::new()).await.unwrap(), Bytes::new());
    }

    #[tokio::test]
    async fn chain_id_defaults_to_chain_but_can_be_overridden() {
        let p = MemoryProvider::new(Chain::Arbitrum);
        assert_eq!(p.chain_id().await.unwrap(), 42_161);
        let p2 = MemoryProvider::new(Chain::Arbitrum).with_chain_id(31_337);
        assert_eq!(p2.chain_id().await.unwrap(), 31_337);
    }

    #[tokio::test]
    async fn call_count_increments() {
        let p = MemoryProvider::new(Chain::EthereumMainnet);
        let _ = p.get_code(addr(1)).await;
        let _ = p.chain_id().await;
        assert_eq!(p.call_count(), 2);
    }
}
