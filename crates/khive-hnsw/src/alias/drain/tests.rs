use super::*;

#[test]
fn test_reader_counter_acquire_release() {
    let counter = ReaderCounter::new();
    assert_eq!(counter.load(), 0);

    counter.acquire();
    assert_eq!(counter.load(), 1);

    counter.acquire();
    assert_eq!(counter.load(), 2);

    counter.release();
    assert_eq!(counter.load(), 1);

    counter.release();
    assert_eq!(counter.load(), 0);
}

#[test]
fn test_reader_guard_decrements_on_drop() {
    let index = Arc::new(HnswIndex::new(4));
    let counter = Arc::new(ReaderCounter::new());

    {
        let _guard = ReaderGuard::new(Arc::clone(&index), Arc::clone(&counter));
        assert_eq!(counter.load(), 1);

        let _guard2 = ReaderGuard::new(Arc::clone(&index), Arc::clone(&counter));
        assert_eq!(counter.load(), 2);
    }
    // Both guards dropped
    assert_eq!(counter.load(), 0);
}

#[test]
fn test_reader_guard_deref() {
    let index = Arc::new(HnswIndex::new(8));
    let counter = Arc::new(ReaderCounter::new());
    let guard = ReaderGuard::new(Arc::clone(&index), Arc::clone(&counter));

    // Should be able to call HnswIndex methods via Deref
    assert_eq!(guard.len(), 0);
    assert!(guard.is_empty());
}

#[tokio::test]
async fn test_drain_readers_immediate() {
    let counter = Arc::new(ReaderCounter::new());
    // No readers -- drain should return immediately
    let result = drain_readers(
        &counter,
        Duration::from_millis(100),
        Duration::from_millis(10),
    )
    .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_drain_readers_timeout() {
    let counter = Arc::new(ReaderCounter::new());
    counter.acquire(); // Simulate an active reader that never finishes

    let result = drain_readers(
        &counter,
        Duration::from_millis(50),
        Duration::from_millis(10),
    )
    .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        AliasError::DrainTimeout { active_readers, .. } => {
            assert_eq!(active_readers, 1);
        }
        other => panic!("Expected DrainTimeout, got: {other:?}"),
    }

    // Clean up
    counter.release();
}

#[tokio::test]
async fn test_drain_readers_delayed_release() {
    let counter = Arc::new(ReaderCounter::new());
    counter.acquire();

    let counter_clone = Arc::clone(&counter);

    // Spawn a task that releases after 30ms
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        counter_clone.release();
    });

    // Drain with 200ms timeout -- should succeed after ~30ms
    let result = drain_readers(
        &counter,
        Duration::from_millis(200),
        Duration::from_millis(5),
    )
    .await;
    assert!(result.is_ok());
}
