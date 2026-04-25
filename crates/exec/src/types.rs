//! Shared data types for the execution surface.

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use alloy_primitives::{Address, Bytes, B256, U256};
use serde::{Deserialize, Serialize};

/// `serde` skip-helper: `Bytes` doesn't expose a free-standing
/// `is_empty` matching the `&Bytes -> bool` shape that
/// `skip_serializing_if` wants.
fn bytes_is_empty(b: &Bytes) -> bool {
    b.as_ref().is_empty()
}

/// Which chain a fork should target. Matches the subset of
/// `basilisk-core::Chain` we actually fork against in Set 9 — adding a
/// new chain is just adding a variant + a default RPC URL pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkChain {
    Ethereum,
    Optimism,
    Arbitrum,
    Polygon,
    Base,
    Bnb,
}

impl ForkChain {
    /// Lower-case canonical name. Used for env-var lookups
    /// (`RPC_URL_ETHEREUM`) and Alchemy URL templates.
    pub fn canonical(self) -> &'static str {
        match self {
            Self::Ethereum => "ethereum",
            Self::Optimism => "optimism",
            Self::Arbitrum => "arbitrum",
            Self::Polygon => "polygon",
            Self::Base => "base",
            Self::Bnb => "bnb",
        }
    }

    /// Numeric chain id — sanity-check the upstream returns the right one.
    pub fn id(self) -> u64 {
        match self {
            Self::Ethereum => 1,
            Self::Optimism => 10,
            Self::Arbitrum => 42_161,
            Self::Polygon => 137,
            Self::Base => 8_453,
            Self::Bnb => 56,
        }
    }
}

/// Block selector for `fork_at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkBlock {
    /// Fork at chain head — uses the upstream's view of `latest`.
    Latest,
    /// Specific block number. Most benchmark targets pin a block here.
    Number(u64),
}

/// What the operator wants from a fork — chain, block, and any
/// optional knobs the backend should honour at startup.
#[derive(Debug, Clone)]
pub struct ForkSpec {
    pub chain: ForkChain,
    pub block: ForkBlock,
    /// Override the upstream RPC URL. When `None`, the backend resolves
    /// from `MAINNET_RPC_URL` / `RPC_URL_<CHAIN>` / `ALCHEMY_API_KEY`.
    pub upstream_rpc_url: Option<String>,
    /// Per-fork timeout — anvil shuts itself down after this if the
    /// caller forgets to. Defaults to 30 minutes; vuln runs are long
    /// but not infinite.
    pub idle_timeout: Duration,
}

impl ForkSpec {
    pub fn new(chain: ForkChain, block: ForkBlock) -> Self {
        Self {
            chain,
            block,
            upstream_rpc_url: None,
            idle_timeout: Duration::from_secs(30 * 60),
        }
    }
}

/// Opaque snapshot id returned by [`Fork::snapshot`](crate::Fork::snapshot).
/// Anvil emits these as hex-encoded `U256`; we store the original string
/// so callers can hand it back to `revert` without re-parsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SnapshotId(pub String);

impl SnapshotId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One read-only call against a fork. `from` defaults to
/// `Address::ZERO` if absent — anvil treats that as "no impersonation."
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TxRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<Address>,
    pub to: Address,
    #[serde(skip_serializing_if = "bytes_is_empty", default)]
    pub data: Bytes,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<U256>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas: Option<u64>,
}

impl TxRequest {
    pub fn new(to: Address) -> Self {
        Self {
            to,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_data(mut self, data: impl Into<Bytes>) -> Self {
        self.data = data.into();
        self
    }

    #[must_use]
    pub fn with_from(mut self, from: Address) -> Self {
        self.from = Some(from);
        self
    }

    #[must_use]
    pub fn with_value(mut self, value: U256) -> Self {
        self.value = Some(value);
        self
    }
}

/// Outcome of an `eth_call`-style invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallResult {
    pub success: bool,
    /// Hex-encoded return data on success; raw revert bytes on failure.
    pub return_data: Bytes,
    /// Decoded revert reason when one was present (`Error(string)` /
    /// `Panic(uint256)`). `None` when the call succeeded or the revert
    /// was bare.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revert_reason: Option<String>,
}

/// Outcome of a state-modifying transaction (anvil broadcasts it
/// against its in-memory chain state — no upstream broadcast).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxReceipt {
    pub success: bool,
    pub tx_hash: B256,
    pub gas_used: u64,
    pub return_data: Bytes,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revert_reason: Option<String>,
    pub events: Vec<EventLog>,
    pub state_diff: StateDiff,
}

/// One log entry decoded into structured form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventLog {
    pub address: Address,
    pub topics: Vec<B256>,
    pub data: Bytes,
}

/// Per-address summary of slots that changed during a transaction.
/// Compact by design — we don't dump full state, only what was
/// touched, so the agent can reason about it without choking on
/// noise.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateDiff {
    /// Balance and nonce changes per address.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub balances: BTreeMap<Address, U256>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub nonces: BTreeMap<Address, u64>,
    /// Storage slot changes per address.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub storage: BTreeMap<Address, StorageDiff>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageDiff {
    #[serde(flatten)]
    pub slots: BTreeMap<B256, B256>,
}

impl StorageDiff {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, slot: B256, value: B256) {
        self.slots.insert(slot, value);
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }
}

/// Inputs for a `forge test --fork-url` run. The execution backend
/// owns scaffolding — temp dir, foundry.toml, src/, test/. Caller
/// supplies the test source and the chain context.
#[derive(Debug, Clone)]
pub struct ForgeProject {
    pub root: PathBuf,
    /// Target solc version — passed through to `foundry.toml`'s
    /// `solc = "..."`. When `None`, forge auto-detects via svm.
    pub solc_version: Option<String>,
    /// `remappings.txt` lines. Empty for no remappings.
    pub remappings: Vec<String>,
    /// RPC URL forge connects to for the fork. Usually the same
    /// upstream the backend resolved.
    pub fork_url: String,
    /// Block to fork from.
    pub fork_block: u64,
    /// Optional name filter passed as `--match-test <pattern>`.
    pub match_test: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeTestResult {
    pub passed: Vec<TestCase>,
    pub failed: Vec<TestCase>,
    /// Populated when forge couldn't even reach the test phase
    /// (compile error, missing remapping, etc.). When `Some`, the
    /// `passed` / `failed` lists are empty.
    pub setup_failed: Option<String>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}

impl ForgeTestResult {
    /// `true` when the run fully passed: no setup failure, no failed
    /// cases, at least one passing case.
    pub fn ok(&self) -> bool {
        self.setup_failed.is_none() && self.failed.is_empty() && !self.passed.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestCase {
    pub name: String,
    pub status: TestStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas_used: Option<u64>,
    /// Forge's human-readable trace, when verbose-enough output was
    /// captured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TestStatus {
    Passed,
    Failed { reason: String },
    Skipped,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fork_chain_canonical_and_id_round_trip() {
        assert_eq!(ForkChain::Ethereum.canonical(), "ethereum");
        assert_eq!(ForkChain::Ethereum.id(), 1);
        assert_eq!(ForkChain::Bnb.id(), 56);
    }

    #[test]
    fn tx_request_builder_chain_compiles() {
        let req = TxRequest::new(Address::ZERO)
            .with_data(vec![0x12, 0x34])
            .with_value(U256::from(1_000));
        assert_eq!(req.to, Address::ZERO);
        assert_eq!(req.data.as_ref(), &[0x12, 0x34]);
        assert_eq!(req.value, Some(U256::from(1_000)));
    }

    #[test]
    fn tx_request_serde_omits_empty_data() {
        let req = TxRequest::new(Address::ZERO);
        let s = serde_json::to_string(&req).unwrap();
        assert!(!s.contains("data"));
        assert!(s.contains("\"to\""));
    }

    #[test]
    fn forge_test_result_ok_requires_at_least_one_pass() {
        let r = ForgeTestResult {
            passed: vec![],
            failed: vec![],
            setup_failed: None,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: 0,
        };
        assert!(!r.ok());
    }

    #[test]
    fn forge_test_result_ok_negated_by_failure() {
        let r = ForgeTestResult {
            passed: vec![TestCase {
                name: "a".into(),
                status: TestStatus::Passed,
                gas_used: None,
                trace: None,
            }],
            failed: vec![TestCase {
                name: "b".into(),
                status: TestStatus::Failed { reason: "x".into() },
                gas_used: None,
                trace: None,
            }],
            setup_failed: None,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: 0,
        };
        assert!(!r.ok());
    }

    #[test]
    fn state_diff_default_empty() {
        let sd = StateDiff::default();
        assert!(sd.balances.is_empty());
        assert!(sd.storage.is_empty());
    }

    #[test]
    fn storage_diff_insert_and_len() {
        let mut sd = StorageDiff::new();
        assert!(sd.is_empty());
        sd.insert(B256::ZERO, B256::repeat_byte(0xab));
        assert_eq!(sd.len(), 1);
        assert!(!sd.is_empty());
    }

    #[test]
    fn snapshot_id_round_trips() {
        let s = SnapshotId("0x1".into());
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"0x1\"");
        let back: SnapshotId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn fork_spec_default_idle_timeout_is_thirty_min() {
        let spec = ForkSpec::new(ForkChain::Ethereum, ForkBlock::Latest);
        assert_eq!(spec.idle_timeout.as_secs(), 30 * 60);
    }
}
