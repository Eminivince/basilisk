//! Global fork registry — tracks every spawned [`Fork`] so signal
//! handlers can shut them down cleanly on `Ctrl-C` / `SIGTERM`.
//!
//! Set 9.5 / CP9.5.5. Set 9 relied on `Drop` to reap anvil
//! subprocesses, which works under normal scope exit but leaves
//! processes behind on signal-driven termination — the spec called a
//! leaked anvil "an incident." This module gives the CLI a hook to
//! enumerate live forks at signal time and call `shutdown()` on each
//! before re-raising the signal for normal process termination.
//!
//! The registry stores [`Weak<dyn Fork>`](std::sync::Weak) so dropping
//! a fork doesn't keep it alive in the registry. Stale entries are
//! gc'd opportunistically on every `register` and on `shutdown_all`.

use std::sync::{LazyLock, Mutex, Weak};

use crate::backend::Fork;

/// Process-wide singleton tracking spawned forks. Use
/// [`GLOBAL_FORK_REGISTRY`] to access it.
pub struct ForkRegistry {
    forks: Mutex<Vec<Weak<dyn Fork>>>,
}

impl ForkRegistry {
    fn new() -> Self {
        Self {
            forks: Mutex::new(Vec::new()),
        }
    }

    /// Add a freshly-spawned fork. Caller passes a strong handle;
    /// the registry keeps a weak ref so it doesn't extend the
    /// fork's lifetime.
    pub fn register(&self, fork: &std::sync::Arc<dyn Fork>) {
        let mut guard = match self.forks.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.push(std::sync::Arc::downgrade(fork));
        // Opportunistic GC of dropped forks.
        guard.retain(|w| w.strong_count() > 0);
    }

    /// Snapshot the live (still-strong) forks. Caller iterates and
    /// calls `shutdown()` on each.
    pub fn live(&self) -> Vec<std::sync::Arc<dyn Fork>> {
        let guard = match self.forks.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.iter().filter_map(Weak::upgrade).collect()
    }

    /// Iterate every live fork and call `shutdown()` sequentially.
    /// Errors are logged via `tracing::warn` and otherwise swallowed
    /// — this is signal-handler / cleanup territory, not user-facing.
    pub async fn shutdown_all(&self) -> usize {
        let live = self.live();
        let count = live.len();
        for fork in live {
            if let Err(e) = fork.shutdown().await {
                tracing::warn!(error = %e, fork = %fork.id(), "fork shutdown errored");
            }
        }
        // Drop dead refs after shutdown.
        if let Ok(mut guard) = self.forks.lock() {
            guard.retain(|w| w.strong_count() > 0);
        }
        count
    }

    /// Number of registered (potentially-stale) entries. Mostly for
    /// tests.
    pub fn len(&self) -> usize {
        self.forks.lock().map(|g| g.len()).unwrap_or(0)
    }

    /// `true` when the registry has no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Process-wide singleton. Every [`AnvilForkBackend`](crate::AnvilForkBackend)
/// `fork_at` call registers its result here automatically.
pub static GLOBAL_FORK_REGISTRY: LazyLock<ForkRegistry> = LazyLock::new(ForkRegistry::new);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::ExecutionBackend;
    use crate::{ForkBlock, ForkChain, ForkSpec, MockExecutionBackend};
    use std::sync::Arc;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Runtime::new().unwrap().block_on(f)
    }

    #[test]
    fn register_then_live_returns_strong_ref() {
        let reg = ForkRegistry::new();
        let backend = MockExecutionBackend::new();
        let fork: Arc<dyn Fork> =
            block_on(backend.fork_at(ForkSpec::new(ForkChain::Ethereum, ForkBlock::Latest)))
                .unwrap();
        reg.register(&fork);
        assert_eq!(reg.live().len(), 1);
    }

    #[test]
    fn weak_refs_drop_when_fork_drops() {
        let reg = ForkRegistry::new();
        let backend = MockExecutionBackend::new();
        {
            let fork: Arc<dyn Fork> =
                block_on(backend.fork_at(ForkSpec::new(ForkChain::Ethereum, ForkBlock::Latest)))
                    .unwrap();
            reg.register(&fork);
            assert_eq!(reg.live().len(), 1);
            // fork drops at end of scope
        }
        assert_eq!(reg.live().len(), 0);
    }

    #[test]
    fn shutdown_all_calls_shutdown_on_every_live_fork() {
        let reg = ForkRegistry::new();
        let backend = MockExecutionBackend::new();
        let f1: Arc<dyn Fork> =
            block_on(backend.fork_at(ForkSpec::new(ForkChain::Ethereum, ForkBlock::Latest)))
                .unwrap();
        let f2: Arc<dyn Fork> =
            block_on(backend.fork_at(ForkSpec::new(ForkChain::Ethereum, ForkBlock::Latest)))
                .unwrap();
        reg.register(&f1);
        reg.register(&f2);
        let n = block_on(reg.shutdown_all());
        assert_eq!(n, 2);
    }

    #[test]
    fn register_gcs_dropped_entries() {
        let reg = ForkRegistry::new();
        let backend = MockExecutionBackend::new();
        {
            let f: Arc<dyn Fork> =
                block_on(backend.fork_at(ForkSpec::new(ForkChain::Ethereum, ForkBlock::Latest)))
                    .unwrap();
            reg.register(&f);
        }
        // Now register a new one — GC should prune the dropped one.
        let f2: Arc<dyn Fork> =
            block_on(backend.fork_at(ForkSpec::new(ForkChain::Ethereum, ForkBlock::Latest)))
                .unwrap();
        reg.register(&f2);
        assert_eq!(reg.len(), 1);
    }
}
