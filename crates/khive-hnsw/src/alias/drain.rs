//! RAII `ReaderGuard` and drain polling for zero-downtime HNSW index swaps.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::error::AliasError;
use crate::HnswIndex;

/// Tracks active readers for a single collection.
///
/// The counter is shared (via `Arc`) between the alias manager and all
/// outstanding `ReaderGuard`s. When the alias manager wants to drain a
/// collection, it polls this counter.
#[derive(Debug)]
pub(crate) struct ReaderCounter {
    count: AtomicU64,
}

impl ReaderCounter {
    pub fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
        }
    }

    /// Increment the reader count. Returns the previous value.
    #[inline]
    pub fn acquire(&self) -> u64 {
        self.count.fetch_add(1, Ordering::Acquire)
    }

    /// Decrement the reader count. Returns the previous value.
    #[inline]
    pub fn release(&self) -> u64 {
        self.count.fetch_sub(1, Ordering::Release)
    }

    /// Get the current reader count.
    #[inline]
    pub fn load(&self) -> u64 {
        self.count.load(Ordering::Acquire)
    }
}

/// RAII guard holding an index snapshot; decrements reader count on drop.
/// Index stays alive for the guard lifetime even if the alias is swapped.
pub struct ReaderGuard {
    /// Snapshot of the index at the time the guard was acquired.
    index: Arc<HnswIndex>,
    /// Reader counter to decrement on drop.
    counter: Arc<ReaderCounter>,
}

impl ReaderGuard {
    /// Create a new reader guard, incrementing the reader counter.
    pub(crate) fn new(index: Arc<HnswIndex>, counter: Arc<ReaderCounter>) -> Self {
        counter.acquire();
        Self { index, counter }
    }

    /// Get a reference to the index snapshot.
    pub fn index(&self) -> &HnswIndex {
        &self.index
    }
}

impl Drop for ReaderGuard {
    fn drop(&mut self) {
        self.counter.release();
    }
}

impl std::ops::Deref for ReaderGuard {
    type Target = HnswIndex;

    fn deref(&self) -> &Self::Target {
        &self.index
    }
}

/// Poll `counter` until zero readers remain or timeout is exceeded.
pub(crate) async fn drain_readers(
    counter: &Arc<ReaderCounter>,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<(), AliasError> {
    let start = Instant::now();

    loop {
        let active = counter.load();
        if active == 0 {
            return Ok(());
        }

        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return Err(AliasError::DrainTimeout {
                elapsed,
                timeout,
                active_readers: active,
            });
        }

        // Sleep for the poll interval (or remaining time, whichever is shorter)
        let remaining = timeout - elapsed;
        let sleep_dur = poll_interval.min(remaining);
        tokio::time::sleep(sleep_dur).await;
    }
}

#[cfg(test)]
mod tests {
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
}
