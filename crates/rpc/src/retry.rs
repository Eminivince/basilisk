//! Exponential-backoff retry helper used by RPC providers.
//!
//! 3 attempts total, base delay 500 ms, doubling each attempt. Only retries
//! on errors for which [`RpcError::is_transient`] is true.

use std::{future::Future, time::Duration};

use crate::error::RpcError;

/// Maximum attempts including the initial call.
pub const MAX_ATTEMPTS: u32 = 3;
/// Base delay between retries (doubled per attempt).
pub const BASE_DELAY: Duration = Duration::from_millis(500);

/// Run `op` with bounded retries on transient errors.
pub async fn with_retry<F, Fut, T>(mut op: F) -> Result<T, RpcError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, RpcError>>,
{
    let mut delay = BASE_DELAY;
    for attempt in 1..=MAX_ATTEMPTS {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) if e.is_transient() && attempt < MAX_ATTEMPTS => {
                tracing::warn!(
                    attempt,
                    max_attempts = MAX_ATTEMPTS,
                    error = %e,
                    "RPC transient error; retrying",
                );
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2);
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("loop body either returns or sleeps and retries");
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    };

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn succeeds_on_first_try() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let out: Result<u32, _> = with_retry(|| {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(7)
            }
        })
        .await;
        assert_eq!(out.unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retries_on_transient_then_succeeds() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let out: Result<u32, _> = with_retry(|| {
            let c = c.clone();
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Err(RpcError::Transient("nope".into()))
                } else {
                    Ok(42)
                }
            }
        })
        .await;
        assert_eq!(out.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_after_max_attempts() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let out: Result<u32, _> = with_retry(|| {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(RpcError::Transient("never".into()))
            }
        })
        .await;
        assert!(matches!(out, Err(RpcError::Transient(_))));
        assert_eq!(calls.load(Ordering::SeqCst), MAX_ATTEMPTS);
    }

    #[tokio::test(start_paused = true)]
    async fn does_not_retry_on_permanent_error() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let out: Result<u32, _> = with_retry(|| {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(RpcError::Server("fatal".into()))
            }
        })
        .await;
        assert!(matches!(out, Err(RpcError::Server(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limited_is_transient() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let _out: Result<u32, _> = with_retry(|| {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(RpcError::RateLimited)
            }
        })
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), MAX_ATTEMPTS);
    }
}
