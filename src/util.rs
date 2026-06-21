use std::future::Future;
use std::time::Duration;

/// Outcome of [`retry_with_backoff`].
#[derive(Debug)]
pub enum RetryOutcome<T> {
    /// The operation succeeded (possibly after retries).
    Success(T),
    /// `should_retry` returned `false` after a failure, stopping the loop
    /// early. Carries the last error.
    Aborted(anyhow::Error),
    /// Every attempt failed. Carries the last error.
    Exhausted(anyhow::Error),
}

/// Retries an async operation on failure, sleeping for the next entry in
/// `delays` between attempts (`delays.len() + 1` attempts total).
///
/// After each failed attempt's sleep, `should_retry` is consulted; returning
/// `false` stops the loop early with [`RetryOutcome::Aborted`]. Callers use
/// this to bail out when another code path already achieved the operation's
/// goal while this loop was sleeping (in which case re-running the operation
/// would not be a harmless no-op).
pub async fn retry_with_backoff<T, F, Fut, C>(
    operation: &str,
    delays: &[Duration],
    mut should_retry: C,
    mut op: F,
) -> RetryOutcome<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
    C: FnMut() -> bool,
{
    let max_attempts = delays.len() + 1;
    let mut attempt = 1usize;
    loop {
        match op().await {
            Ok(value) => {
                if attempt > 1 {
                    tracing::info!(operation, attempt, "Operation succeeded after retries");
                }
                return RetryOutcome::Success(value);
            }
            Err(e) => {
                let Some(delay) = delays.get(attempt - 1) else {
                    tracing::warn!(
                        operation,
                        attempt,
                        error = %e,
                        "Operation failed; retries exhausted"
                    );
                    return RetryOutcome::Exhausted(e);
                };
                tracing::warn!(
                    operation,
                    attempt,
                    max_attempts,
                    retry_in_seconds = delay.as_secs(),
                    error = %e,
                    "Operation failed; will retry"
                );
                tokio::time::sleep(*delay).await;
                if !should_retry() {
                    tracing::info!(
                        operation,
                        attempt,
                        "Stopping retries early; operation no longer needed"
                    );
                    return RetryOutcome::Aborted(e);
                }
                attempt += 1;
            }
        }
    }
}

pub fn strip_leading_whitespace(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < bytes.len()
        && (bytes[start] == b' '
            || bytes[start] == b'\t'
            || bytes[start] == b'\n'
            || bytes[start] == b'\r')
    {
        start += 1;
    }
    &bytes[start..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    type BoxedRetryFuture = std::pin::Pin<Box<dyn Future<Output = anyhow::Result<usize>> + Send>>;

    /// Returns an op closure that fails `failures` times, then succeeds,
    /// along with the attempt counter it increments.
    fn flaky_op(failures: usize) -> (impl FnMut() -> BoxedRetryFuture, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_in_op = calls.clone();
        let op = move || {
            let calls = calls_in_op.clone();
            Box::pin(async move {
                let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n <= failures {
                    Err(anyhow!("transient failure {n}"))
                } else {
                    Ok(n)
                }
            }) as BoxedRetryFuture
        };
        (op, calls)
    }

    const SHORT_DELAYS: &[Duration] = &[
        Duration::from_secs(1),
        Duration::from_secs(2),
        Duration::from_secs(4),
    ];

    #[tokio::test(start_paused = true)]
    async fn retry_first_attempt_success_never_sleeps_or_checks() {
        let (op, calls) = flaky_op(0);
        let outcome = retry_with_backoff(
            "test",
            SHORT_DELAYS,
            || panic!("should_retry must not be consulted when the op succeeds"),
            op,
        )
        .await;
        assert!(matches!(outcome, RetryOutcome::Success(1)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_recovers_after_transient_failures() {
        let (op, calls) = flaky_op(2);
        let outcome = retry_with_backoff("test", SHORT_DELAYS, || true, op).await;
        assert!(matches!(outcome, RetryOutcome::Success(3)));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_exhausts_after_all_attempts_fail() {
        let (op, calls) = flaky_op(usize::MAX);
        let outcome = retry_with_backoff("test", SHORT_DELAYS, || true, op).await;
        // delays.len() + 1 attempts, and the error is from the LAST attempt
        match outcome {
            RetryOutcome::Exhausted(e) => {
                assert_eq!(e.to_string(), "transient failure 4");
            }
            other => panic!("expected Exhausted, got {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), SHORT_DELAYS.len() + 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_aborts_when_should_retry_returns_false() {
        let (op, calls) = flaky_op(usize::MAX);
        let outcome = retry_with_backoff("test", SHORT_DELAYS, || false, op).await;
        match outcome {
            RetryOutcome::Aborted(e) => {
                assert_eq!(e.to_string(), "transient failure 1");
            }
            other => panic!("expected Aborted, got {other:?}"),
        }
        // Aborted after the first failure's sleep, before any second attempt.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_with_empty_delays_makes_single_attempt() {
        let (op, calls) = flaky_op(usize::MAX);
        let outcome = retry_with_backoff("test", &[], || true, op).await;
        assert!(matches!(outcome, RetryOutcome::Exhausted(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_empty_input() {
        assert_eq!(strip_leading_whitespace(b""), b"");
    }

    #[test]
    fn test_all_whitespace() {
        assert_eq!(strip_leading_whitespace(b"   \t\n\r"), b"");
    }

    #[test]
    fn test_leading_spaces() {
        assert_eq!(strip_leading_whitespace(b"   hello"), b"hello");
    }

    #[test]
    fn test_leading_tabs() {
        assert_eq!(strip_leading_whitespace(b"\t\thello"), b"hello");
    }

    #[test]
    fn test_leading_newlines() {
        assert_eq!(strip_leading_whitespace(b"\n\nhello"), b"hello");
    }

    #[test]
    fn test_leading_carriage_returns() {
        assert_eq!(strip_leading_whitespace(b"\r\rhello"), b"hello");
    }

    #[test]
    fn test_mixed_whitespace() {
        assert_eq!(
            strip_leading_whitespace(b" \t\n\rhello world"),
            b"hello world"
        );
    }

    #[test]
    fn test_no_leading_whitespace() {
        assert_eq!(strip_leading_whitespace(b"hello"), b"hello");
    }

    #[test]
    fn test_trailing_whitespace_preserved() {
        assert_eq!(strip_leading_whitespace(b"hello   "), b"hello   ");
    }
}
