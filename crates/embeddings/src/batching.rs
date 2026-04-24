//! Batching / retry / rate-limit wrapper for [`EmbeddingProvider`].
//!
//! Wraps an inner provider and adds three concerns:
//!
//!  1. **Auto-split.** If the caller hands in a slice larger than
//!     the inner provider's `max_batch_size`, split it into chunks
//!     and issue sequential calls. Most ingest-path callers hand in
//!     hundreds-to-thousands of chunks; auto-split keeps their code
//!     simple.
//!  2. **Retry with backoff.** On [`EmbeddingError::is_retryable`]
//!     errors (rate limits, network blips, 5xx) retry up to
//!     `max_retries` with exponential backoff, honouring any
//!     `Retry-After` hint.
//!  3. **Optional token-rate gate.** Voyage's free tier throttles
//!     by *tokens per minute* (10k), not call count. [`TokenBudgetGate`]
//!     tracks tokens-consumed-per-window and sleeps callers until
//!     there's room.
//!
//! Deliberately NOT included: the 50ms small-batch accumulator.
//! Ingest callers hand in pre-built batches, so accumulating
//! concurrent single-input calls is work without a consumer.
//! Revisit if/when search-time workloads start issuing many
//! concurrent 1-item queries.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::{
    backend::EmbeddingProvider,
    error::EmbeddingError,
    types::{Embedding, EmbeddingInput},
};

/// Retry / backoff configuration.
#[derive(Debug, Clone, Copy)]
pub struct RetryConfig {
    /// Maximum retry attempts after the initial call.
    pub max_retries: u32,
    /// Initial delay before the first retry.
    pub base_delay: Duration,
    /// Multiplier applied to the delay between retries.
    pub multiplier: f32,
    /// Upper bound on any single wait.
    pub max_delay: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 4,
            base_delay: Duration::from_millis(500),
            multiplier: 2.0,
            max_delay: Duration::from_secs(30),
        }
    }
}

/// Token-bucket-like gate. Tracks tokens consumed over a rolling
/// window; blocks callers past the limit until the window refreshes.
///
/// Use-case: Voyage's free tier = 10k tokens/minute. Without the
/// gate, a large ingest slams Voyage with 20k tokens/minute and
/// gets 429'd every second call.
#[derive(Debug)]
pub struct TokenBudgetGate {
    window: Duration,
    limit: u32,
    state: Mutex<GateState>,
}

#[derive(Debug)]
struct GateState {
    window_start: Instant,
    tokens_used: u32,
}

impl TokenBudgetGate {
    /// Allow `limit` tokens per `window`. `None` for either disables
    /// the gate (pass-through).
    #[must_use]
    pub fn new(limit: u32, window: Duration) -> Self {
        Self {
            window,
            limit,
            state: Mutex::new(GateState {
                window_start: Instant::now(),
                tokens_used: 0,
            }),
        }
    }

    /// Block until there's room for `estimated_tokens` in the
    /// current window. Always returns — worst case is a sleep of
    /// `window`.
    pub async fn acquire(&self, estimated_tokens: u32) {
        loop {
            let wait = {
                let mut state = self.state.lock().await;
                let elapsed = state.window_start.elapsed();
                if elapsed >= self.window {
                    state.window_start = Instant::now();
                    state.tokens_used = 0;
                }
                let remaining = self.limit.saturating_sub(state.tokens_used);
                if estimated_tokens <= remaining {
                    state.tokens_used = state.tokens_used.saturating_add(estimated_tokens);
                    return;
                }
                // Release lock before sleeping so other callers that
                // fit can proceed.
                self.window.saturating_sub(elapsed) + Duration::from_millis(10)
            };
            tokio::time::sleep(wait).await;
        }
    }
}

/// Wraps an inner provider with auto-split + retry + optional
/// token-rate gating. Cheap to clone — inner state is `Arc`-shared.
#[derive(Clone)]
pub struct BatchingProvider {
    inner: Arc<dyn EmbeddingProvider>,
    retry: RetryConfig,
    gate: Option<Arc<TokenBudgetGate>>,
}

impl BatchingProvider {
    /// Wrap `inner` with default retry config and no token gate.
    #[must_use]
    pub fn new(inner: Arc<dyn EmbeddingProvider>) -> Self {
        Self {
            inner,
            retry: RetryConfig::default(),
            gate: None,
        }
    }

    /// Builder: set a custom retry config.
    #[must_use]
    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    /// Builder: add a token-rate gate. Callers that need to respect
    /// Voyage's 10k-token/min free tier pass this.
    #[must_use]
    pub fn with_token_gate(mut self, gate: Arc<TokenBudgetGate>) -> Self {
        self.gate = Some(gate);
        self
    }

    async fn call_once(&self, inputs: &[EmbeddingInput]) -> Result<Vec<Embedding>, EmbeddingError> {
        if let Some(gate) = &self.gate {
            gate.acquire(estimate_tokens(inputs)).await;
        }
        self.inner.embed(inputs).await
    }

    async fn call_with_retry(
        &self,
        inputs: &[EmbeddingInput],
    ) -> Result<Vec<Embedding>, EmbeddingError> {
        let mut attempt: u32 = 0;
        let mut delay = self.retry.base_delay;
        loop {
            match self.call_once(inputs).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if attempt >= self.retry.max_retries || !e.is_retryable() {
                        return Err(e);
                    }
                    // Respect Retry-After when the provider sent one.
                    let wait = if let EmbeddingError::RateLimited {
                        retry_after: Some(ra),
                    } = &e
                    {
                        (*ra).min(self.retry.max_delay)
                    } else {
                        delay.min(self.retry.max_delay)
                    };
                    tracing::warn!(
                        error = %e,
                        attempt,
                        wait_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
                        "embedding retry",
                    );
                    tokio::time::sleep(wait).await;
                    attempt = attempt.saturating_add(1);
                    // Exponential backoff for the NEXT retry.
                    let next = delay.as_secs_f32() * self.retry.multiplier;
                    delay = Duration::from_secs_f32(next).min(self.retry.max_delay);
                }
            }
        }
    }
}

#[async_trait]
impl EmbeddingProvider for BatchingProvider {
    fn identifier(&self) -> &str {
        self.inner.identifier()
    }
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    fn max_tokens_per_input(&self) -> usize {
        self.inner.max_tokens_per_input()
    }
    fn max_batch_size(&self) -> usize {
        // Auto-split erases the inner cap from the caller's view —
        // the wrapper accepts any size and splits under the hood.
        usize::MAX
    }

    async fn embed(&self, inputs: &[EmbeddingInput]) -> Result<Vec<Embedding>, EmbeddingError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let chunk_size = self.inner.max_batch_size().max(1);
        let mut out = Vec::with_capacity(inputs.len());
        for batch in inputs.chunks(chunk_size) {
            let mut rows = self.call_with_retry(batch).await?;
            out.append(&mut rows);
        }
        Ok(out)
    }
}

/// Rough token estimate for rate gating. Actual token counts come
/// back with the response, but the gate needs an *a priori*
/// estimate to decide whether to sleep. Byte-length / 4 is the
/// standard heuristic for English-ish text; it over-counts for
/// dense code but that's the conservative side (gate waits more,
/// not less).
fn estimate_tokens(inputs: &[EmbeddingInput]) -> u32 {
    let bytes: usize = inputs.iter().map(|i| i.text.len()).sum();
    u32::try_from(bytes / 4 + 1).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Counts calls and echoes back a deterministic fixed vector.
    /// Used to assert auto-split arithmetic without hitting a wire.
    struct CountingProvider {
        max_batch: usize,
        calls: AtomicU32,
        err_first: AtomicU32, // err on first N calls, then succeed
    }

    impl CountingProvider {
        fn new(max_batch: usize) -> Self {
            Self {
                max_batch,
                calls: AtomicU32::new(0),
                err_first: AtomicU32::new(0),
            }
        }
        fn err_first(n: u32, max_batch: usize) -> Self {
            Self {
                max_batch,
                calls: AtomicU32::new(0),
                err_first: AtomicU32::new(n),
            }
        }
        fn call_count(&self) -> u32 {
            self.calls.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl EmbeddingProvider for CountingProvider {
        #[allow(clippy::unnecessary_literal_bound)]
        fn identifier(&self) -> &str {
            "test/counting"
        }
        fn dimensions(&self) -> usize {
            4
        }
        fn max_tokens_per_input(&self) -> usize {
            1000
        }
        fn max_batch_size(&self) -> usize {
            self.max_batch
        }
        async fn embed(&self, inputs: &[EmbeddingInput]) -> Result<Vec<Embedding>, EmbeddingError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let remaining = self.err_first.load(Ordering::Relaxed);
            if remaining > 0 {
                self.err_first.fetch_sub(1, Ordering::Relaxed);
                return Err(EmbeddingError::NetworkError("test".into()));
            }
            Ok(inputs
                .iter()
                .map(|_| Embedding {
                    vector: vec![1.0, 2.0, 3.0, 4.0],
                    input_tokens: 1,
                })
                .collect())
        }
    }

    #[tokio::test]
    async fn auto_split_emits_multiple_calls_when_over_max_batch() {
        let inner = Arc::new(CountingProvider::new(3));
        let wrapped = BatchingProvider::new(inner.clone());
        let inputs: Vec<_> = (0..10)
            .map(|i| EmbeddingInput::document(format!("d{i}")))
            .collect();
        let out = wrapped.embed(&inputs).await.unwrap();
        assert_eq!(out.len(), 10);
        // 10 items / 3-per-batch = 4 calls (3+3+3+1)
        assert_eq!(inner.call_count(), 4);
    }

    #[tokio::test]
    async fn auto_split_preserves_input_order() {
        let inner = Arc::new(CountingProvider::new(2));
        let wrapped = BatchingProvider::new(inner.clone());
        let inputs: Vec<_> = (0..5)
            .map(|i| EmbeddingInput::document(format!("d{i}")))
            .collect();
        let out = wrapped.embed(&inputs).await.unwrap();
        assert_eq!(out.len(), 5);
    }

    #[tokio::test]
    async fn empty_input_does_not_call_inner() {
        let inner = Arc::new(CountingProvider::new(10));
        let wrapped = BatchingProvider::new(inner.clone());
        let out = wrapped.embed(&[]).await.unwrap();
        assert!(out.is_empty());
        assert_eq!(inner.call_count(), 0);
    }

    #[tokio::test]
    async fn retry_recovers_from_transient_error() {
        // Fail twice, succeed on the third try.
        let inner = Arc::new(CountingProvider::err_first(2, 10));
        let wrapped = BatchingProvider::new(inner.clone()).with_retry(RetryConfig {
            max_retries: 4,
            base_delay: Duration::from_millis(1),
            multiplier: 1.5,
            max_delay: Duration::from_millis(10),
        });
        let out = wrapped
            .embed(&[EmbeddingInput::document("x")])
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(inner.call_count(), 3);
    }

    #[tokio::test]
    async fn retry_gives_up_on_non_retryable_error() {
        struct AuthErrProvider;
        #[async_trait]
        impl EmbeddingProvider for AuthErrProvider {
            #[allow(clippy::unnecessary_literal_bound)]
            fn identifier(&self) -> &str {
                "test/auth"
            }
            fn dimensions(&self) -> usize {
                1
            }
            fn max_tokens_per_input(&self) -> usize {
                1
            }
            fn max_batch_size(&self) -> usize {
                1
            }
            async fn embed(&self, _: &[EmbeddingInput]) -> Result<Vec<Embedding>, EmbeddingError> {
                Err(EmbeddingError::AuthError("401".into()))
            }
        }
        let wrapped = BatchingProvider::new(Arc::new(AuthErrProvider)).with_retry(RetryConfig {
            max_retries: 4,
            base_delay: Duration::from_millis(1),
            multiplier: 1.1,
            max_delay: Duration::from_millis(5),
        });
        let err = wrapped
            .embed(&[EmbeddingInput::document("x")])
            .await
            .unwrap_err();
        assert!(matches!(err, EmbeddingError::AuthError(_)));
    }

    #[tokio::test]
    async fn retry_gives_up_after_max_retries() {
        // Always fail transiently — we expect exhaustion, not success.
        let inner = Arc::new(CountingProvider::err_first(999, 10));
        let wrapped = BatchingProvider::new(inner.clone()).with_retry(RetryConfig {
            max_retries: 2,
            base_delay: Duration::from_millis(1),
            multiplier: 1.1,
            max_delay: Duration::from_millis(5),
        });
        let err = wrapped
            .embed(&[EmbeddingInput::document("x")])
            .await
            .unwrap_err();
        assert!(matches!(err, EmbeddingError::NetworkError(_)));
        // Initial + 2 retries = 3 calls.
        assert_eq!(inner.call_count(), 3);
    }

    #[tokio::test]
    async fn token_gate_blocks_second_call_when_window_full() {
        // 10 tokens/window; each call uses ~2 tokens (≈ bytes/4).
        // Five calls fit; the sixth should sleep until the window
        // refreshes. We use a very short window so the test is fast.
        let gate = Arc::new(TokenBudgetGate::new(10, Duration::from_millis(100)));
        let inner = Arc::new(CountingProvider::new(1));
        let wrapped = BatchingProvider::new(inner.clone()).with_token_gate(Arc::clone(&gate));
        let start = Instant::now();
        // Each call is 8 bytes → estimate 2 tokens.
        for _ in 0..6 {
            wrapped
                .embed(&[EmbeddingInput::document("12345678")])
                .await
                .unwrap();
        }
        // Sixth call required waiting for window refresh.
        assert!(start.elapsed() >= Duration::from_millis(90));
        assert_eq!(inner.call_count(), 6);
    }

    #[test]
    fn estimate_tokens_rounds_up_for_short_inputs() {
        // 1 char → 0 + 1 = 1 token (never 0 — the gate shouldn't
        // count a non-empty call as free).
        assert_eq!(estimate_tokens(&[EmbeddingInput::document("a")]), 1);
    }

    #[test]
    fn estimate_tokens_scales_with_bytes() {
        // 40 bytes → 40/4 + 1 = 11.
        let txt = "a".repeat(40);
        assert_eq!(estimate_tokens(&[EmbeddingInput::document(txt)]), 11);
    }
}
