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
    let result = search_with_deadline(future, Some(Duration::from_millis(50)), Some(token)).await;
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
