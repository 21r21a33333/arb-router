//! Retry an async operation with exponential backoff.
//!
//! Transient RPC failures (timeouts, rate limits, brief disconnects) are common;
//! wrapping a read in a bounded exponential-backoff retry smooths them over
//! without unbounded blocking.

use std::future::Future;
use std::time::Duration;

/// Cap on the backoff exponent so the delay multiplier can never overflow.
const MAX_EXPONENT: u32 = 16;

/// Run `op` until it succeeds or `max_attempts` is reached, doubling the delay
/// after each failure (`base_delay`, `2·base_delay`, `4·base_delay`, …). Returns
/// the last error if every attempt fails. `max_attempts` of 0 is treated as 1.
pub async fn retry_with_backoff<T, E, F, Fut>(
    max_attempts: usize,
    base_delay: Duration,
    mut op: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let attempts = max_attempts.max(1);
    let mut attempt = 0;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                attempt += 1;
                match attempt >= attempts {
                    true => return Err(err),
                    false => {
                        let factor = 2u32.saturating_pow((attempt as u32 - 1).min(MAX_EXPONENT));
                        tokio::time::sleep(base_delay.saturating_mul(factor)).await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Fails once, then succeeds — the retry must return the eventual `Ok`.
    #[tokio::test]
    async fn retries_then_succeeds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let result: Result<u32, &str> = retry_with_backoff(3, Duration::ZERO, || {
            let seen = seen.clone();
            async move {
                match seen.fetch_add(1, Ordering::SeqCst) {
                    0 => Err("transient"),
                    _ => Ok(42),
                }
            }
        })
        .await;
        assert_eq!(result, Ok(42));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// Always fails — the retry must give up after exactly `max_attempts` and
    /// surface the last error.
    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let result: Result<u32, &str> = retry_with_backoff(3, Duration::ZERO, || {
            let seen = seen.clone();
            async move {
                seen.fetch_add(1, Ordering::SeqCst);
                Err::<u32, &str>("always")
            }
        })
        .await;
        assert_eq!(result, Err("always"));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
