//! Execution backends for forked-EVM simulation.
//!
//! Two layers:
//!
//!   1. The [`ExecutionBackend`] / [`Fork`] traits define a chain-agnostic
//!      surface for "spin up a forked copy of mainnet at block N, then
//!      manipulate it." Callers (the analytical tools, the agent's
//!      `simulate_call_chain`, the `PoC` runner) hold `Arc<dyn Fork>` and
//!      don't care which backend is providing the EVM underneath.
//!   2. Two concrete impls ship: [`AnvilForkBackend`] spawns `anvil`
//!      subprocesses for full Foundry compatibility (cheatcodes, traces,
//!      `forge test --fork-url`); [`MockExecutionBackend`] is the
//!      deterministic in-memory backend used by unit tests.
//!
//! See the module docs on [`backend`] for the trait shape and
//! [`anvil`] / [`mock`] for the implementations.

pub mod anvil;
pub mod backend;
pub mod error;
pub mod mock;
pub mod rpc_url;
pub mod types;

pub use anvil::{AnvilFork, AnvilForkBackend};
pub use backend::{ExecutionBackend, Fork};
pub use error::ExecError;
pub use mock::{MockExecutionBackend, MockFork};
pub use rpc_url::resolve_rpc_url;
pub use types::{
    CallResult, EventLog, ForgeProject, ForgeTestResult, ForkBlock, ForkChain, ForkSpec,
    SnapshotId, StateDiff, StorageDiff, TestCase, TestStatus, TxReceipt, TxRequest,
};
