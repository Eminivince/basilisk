//! Error types for the execution backend.

use std::time::Duration;

use thiserror::Error;

/// Every failure mode an [`ExecutionBackend`](crate::ExecutionBackend) /
/// [`Fork`](crate::Fork) implementation can produce. Callers usually
/// surface these into [`ToolResult`](basilisk-agent::ToolResult) errors
/// — the agent decides whether to retry or work around.
#[derive(Debug, Error)]
pub enum ExecError {
    /// `forge` / `anvil` (or another required external binary) is not
    /// installed or not on `$PATH`. The hint is operator-readable.
    #[error("toolchain not found: {binary} ({install_hint})")]
    ToolchainMissing {
        binary: &'static str,
        install_hint: &'static str,
    },

    /// The configured upstream RPC URL is missing or empty. Forking
    /// requires a real archive RPC — there's no sane fallback.
    #[error("no RPC URL configured for {chain}: set MAINNET_RPC_URL, RPC_URL_<CHAIN>, or ALCHEMY_API_KEY")]
    NoRpcUrl { chain: String },

    /// We couldn't launch the subprocess. Carries the OS error.
    #[error("failed to spawn {binary}: {source}")]
    SpawnFailed {
        binary: &'static str,
        #[source]
        source: std::io::Error,
    },

    /// Subprocess started but didn't accept connections within the
    /// startup window. Usually means the upstream RPC rejected our
    /// fork request (rate-limited / wrong block / wrong chain).
    #[error("{binary} did not become ready within {timeout:?}")]
    StartupTimeout {
        binary: &'static str,
        timeout: Duration,
    },

    /// Subprocess exited unexpectedly during a call.
    #[error("{binary} exited unexpectedly: status={status:?}, stderr={stderr}")]
    ProcessExited {
        binary: &'static str,
        status: Option<i32>,
        stderr: String,
    },

    /// Couldn't allocate a free localhost port for the fork.
    #[error("failed to allocate local port: {0}")]
    PortAllocation(String),

    /// The upstream RPC returned a JSON-RPC error (e.g. block not
    /// found, method not supported).
    #[error("rpc error ({code}): {message}")]
    RpcError { code: i64, message: String },

    /// Transport-layer error talking to anvil or the upstream.
    #[error("network error: {0}")]
    Network(String),

    /// Couldn't deserialize the response.
    #[error("parse error: {0}")]
    Parse(String),

    /// `forge test` ran but the test scaffolding itself failed (compile
    /// error in the user's `PoC`, missing remappings, etc.). Distinct
    /// from a *test* failure, which surfaces inside
    /// [`ForgeTestResult`](crate::ForgeTestResult).
    #[error("forge build/setup failed: {0}")]
    ForgeSetup(String),

    /// Operation isn't supported by this backend variant. Mostly used
    /// by the future revm-only backend to refuse foundry-test calls.
    #[error("operation not supported by this backend: {0}")]
    Unsupported(&'static str),

    /// Catch-all for anything else.
    #[error("{0}")]
    Other(String),
}

impl ExecError {
    /// `true` when retrying after a backoff has a reasonable chance.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::StartupTimeout { .. } | Self::Network(_) | Self::PortAllocation(_)
        )
    }
}

impl From<reqwest::Error> for ExecError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            Self::Network(format!("timeout: {e}"))
        } else if e.is_connect() {
            Self::Network(format!("connect: {e}"))
        } else {
            Self::Network(e.to_string())
        }
    }
}

impl From<serde_json::Error> for ExecError {
    fn from(e: serde_json::Error) -> Self {
        Self::Parse(e.to_string())
    }
}
