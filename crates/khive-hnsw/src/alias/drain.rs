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
#[path = "drain/tests.rs"]
mod tests;
