//! Timeout and cancellation support for search operations.
//!
//! Provides utilities for wrapping search futures with timeout and cancellation
//! semantics. Uses `tokio::time::timeout` for deadline enforcement and
//! `tokio_util::sync::CancellationToken` for cooperative cancellation.
//!
//! # Design
//!
//! Timeout and cancellation are applied at the search entry points (hybrid search,
//! graph traversal) rather than at every internal function call. This keeps the
//! internal algorithms clean while providing operational safety at the boundaries.
//!
//! # Usage
//!
//! ```rust,ignore
//! use std::time::Duration;
//! use khive_retrieval::timeout::{search_with_timeout, search_with_cancellation};
//! use tokio_util::sync::CancellationToken;
//!
//! // Timeout: cancel if search takes longer than 5 seconds
//! let results = search_with_timeout(
//!     searcher.hybrid_search(&query, &config),
//!     Duration::from_secs(5),
//! ).await?;
//!
//! // Cancellation: cancel via token (e.g., from a request handler)
//! let token = CancellationToken::new();
//! let results = search_with_cancellation(
//!     searcher.hybrid_search(&query, &config),
//!     token.clone(),
//! ).await?;
//!
//! // From another task:
//! token.cancel();
//! ```
//!
//! See also: [`HybridConfig::timeout`] for declarative timeout configuration.

use std::future::Future;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::error::{Result, RetrievalError};

/// Execute a search future with a timeout.
///
/// Wraps the given future with `tokio::time::timeout`. If the future does not
/// complete within the specified duration, returns [`RetrievalError::QueryTimeout`].
///
/// # Arguments
///
/// * `future` - The search operation to execute
/// * `duration` - Maximum time to wait for completion
///
/// # Returns
///
/// The search result if completed within the timeout, or `QueryTimeout` error.
///
/// # Example
///
/// ```rust,ignore
/// use std::time::Duration;
/// use khive_retrieval::timeout::search_with_timeout;
///
/// let results = search_with_timeout(
///     searcher.hybrid_search(&query, &config),
///     Duration::from_secs(5),
/// ).await?;
/// ```
pub async fn search_with_timeout<F, T>(future: F, duration: Duration) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    match tokio::time::timeout(duration, future).await {
        Ok(result) => result,
        Err(_elapsed) => Err(RetrievalError::QueryTimeout {
            elapsed_ms: duration.as_millis() as u64,
        }),
    }
}

/// Execute a search future with an optional timeout.
///
/// If `timeout` is `Some`, wraps the future with [`search_with_timeout`].
/// If `None`, executes the future directly without timeout.
///
/// This is a convenience function for use with [`HybridConfig::timeout`].
///
/// # Arguments
///
/// * `future` - The search operation to execute
/// * `timeout` - Optional maximum time to wait
///
/// # Returns
///
/// The search result, or `QueryTimeout` if the timeout elapsed.
pub async fn search_with_optional_timeout<F, T>(future: F, timeout: Option<Duration>) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    match timeout {
        Some(duration) => search_with_timeout(future, duration).await,
        None => future.await,
    }
}

/// Execute a search future with a cancellation token.
///
/// Uses `tokio::select!` to race the search future against the cancellation token.
/// If the token is cancelled before the search completes, returns
/// [`RetrievalError::QueryCancelled`].
///
/// # Arguments
///
/// * `future` - The search operation to execute
/// * `token` - Cancellation token to observe
///
/// # Returns
///
/// The search result if completed before cancellation, or `QueryCancelled` error.
///
/// # Example
///
/// ```rust,ignore
/// use tokio_util::sync::CancellationToken;
/// use khive_retrieval::timeout::search_with_cancellation;
///
/// let token = CancellationToken::new();
/// let token_clone = token.clone();
///
/// // Spawn a task that cancels after 1 second
/// tokio::spawn(async move {
///     tokio::time::sleep(Duration::from_secs(1)).await;
///     token_clone.cancel();
/// });
///
/// let results = search_with_cancellation(
///     searcher.hybrid_search(&query, &config),
///     token,
/// ).await?;
/// ```
pub async fn search_with_cancellation<F, T>(future: F, token: CancellationToken) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    tokio::select! {
        result = future => result,
        _ = token.cancelled() => Err(RetrievalError::QueryCancelled),
    }
}

/// Execute a search future with both timeout and optional cancellation.
///
/// Combines timeout and cancellation into a single wrapper. The search will
/// be terminated if either:
/// - The timeout duration elapses (`QueryTimeout`)
/// - The cancellation token is triggered (`QueryCancelled`)
/// - The search completes normally
///
/// # Arguments
///
/// * `future` - The search operation to execute
/// * `timeout` - Optional maximum time to wait
/// * `cancel` - Optional cancellation token to observe
///
/// # Returns
///
/// The search result, or an appropriate error if timed out or cancelled.
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
                        Err(_elapsed) => Err(RetrievalError::QueryTimeout {
                            elapsed_ms: duration.as_millis() as u64,
                        }),
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
            Some(d) => DurationMs(d.as_millis() as u64).serialize(serializer),
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
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_search_with_timeout_completes() {
        // A future that completes immediately
        let future = async { Ok::<_, RetrievalError>(vec![1, 2, 3]) };
        let result = search_with_timeout(future, Duration::from_secs(5)).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn test_search_with_timeout_expires() {
        // A future that takes too long
        let future = async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok::<_, RetrievalError>(vec![1, 2, 3])
        };
        let result = search_with_timeout(future, Duration::from_millis(50)).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, RetrievalError::QueryTimeout { .. }));
        assert!(err.is_transient());
    }

    #[tokio::test]
    async fn test_search_with_timeout_propagates_error() {
        // A future that fails with a different error
        let future = async { Err::<Vec<i32>, _>(RetrievalError::invalid_query("bad query")) };
        let result = search_with_timeout(future, Duration::from_secs(5)).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            RetrievalError::InvalidQuery(_)
        ));
    }

    #[tokio::test]
    async fn test_search_with_optional_timeout_none() {
        // No timeout means direct execution
        let future = async { Ok::<_, RetrievalError>(42) };
        let result = search_with_optional_timeout(future, None).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn test_search_with_optional_timeout_some() {
        // With timeout, same as search_with_timeout
        let future = async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok::<_, RetrievalError>(42)
        };
        let result = search_with_optional_timeout(future, Some(Duration::from_millis(50))).await;
        assert!(matches!(
            result.unwrap_err(),
            RetrievalError::QueryTimeout { .. }
        ));
    }

    #[tokio::test]
    async fn test_search_with_cancellation_completes() {
        let token = CancellationToken::new();
        let future = async { Ok::<_, RetrievalError>(vec![1, 2, 3]) };
        let result = search_with_cancellation(future, token).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_search_with_cancellation_cancelled() {
        let token = CancellationToken::new();
        let token_clone = token.clone();

        // Cancel immediately
        token_clone.cancel();

        let future = async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok::<_, RetrievalError>(vec![1, 2, 3])
        };
        let result = search_with_cancellation(future, token).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, RetrievalError::QueryCancelled));
        assert!(err.is_transient());
    }

    #[tokio::test]
    async fn test_search_with_cancellation_delayed() {
        let token = CancellationToken::new();
        let token_clone = token.clone();

        // Cancel after a short delay
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            token_clone.cancel();
        });

        let future = async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok::<_, RetrievalError>(vec![1, 2, 3])
        };
        let result = search_with_cancellation(future, token).await;
        assert!(matches!(
            result.unwrap_err(),
            RetrievalError::QueryCancelled
        ));
    }

    #[tokio::test]
    async fn test_search_with_deadline_timeout_and_cancel() {
        let token = CancellationToken::new();

        // Timeout fires first (50ms vs 10s sleep)
        let future = async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok::<_, RetrievalError>(42)
        };
        let result =
            search_with_deadline(future, Some(Duration::from_millis(50)), Some(token)).await;
        assert!(matches!(
            result.unwrap_err(),
            RetrievalError::QueryTimeout { .. }
        ));
    }

    #[tokio::test]
    async fn test_search_with_deadline_cancel_fires_first() {
        let token = CancellationToken::new();
        let token_clone = token.clone();

        // Cancel immediately, timeout is long
        token_clone.cancel();

        let future = async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok::<_, RetrievalError>(42)
        };
        let result = search_with_deadline(future, Some(Duration::from_secs(60)), Some(token)).await;
        assert!(matches!(
            result.unwrap_err(),
            RetrievalError::QueryCancelled
        ));
    }

    #[tokio::test]
    async fn test_search_with_deadline_neither() {
        // No timeout, no cancellation: direct execution
        let future = async { Ok::<_, RetrievalError>(42) };
        let result = search_with_deadline(future, None, None).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn test_timeout_error_display() {
        let err = RetrievalError::query_timeout(5000);
        assert_eq!(err.to_string(), "query timed out after 5000ms");
    }

    #[tokio::test]
    async fn test_cancelled_error_display() {
        let err = RetrievalError::query_cancelled();
        assert_eq!(err.to_string(), "query cancelled");
    }

    #[tokio::test]
    async fn test_timeout_error_is_transient() {
        assert!(RetrievalError::query_timeout(100).is_transient());
        assert!(RetrievalError::query_cancelled().is_transient());
        assert!(!RetrievalError::query_timeout(100).is_permanent());
        assert!(!RetrievalError::query_cancelled().is_permanent());
    }

    #[test]
    fn test_serde_opt_duration_roundtrip() {
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        struct TestConfig {
            #[serde(
                default,
                skip_serializing_if = "Option::is_none",
                with = "super::serde_opt_duration"
            )]
            timeout: Option<Duration>,
        }

        // With timeout
        let config = TestConfig {
            timeout: Some(Duration::from_millis(5000)),
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("5000"));
        let restored: TestConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.timeout, Some(Duration::from_millis(5000)));

        // Without timeout
        let config = TestConfig { timeout: None };
        let json = serde_json::to_string(&config).unwrap();
        assert!(!json.contains("timeout"));
        let restored: TestConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(restored.timeout, None);
    }
}
