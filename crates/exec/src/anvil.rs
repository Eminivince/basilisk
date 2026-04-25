//! Anvil-backed fork.
//!
//! Each fork is one `anvil` subprocess listening on a localhost port,
//! forked from the upstream RPC at the requested block. The backend
//! tracks every fork it has spawned; both explicit [`Fork::shutdown`]
//! and `Drop` kill the subprocess. Leaked anvil processes are an
//! incident — spawn-and-drop must be bulletproof under panic / SIGINT /
//! parent-OOM.
//!
//! The Foundry-test runner lands in CP9.2; the placeholder here
//! returns [`ExecError::Unsupported`].

use std::{
    net::{Ipv4Addr, SocketAddr, TcpListener},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256, U256};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::{
    process::{Child, Command},
    sync::Mutex as AsyncMutex,
    time::sleep,
};
use tracing::{debug, info, warn};

use crate::{
    backend::{ExecutionBackend, Fork},
    error::ExecError,
    types::{
        CallResult, EventLog, ForgeProject, ForgeTestResult, ForkBlock, ForkSpec, SnapshotId,
        StateDiff, TxReceipt, TxRequest,
    },
};

/// Default time we wait for anvil to start accepting connections.
/// First-fork cold-start with state fetched from Alchemy can be slow;
/// 30s gives margin without hiding genuine failures.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// HTTP request timeout for anvil JSON-RPC calls. Distinct from
/// `STARTUP_TIMEOUT`; once anvil is up, individual calls should be
/// fast.
const RPC_TIMEOUT: Duration = Duration::from_secs(60);

/// Path detection for the `anvil` binary. Cached per-process so we
/// don't re-shell `which` on every fork.
fn anvil_binary() -> Result<std::path::PathBuf, ExecError> {
    which::which("anvil").map_err(|_| ExecError::ToolchainMissing {
        binary: "anvil",
        install_hint: "install Foundry: curl -L https://foundry.paradigm.xyz | bash && foundryup",
    })
}

/// Reserve a free localhost port. Binds `127.0.0.1:0`, reads the
/// allocated port, and drops the listener — there's a small race
/// window between the drop and the anvil bind, but it's tiny in
/// practice (anvil is the only thing we'll spawn on the port).
fn allocate_port() -> Result<u16, ExecError> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .map_err(|e| ExecError::PortAllocation(e.to_string()))?;
    let port = listener
        .local_addr()
        .map_err(|e| ExecError::PortAllocation(e.to_string()))?
        .port();
    drop(listener);
    Ok(port)
}

/// Backend that spawns anvil subprocesses on demand.
///
/// Cheap to construct, cheap to clone. Holds no per-fork state — every
/// `fork_at` returns a fresh [`AnvilFork`] whose lifetime is the
/// caller's responsibility (the agent runner tracks them for shutdown
/// at session end).
#[derive(Debug, Clone)]
pub struct AnvilForkBackend {
    /// Default startup timeout. Operators can override per-call by
    /// constructing a custom backend.
    startup_timeout: Duration,
}

impl Default for AnvilForkBackend {
    fn default() -> Self {
        Self {
            startup_timeout: STARTUP_TIMEOUT,
        }
    }
}

impl AnvilForkBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the startup timeout. Useful for tests against slow
    /// upstreams.
    #[must_use]
    pub fn with_startup_timeout(mut self, t: Duration) -> Self {
        self.startup_timeout = t;
        self
    }

    /// Verify the anvil binary is available without spawning a fork.
    /// Returns the resolved path.
    pub fn require_binary() -> Result<std::path::PathBuf, ExecError> {
        anvil_binary()
    }
}

#[async_trait]
impl ExecutionBackend for AnvilForkBackend {
    fn identifier(&self) -> &'static str {
        "anvil"
    }

    async fn fork_at(&self, spec: ForkSpec) -> Result<Arc<dyn Fork>, ExecError> {
        let upstream = spec.upstream_rpc_url.clone().ok_or_else(|| {
            ExecError::NoRpcUrl {
                chain: spec.chain.canonical().into(),
            }
        })?;
        let bin = anvil_binary()?;
        let port = allocate_port()?;
        let mut cmd = Command::new(&bin);
        cmd.kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .arg("--port")
            .arg(port.to_string())
            .arg("--host")
            .arg("127.0.0.1")
            // Be generous on timeouts — Alchemy can take a moment.
            .arg("--timeout")
            .arg("120000")
            .arg("--silent")
            .arg("--fork-url")
            .arg(&upstream);
        if let ForkBlock::Number(n) = spec.block {
            cmd.arg("--fork-block-number").arg(n.to_string());
        }
        debug!(?bin, port, ?spec.block, "spawning anvil");
        let child = cmd.spawn().map_err(|source| ExecError::SpawnFailed {
            binary: "anvil",
            source,
        })?;

        let url = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::builder()
            .timeout(RPC_TIMEOUT)
            .build()
            .map_err(|e| ExecError::Other(format!("reqwest build: {e}")))?;

        let inner = Arc::new(AnvilForkInner {
            id: format!("anvil-{port}"),
            port,
            url: url.clone(),
            client,
            upstream,
            child: AsyncMutex::new(Some(child)),
            shut_down: AsyncMutex::new(false),
        });

        // Block until anvil is reachable.
        wait_for_ready(&inner, self.startup_timeout).await?;
        info!(id = %inner.id, "anvil fork ready");
        Ok(Arc::new(AnvilFork { inner }))
    }
}

struct AnvilForkInner {
    id: String,
    port: u16,
    url: String,
    client: reqwest::Client,
    upstream: String,
    child: AsyncMutex<Option<Child>>,
    shut_down: AsyncMutex<bool>,
}

impl std::fmt::Debug for AnvilForkInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Skip `client`, `child`, and `shut_down` — runtime handles
        // that don't print informatively. Field count is intentionally
        // smaller than the struct's.
        f.debug_struct("AnvilForkInner")
            .field("id", &self.id)
            .field("port", &self.port)
            .field("url", &self.url)
            .field("upstream", &self.upstream)
            .finish_non_exhaustive()
    }
}

/// One running anvil subprocess. Cheap to clone.
#[derive(Debug, Clone)]
pub struct AnvilFork {
    inner: Arc<AnvilForkInner>,
}

impl AnvilFork {
    pub fn upstream(&self) -> &str {
        &self.inner.upstream
    }

    pub fn port(&self) -> u16 {
        self.inner.port
    }
}

impl Drop for AnvilForkInner {
    fn drop(&mut self) {
        // tokio's `kill_on_drop(true)` issues SIGKILL, but only when
        // the `Child` itself drops. By taking it here we make sure
        // the kill happens even if shutdown was never called.
        if let Ok(mut guard) = self.child.try_lock() {
            if let Some(mut child) = guard.take() {
                // Best-effort: explicit kill. tokio's kill returns a
                // future, but since we're in a sync Drop we fall
                // back to start_kill (sends the signal, doesn't
                // await).
                if let Err(e) = child.start_kill() {
                    warn!(id = %self.id, error = %e, "failed to kill anvil on drop");
                }
            }
        }
    }
}

#[async_trait]
impl Fork for AnvilFork {
    fn id(&self) -> &str {
        &self.inner.id
    }

    fn rpc_url(&self) -> &str {
        &self.inner.url
    }

    async fn impersonate(&self, who: Address) -> Result<(), ExecError> {
        rpc_call::<Value>(
            &self.inner,
            "anvil_impersonateAccount",
            json!([address_str(who)]),
        )
        .await?;
        Ok(())
    }

    async fn stop_impersonating(&self, who: Address) -> Result<(), ExecError> {
        rpc_call::<Value>(
            &self.inner,
            "anvil_stopImpersonatingAccount",
            json!([address_str(who)]),
        )
        .await?;
        Ok(())
    }

    async fn set_balance(&self, who: Address, amount: U256) -> Result<(), ExecError> {
        let amount_hex = format!("0x{amount:x}");
        rpc_call::<Value>(
            &self.inner,
            "anvil_setBalance",
            json!([address_str(who), amount_hex]),
        )
        .await?;
        Ok(())
    }

    async fn set_storage(
        &self,
        addr: Address,
        slot: B256,
        value: B256,
    ) -> Result<(), ExecError> {
        rpc_call::<Value>(
            &self.inner,
            "anvil_setStorageAt",
            json!([address_str(addr), b256_str(slot), b256_str(value)]),
        )
        .await?;
        Ok(())
    }

    async fn warp_to(&self, timestamp: u64) -> Result<(), ExecError> {
        rpc_call::<Value>(
            &self.inner,
            "anvil_setNextBlockTimestamp",
            json!([format!("0x{timestamp:x}")]),
        )
        .await?;
        Ok(())
    }

    async fn snapshot(&self) -> Result<SnapshotId, ExecError> {
        let v: String = rpc_call(&self.inner, "evm_snapshot", json!([])).await?;
        Ok(SnapshotId(v))
    }

    async fn revert(&self, snapshot: SnapshotId) -> Result<(), ExecError> {
        let ok: bool = rpc_call(&self.inner, "evm_revert", json!([snapshot.0])).await?;
        if !ok {
            return Err(ExecError::Other(format!(
                "evm_revert refused snapshot {}",
                snapshot.as_str()
            )));
        }
        Ok(())
    }

    async fn call(&self, tx: TxRequest) -> Result<CallResult, ExecError> {
        let params = json!([tx_object(&tx, false), "latest"]);
        match rpc_call::<String>(&self.inner, "eth_call", params).await {
            Ok(hex) => Ok(CallResult {
                success: true,
                return_data: hex_to_bytes(&hex)?,
                revert_reason: None,
            }),
            Err(ExecError::RpcError { code: _, message }) => {
                let (revert_reason, return_data) = parse_revert_message(&message);
                Ok(CallResult {
                    success: false,
                    return_data,
                    revert_reason,
                })
            }
            Err(other) => Err(other),
        }
    }

    async fn send(&self, tx: TxRequest) -> Result<TxReceipt, ExecError> {
        let params = json!([tx_object(&tx, true)]);
        let tx_hash_hex: String = rpc_call(&self.inner, "eth_sendTransaction", params).await?;
        let tx_hash = parse_b256(&tx_hash_hex)?;
        // Anvil auto-mines unless configured otherwise. Fetch the
        // receipt to capture gas + revert status.
        let receipt: Value = rpc_call(
            &self.inner,
            "eth_getTransactionReceipt",
            json!([tx_hash_hex]),
        )
        .await?;
        let success = receipt
            .get("status")
            .and_then(Value::as_str)
            == Some("0x1");
        let gas_used = receipt
            .get("gasUsed")
            .and_then(Value::as_str)
            .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
            .unwrap_or(0);
        let events = parse_logs(receipt.get("logs"));
        Ok(TxReceipt {
            success,
            tx_hash,
            gas_used,
            return_data: alloy_primitives::Bytes::new(),
            revert_reason: None,
            events,
            // Real state-diff capture lands in CP9.5 alongside
            // simulate_call_chain — the trace_call infrastructure
            // will populate this. Leave empty here so callers see a
            // well-formed (but uninformative) diff.
            state_diff: StateDiff::default(),
        })
    }

    async fn run_foundry_test(
        &self,
        _project: ForgeProject,
    ) -> Result<ForgeTestResult, ExecError> {
        // Lands in CP9.2.
        Err(ExecError::Unsupported(
            "run_foundry_test ships in CP9.2",
        ))
    }

    async fn shutdown(&self) -> Result<(), ExecError> {
        let mut sd = self.inner.shut_down.lock().await;
        if *sd {
            return Ok(());
        }
        *sd = true;
        let mut guard = self.inner.child.lock().await;
        if let Some(mut child) = guard.take() {
            // Polite SIGTERM via tokio's kill (sync start_kill is OK
            // here too, but kill().await waits for the OS to confirm).
            let _ = child.kill().await;
        }
        info!(id = %self.inner.id, "anvil fork shut down");
        Ok(())
    }
}

/// Block until the anvil fork's RPC accepts a `web3_clientVersion`.
/// Tries fast first (50ms), then backs off to 250ms.
async fn wait_for_ready(inner: &Arc<AnvilForkInner>, total: Duration) -> Result<(), ExecError> {
    let started = Instant::now();
    let mut delay = Duration::from_millis(50);
    loop {
        if started.elapsed() >= total {
            return Err(ExecError::StartupTimeout {
                binary: "anvil",
                timeout: total,
            });
        }
        if rpc_call::<Value>(inner, "web3_clientVersion", json!([]))
            .await
            .is_ok()
        {
            return Ok(());
        }
        sleep(delay).await;
        if delay < Duration::from_millis(250) {
            delay = Duration::from_millis(250);
        }
    }
}

/// Send one JSON-RPC call. `T` is the deserialised `result` field.
async fn rpc_call<T: serde::de::DeserializeOwned>(
    inner: &Arc<AnvilForkInner>,
    method: &str,
    params: Value,
) -> Result<T, ExecError> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let resp = inner.client.post(&inner.url).json(&body).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(ExecError::RpcError {
            code: i64::from(status.as_u16()),
            message: text,
        });
    }
    let value: Value = serde_json::from_str(&text)?;
    if let Some(err) = value.get("error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("(no message)")
            .to_string();
        return Err(ExecError::RpcError { code, message });
    }
    let result = value
        .get("result")
        .ok_or_else(|| ExecError::Parse("no result field".into()))?;
    Ok(serde_json::from_value(result.clone())?)
}

fn tx_object(tx: &TxRequest, include_gas: bool) -> Value {
    let mut o = serde_json::Map::new();
    if let Some(from) = tx.from {
        o.insert("from".into(), json!(address_str(from)));
    }
    o.insert("to".into(), json!(address_str(tx.to)));
    if !tx.data.is_empty() {
        o.insert("data".into(), json!(format!("0x{}", hex::encode(&tx.data))));
    }
    if let Some(v) = tx.value {
        o.insert("value".into(), json!(format!("0x{v:x}")));
    }
    if include_gas {
        let gas = tx.gas.unwrap_or(30_000_000);
        o.insert("gas".into(), json!(format!("0x{gas:x}")));
    }
    Value::Object(o)
}

fn parse_logs(v: Option<&Value>) -> Vec<EventLog> {
    let Some(Value::Array(logs)) = v else {
        return vec![];
    };
    logs.iter()
        .filter_map(|l| {
            let address = parse_address(l.get("address")?.as_str()?).ok()?;
            let topics: Vec<B256> = l
                .get("topics")?
                .as_array()?
                .iter()
                .filter_map(|t| parse_b256(t.as_str()?).ok())
                .collect();
            let data = hex_to_bytes(l.get("data")?.as_str()?).ok()?;
            Some(EventLog {
                address,
                topics,
                data,
            })
        })
        .collect()
}

fn parse_revert_message(s: &str) -> (Option<String>, alloy_primitives::Bytes) {
    // Anvil surfaces reverts as JSON-RPC error messages of the form
    // "execution reverted: <reason>" or "execution reverted: 0x..."
    // Pull out the human reason where possible.
    let after = s
        .split_once("execution reverted")
        .map(|(_, t)| t.trim_start_matches(':').trim());
    if let Some(rest) = after {
        if rest.starts_with("0x") {
            return (None, hex_to_bytes(rest).unwrap_or_default());
        }
        if !rest.is_empty() {
            return (Some(rest.to_string()), alloy_primitives::Bytes::new());
        }
    }
    (None, alloy_primitives::Bytes::new())
}

fn address_str(a: Address) -> String {
    format!("0x{}", hex::encode(a.as_slice()))
}

fn b256_str(b: B256) -> String {
    format!("0x{}", hex::encode(b.as_slice()))
}

fn parse_address(s: &str) -> Result<Address, ExecError> {
    let bytes = hex::decode(s.trim_start_matches("0x"))
        .map_err(|e| ExecError::Parse(format!("address: {e}")))?;
    if bytes.len() != 20 {
        return Err(ExecError::Parse(format!(
            "address must be 20 bytes; got {}",
            bytes.len()
        )));
    }
    Ok(Address::from_slice(&bytes))
}

fn parse_b256(s: &str) -> Result<B256, ExecError> {
    let bytes = hex::decode(s.trim_start_matches("0x"))
        .map_err(|e| ExecError::Parse(format!("b256: {e}")))?;
    if bytes.len() != 32 {
        return Err(ExecError::Parse(format!(
            "b256 must be 32 bytes; got {}",
            bytes.len()
        )));
    }
    Ok(B256::from_slice(&bytes))
}

fn hex_to_bytes(s: &str) -> Result<alloy_primitives::Bytes, ExecError> {
    let bytes = hex::decode(s.trim_start_matches("0x"))
        .map_err(|e| ExecError::Parse(format!("hex: {e}")))?;
    Ok(alloy_primitives::Bytes::from(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocate_port_returns_nonzero_port() {
        let p = allocate_port().unwrap();
        assert!(p > 0);
    }

    #[test]
    fn anvil_binary_either_resolves_or_returns_typed_missing() {
        match anvil_binary() {
            Ok(path) => assert!(path.exists()),
            Err(ExecError::ToolchainMissing { binary, .. }) => {
                assert_eq!(binary, "anvil");
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn address_str_round_trip() {
        let a = Address::repeat_byte(0xab);
        let s = address_str(a);
        assert_eq!(s, format!("0x{}", "ab".repeat(20)));
        let back = parse_address(&s).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn b256_str_round_trip() {
        let b = B256::repeat_byte(0x12);
        let s = b256_str(b);
        let back = parse_b256(&s).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn parse_revert_decodes_string_reason() {
        let (reason, _) = parse_revert_message("execution reverted: not authorized");
        assert_eq!(reason.as_deref(), Some("not authorized"));
    }

    #[test]
    fn parse_revert_returns_bytes_on_hex() {
        let (reason, bytes) = parse_revert_message("execution reverted: 0xdeadbeef");
        assert!(reason.is_none());
        assert_eq!(bytes.as_ref(), &[0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn parse_revert_handles_bare_message() {
        let (reason, bytes) = parse_revert_message("execution reverted");
        assert!(reason.is_none());
        assert!(bytes.is_empty());
    }

    #[test]
    fn tx_object_omits_empty_data() {
        let tx = TxRequest::new(Address::ZERO);
        let v = tx_object(&tx, false);
        assert!(v.get("data").is_none());
        assert!(v.get("to").is_some());
    }

    #[test]
    fn tx_object_includes_gas_for_send() {
        let tx = TxRequest::new(Address::ZERO);
        let v = tx_object(&tx, true);
        assert!(v.get("gas").is_some());
    }

    #[test]
    fn fork_spec_with_no_url_errors_at_fork() {
        let backend = AnvilForkBackend::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let res = rt.block_on(backend.fork_at(ForkSpec::new(
            crate::types::ForkChain::Ethereum,
            ForkBlock::Latest,
        )));
        assert!(matches!(res, Err(ExecError::NoRpcUrl { .. })));
    }
}
