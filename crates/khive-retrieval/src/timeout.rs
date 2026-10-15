//! Timeout and cancellation support for search operations.

use std::future::Future;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::error::{Result, RetrievalError};

/// Execute a search future with a timeout; returns [`RetrievalError::QueryTimeout`] if elapsed.
pub async fn search_with_timeout<F, T>(future: F, duration: Duration) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    match tokio::time::timeout(duration, future).await {
        Ok(result) => result,
        Err(_elapsed) => {
            // as-cast is safe for all realistic timeout values (u64::MAX ms ≈ 585M years).
            debug_assert!(
                duration.as_millis() <= u64::MAX as u128,
                "timeout duration overflows u64 milliseconds"
            );
            Err(RetrievalError::QueryTimeout {
                elapsed_ms: duration.as_millis() as u64,
            })
        }
    }
}

/// Execute a search future with an optional timeout (`None` = no timeout).
pub async fn search_with_optional_timeout<F, T>(future: F, timeout: Option<Duration>) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    match timeout {
        Some(duration) => search_with_timeout(future, duration).await,
        None => future.await,
    }
}

/// Execute a search future with a cancellation token; returns `QueryCancelled` if token fires first.
pub async fn search_with_cancellation<F, T>(future: F, token: CancellationToken) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    tokio::select! {
        result = future => result,
        _ = token.cancelled() => Err(RetrievalError::QueryCancelled),
    }
}

/// Execute a search future with optional timeout and optional cancellation token.
pub async fn search_with_deadline<F, T>(
    future: F,
    timeout: Option<Duration>,
    cancel: Option<CancellationToken>,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    match (timeout, cancel) {
        (Some(duration), Some(token)) => {
            tokio::select! {
                result = tokio::time::timeout(duration, future) => {
                    match result {
                        Ok(inner) => inner,
                        Err(_elapsed) => {
                            debug_assert!(
                                duration.as_millis() <= u64::MAX as u128,
                                "timeout duration overflows u64 milliseconds"
                            );
                            Err(RetrievalError::QueryTimeout {
                                elapsed_ms: duration.as_millis() as u64,
                            })
                        }
                    }
                }
                _ = token.cancelled() => Err(RetrievalError::QueryCancelled),
            }
        }
        (Some(duration), None) => search_with_timeout(future, duration).await,
        (None, Some(token)) => search_with_cancellation(future, token).await,
        (None, None) => future.await,
    }
}

/// Serde support for `Option<Duration>` as milliseconds.
///
/// Serializes `Duration` as `u64` milliseconds for JSON compatibility.
pub(crate) mod serde_opt_duration {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    /// Intermediate representation for serde.
    #[derive(Serialize, Deserialize)]
    struct DurationMs(u64);

    /// Serialize `Option<Duration>` as optional milliseconds.
    pub fn serialize<S>(value: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(d) => {
                // as-cast is safe for all realistic timeout values (u64::MAX ms ≈ 585M years).
                debug_assert!(
                    d.as_millis() <= u64::MAX as u128,
                    "timeout duration overflows u64 milliseconds"
                );
                DurationMs(d.as_millis() as u64).serialize(serializer)
            }
            None => serializer.serialize_none(),
        }
    }

    /// Deserialize `Option<Duration>` from optional milliseconds.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt: Option<DurationMs> = Option::deserialize(deserializer)?;
        Ok(opt.map(|ms| Duration::from_millis(ms.0)))
    }
}

#[cfg(test)]
#[path = "timeout/tests.rs"]
mod tests;
