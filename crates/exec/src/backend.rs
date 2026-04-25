//! The [`ExecutionBackend`] / [`Fork`] traits.

use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use async_trait::async_trait;

use crate::{
    error::ExecError,
    types::{
        CallResult, ForgeProject, ForgeTestResult, ForkSpec, SnapshotId, TxReceipt, TxRequest,
    },
};

/// Owner of fork resources. Spawns forks; tracks them; tears them
/// down. Cheap to clone (`Arc<dyn ExecutionBackend>`).
#[async_trait]
pub trait ExecutionBackend: Send + Sync {
    /// Stable identifier of this backend (e.g. `"anvil"`, `"mock"`).
    /// Tools surface this so downstream telemetry can distinguish
    /// "ran on anvil" vs "ran in the deterministic mock."
    fn identifier(&self) -> &'static str;

    /// Create a new fork at the given block. Must be cancel-safe.
    async fn fork_at(&self, spec: ForkSpec) -> Result<Arc<dyn Fork>, ExecError>;
}

/// One forked EVM. Methods read or mutate the fork's local state —
/// **never** the upstream chain. Implementations must guarantee this:
/// transactions broadcast through `send` go to the local fork's
/// in-memory chain, not the upstream.
#[async_trait]
pub trait Fork: Send + Sync {
    /// Stable id of this fork instance — usually a port number, uuid,
    /// or whatever the backend uses to keep multiple forks separate.
    fn id(&self) -> &str;

    /// Local RPC URL the fork exposes. `forge test --fork-url` etc.
    /// connect here. Backends that don't expose an HTTP endpoint
    /// (e.g. the mock used in tests) return an empty string.
    fn rpc_url(&self) -> &str;

    /// Cause the fork to accept transactions signed by `who` without
    /// the private key. Inverse: [`Self::stop_impersonating`].
    async fn impersonate(&self, who: Address) -> Result<(), ExecError>;
    async fn stop_impersonating(&self, who: Address) -> Result<(), ExecError>;

    async fn set_balance(&self, who: Address, amount: U256) -> Result<(), ExecError>;
    async fn set_storage(&self, addr: Address, slot: B256, value: B256) -> Result<(), ExecError>;
    async fn warp_to(&self, timestamp: u64) -> Result<(), ExecError>;

    /// Snapshot the fork's current state. Pair with [`Self::revert`]
    /// to roll back exploratory mutations.
    async fn snapshot(&self) -> Result<SnapshotId, ExecError>;
    async fn revert(&self, snapshot: SnapshotId) -> Result<(), ExecError>;

    /// Read-only call.
    async fn call(&self, tx: TxRequest) -> Result<CallResult, ExecError>;

    /// State-modifying transaction. Capture-aware: implementations
    /// populate [`TxReceipt::state_diff`] so `simulate_call_chain`
    /// can show what changed.
    async fn send(&self, tx: TxRequest) -> Result<TxReceipt, ExecError>;

    /// Run a Foundry test against this fork's RPC URL. Anvil-backed
    /// forks delegate to `forge test --fork-url`; the mock backend
    /// returns a deterministic stub. The revm-only backend (future)
    /// will return [`ExecError::Unsupported`].
    async fn run_foundry_test(&self, project: ForgeProject) -> Result<ForgeTestResult, ExecError>;

    /// Tear the fork down explicitly. The backend's `Drop` runs this
    /// too on a best-effort basis, but explicit shutdown is preferred
    /// for predictable resource lifetimes.
    async fn shutdown(&self) -> Result<(), ExecError>;
}
