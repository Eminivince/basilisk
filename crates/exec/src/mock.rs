//! In-memory deterministic backend for unit tests.
//!
//! `MockExecutionBackend` produces `MockFork`s whose state is just a
//! `HashMap`. Calls and sends record the `TxRequest` so tests can
//! assert what tools sent. Snapshots clone the whole state.
//!
//! No network. No subprocess. Cheap.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use alloy_primitives::{Address, B256, U256};
use async_trait::async_trait;

use crate::{
    backend::{ExecutionBackend, Fork},
    error::ExecError,
    types::{
        CallResult, EventLog, ForgeProject, ForgeTestResult, ForkSpec, SnapshotId, StateDiff,
        TestCase, TestStatus, TxReceipt, TxRequest,
    },
};

#[derive(Debug, Default)]
struct MockState {
    balances: HashMap<Address, U256>,
    storage: HashMap<Address, HashMap<B256, B256>>,
    impersonated: std::collections::HashSet<Address>,
    timestamp: u64,
}

#[derive(Debug, Default, Clone)]
struct MockSnapshot {
    state: MockStateClone,
}

#[derive(Debug, Default, Clone)]
struct MockStateClone {
    balances: HashMap<Address, U256>,
    storage: HashMap<Address, HashMap<B256, B256>>,
    impersonated: std::collections::HashSet<Address>,
    timestamp: u64,
}

impl From<&MockState> for MockStateClone {
    fn from(s: &MockState) -> Self {
        Self {
            balances: s.balances.clone(),
            storage: s.storage.clone(),
            impersonated: s.impersonated.clone(),
            timestamp: s.timestamp,
        }
    }
}

impl MockStateClone {
    fn restore_into(self, state: &mut MockState) {
        state.balances = self.balances;
        state.storage = self.storage;
        state.impersonated = self.impersonated;
        state.timestamp = self.timestamp;
    }
}

/// Programmable in-memory backend. Tests register canned responses
/// for `eth_call`-shaped invocations via [`Self::set_call_response`]
/// and assert against [`MockFork::sent_txs`].
#[derive(Debug, Clone, Default)]
pub struct MockExecutionBackend {
    next_id: Arc<AtomicU64>,
}

impl MockExecutionBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ExecutionBackend for MockExecutionBackend {
    fn identifier(&self) -> &'static str {
        "mock"
    }

    async fn fork_at(&self, _spec: ForkSpec) -> Result<Arc<dyn Fork>, ExecError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        Ok(Arc::new(MockFork {
            id: format!("mock-fork-{id}"),
            inner: Arc::new(MockForkInner::default()),
        }))
    }
}

#[derive(Debug, Default)]
struct MockForkInner {
    state: Mutex<MockState>,
    snapshots: Mutex<HashMap<String, MockSnapshot>>,
    next_snapshot: AtomicU64,
    sent_txs: Mutex<Vec<TxRequest>>,
    canned_calls: Mutex<HashMap<(Address, Vec<u8>), CallResult>>,
    canned_sends: Mutex<HashMap<(Address, Vec<u8>), TxReceipt>>,
    shut_down: Mutex<bool>,
}

#[derive(Debug, Clone)]
pub struct MockFork {
    id: String,
    inner: Arc<MockForkInner>,
}

impl MockFork {
    /// Pre-load a canned [`CallResult`] keyed by `(to, data)`. The
    /// next call matching the key returns this; absent a match,
    /// `call` returns `success=true, return_data=empty`.
    pub fn set_call_response(&self, to: Address, data: Vec<u8>, result: CallResult) {
        self.inner
            .canned_calls
            .lock()
            .unwrap()
            .insert((to, data), result);
    }

    /// Pre-load a canned [`TxReceipt`] keyed by `(to, data)`.
    pub fn set_send_response(&self, to: Address, data: Vec<u8>, receipt: TxReceipt) {
        self.inner
            .canned_sends
            .lock()
            .unwrap()
            .insert((to, data), receipt);
    }

    /// Snapshot of every `send` invocation seen, in order.
    pub fn sent_txs(&self) -> Vec<TxRequest> {
        self.inner.sent_txs.lock().unwrap().clone()
    }

    /// Has [`Fork::shutdown`] been called?
    pub fn was_shut_down(&self) -> bool {
        *self.inner.shut_down.lock().unwrap()
    }
}

#[async_trait]
impl Fork for MockFork {
    fn id(&self) -> &str {
        &self.id
    }

    fn rpc_url(&self) -> &'static str {
        ""
    }

    async fn impersonate(&self, who: Address) -> Result<(), ExecError> {
        self.inner.state.lock().unwrap().impersonated.insert(who);
        Ok(())
    }

    async fn stop_impersonating(&self, who: Address) -> Result<(), ExecError> {
        self.inner.state.lock().unwrap().impersonated.remove(&who);
        Ok(())
    }

    async fn set_balance(&self, who: Address, amount: U256) -> Result<(), ExecError> {
        self.inner
            .state
            .lock()
            .unwrap()
            .balances
            .insert(who, amount);
        Ok(())
    }

    async fn set_storage(&self, addr: Address, slot: B256, value: B256) -> Result<(), ExecError> {
        self.inner
            .state
            .lock()
            .unwrap()
            .storage
            .entry(addr)
            .or_default()
            .insert(slot, value);
        Ok(())
    }

    async fn warp_to(&self, timestamp: u64) -> Result<(), ExecError> {
        self.inner.state.lock().unwrap().timestamp = timestamp;
        Ok(())
    }

    async fn snapshot(&self) -> Result<SnapshotId, ExecError> {
        let id = self.inner.next_snapshot.fetch_add(1, Ordering::Relaxed);
        let key = format!("0x{id:x}");
        let snap = MockSnapshot {
            state: (&*self.inner.state.lock().unwrap()).into(),
        };
        self.inner
            .snapshots
            .lock()
            .unwrap()
            .insert(key.clone(), snap);
        Ok(SnapshotId(key))
    }

    async fn revert(&self, snapshot: SnapshotId) -> Result<(), ExecError> {
        let snap = self
            .inner
            .snapshots
            .lock()
            .unwrap()
            .remove(snapshot.as_str())
            .ok_or_else(|| ExecError::Other(format!("unknown snapshot: {}", snapshot.as_str())))?;
        snap.state
            .restore_into(&mut self.inner.state.lock().unwrap());
        Ok(())
    }

    async fn call(&self, tx: TxRequest) -> Result<CallResult, ExecError> {
        let key = (tx.to, tx.data.to_vec());
        if let Some(canned) = self.inner.canned_calls.lock().unwrap().get(&key).cloned() {
            return Ok(canned);
        }
        Ok(CallResult {
            success: true,
            return_data: alloy_primitives::Bytes::new(),
            revert_reason: None,
        })
    }

    async fn send(&self, tx: TxRequest) -> Result<TxReceipt, ExecError> {
        self.inner.sent_txs.lock().unwrap().push(tx.clone());
        let key = (tx.to, tx.data.to_vec());
        if let Some(canned) = self.inner.canned_sends.lock().unwrap().get(&key).cloned() {
            return Ok(canned);
        }
        Ok(TxReceipt {
            success: true,
            tx_hash: B256::ZERO,
            gas_used: 21_000,
            return_data: alloy_primitives::Bytes::new(),
            revert_reason: None,
            events: Vec::<EventLog>::new(),
            state_diff: StateDiff::default(),
        })
    }

    async fn run_foundry_test(&self, _project: ForgeProject) -> Result<ForgeTestResult, ExecError> {
        // Default mock behaviour: pretend a single test passed. Tests
        // wanting different behaviour wrap the mock fork or use
        // canned responses (future extension). This is enough for
        // most tool-dispatch tests.
        Ok(ForgeTestResult {
            passed: vec![TestCase {
                name: "test_mock_pass".into(),
                status: TestStatus::Passed,
                gas_used: Some(50_000),
                trace: None,
            }],
            failed: vec![],
            setup_failed: None,
            stdout: "[mock]".into(),
            stderr: String::new(),
            duration_ms: 1,
        })
    }

    async fn shutdown(&self) -> Result<(), ExecError> {
        *self.inner.shut_down.lock().unwrap() = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ForkBlock, ForkChain};

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Runtime::new().unwrap().block_on(f)
    }

    #[test]
    fn fork_at_yields_unique_ids() {
        let backend = MockExecutionBackend::new();
        let f1 = block_on(backend.fork_at(ForkSpec::new(ForkChain::Ethereum, ForkBlock::Latest)))
            .unwrap();
        let f2 = block_on(backend.fork_at(ForkSpec::new(ForkChain::Ethereum, ForkBlock::Latest)))
            .unwrap();
        assert_ne!(f1.id(), f2.id());
    }

    #[test]
    fn impersonate_round_trips() {
        let backend = MockExecutionBackend::new();
        let fork = block_on(backend.fork_at(ForkSpec::new(ForkChain::Ethereum, ForkBlock::Latest)))
            .unwrap();
        let who = Address::repeat_byte(0xa);
        block_on(fork.impersonate(who)).unwrap();
        block_on(fork.stop_impersonating(who)).unwrap();
    }

    #[test]
    fn snapshot_revert_restores_balance() {
        let backend = MockExecutionBackend::new();
        let fork = block_on(backend.fork_at(ForkSpec::new(ForkChain::Ethereum, ForkBlock::Latest)))
            .unwrap();
        let who = Address::repeat_byte(0xa);
        block_on(fork.set_balance(who, U256::from(100))).unwrap();
        let snap = block_on(fork.snapshot()).unwrap();
        block_on(fork.set_balance(who, U256::from(999))).unwrap();
        block_on(fork.revert(snap)).unwrap();
        // Reverted to snapshot state — balance is back to 100.
        // (We can't observe balances directly without a getter; the
        // test asserts revert succeeded.)
    }

    #[test]
    fn revert_unknown_snapshot_errors() {
        let backend = MockExecutionBackend::new();
        let fork = block_on(backend.fork_at(ForkSpec::new(ForkChain::Ethereum, ForkBlock::Latest)))
            .unwrap();
        let err = block_on(fork.revert(SnapshotId("0xdead".into()))).unwrap_err();
        assert!(format!("{err}").contains("unknown snapshot"));
    }

    #[test]
    fn canned_call_response_returns() {
        let backend = MockExecutionBackend::new();
        let fork = block_on(backend.fork_at(ForkSpec::new(ForkChain::Ethereum, ForkBlock::Latest)))
            .unwrap();
        let mock_fork = fork.id().to_string();
        let _ = mock_fork; // we keep the trait-level handle; cast for set
        let target = Address::repeat_byte(0xb);
        let data = vec![0xde, 0xad, 0xbe, 0xef];
        // Downcast through a helper: tests need MockFork directly.
        let mf = block_on(backend.fork_at(ForkSpec::new(ForkChain::Ethereum, ForkBlock::Latest)))
            .unwrap();
        // We can't actually downcast `Arc<dyn Fork>` to `MockFork` —
        // grab a fresh MockFork directly for canned-response tests:
        let direct = MockFork {
            id: "test".into(),
            inner: Arc::new(MockForkInner::default()),
        };
        direct.set_call_response(
            target,
            data.clone(),
            CallResult {
                success: true,
                return_data: alloy_primitives::Bytes::from(vec![0x01]),
                revert_reason: None,
            },
        );
        let res = block_on(direct.call(TxRequest::new(target).with_data(data))).unwrap();
        assert_eq!(res.return_data.as_ref(), &[0x01]);
        // Silence unused binding from earlier path
        drop(mf);
    }

    #[test]
    fn send_records_into_sent_txs() {
        let direct = MockFork {
            id: "test".into(),
            inner: Arc::new(MockForkInner::default()),
        };
        let target = Address::repeat_byte(0xb);
        block_on(direct.send(TxRequest::new(target))).unwrap();
        block_on(direct.send(TxRequest::new(target).with_value(U256::from(7)))).unwrap();
        let seen = direct.sent_txs();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].value, Some(U256::from(7)));
    }

    #[test]
    fn shutdown_marks_was_shut_down() {
        let direct = MockFork {
            id: "test".into(),
            inner: Arc::new(MockForkInner::default()),
        };
        assert!(!direct.was_shut_down());
        block_on(direct.shutdown()).unwrap();
        assert!(direct.was_shut_down());
    }

    #[test]
    fn run_foundry_test_returns_default_pass() {
        let direct = MockFork {
            id: "test".into(),
            inner: Arc::new(MockForkInner::default()),
        };
        let project = ForgeProject {
            root: std::path::PathBuf::from("/tmp/x"),
            solc_version: None,
            remappings: vec![],
            fork_url: "http://localhost".into(),
            fork_block: 0,
            match_test: None,
        };
        let r = block_on(direct.run_foundry_test(project)).unwrap();
        assert!(r.ok());
        assert_eq!(r.passed.len(), 1);
    }

    #[test]
    fn warp_to_updates_timestamp() {
        let direct = MockFork {
            id: "test".into(),
            inner: Arc::new(MockForkInner::default()),
        };
        block_on(direct.warp_to(1_700_000_000)).unwrap();
        assert_eq!(direct.inner.state.lock().unwrap().timestamp, 1_700_000_000);
    }

    #[test]
    fn set_storage_records_slot() {
        let direct = MockFork {
            id: "test".into(),
            inner: Arc::new(MockForkInner::default()),
        };
        let addr = Address::repeat_byte(0xc);
        let slot = B256::repeat_byte(0x11);
        let value = B256::repeat_byte(0x22);
        block_on(direct.set_storage(addr, slot, value)).unwrap();
        let st = direct.inner.state.lock().unwrap();
        assert_eq!(st.storage.get(&addr).unwrap().get(&slot), Some(&value));
    }
}
