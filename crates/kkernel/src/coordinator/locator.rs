//! Bounded UUID-to-backend locator cache with TTL eviction.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use khive_runtime::BackendId;
use lru::LruCache;
use uuid::Uuid;

/// Default TTL for locator cache entries (5 minutes).
pub(super) const DEFAULT_LOCATOR_TTL: Duration = Duration::from_secs(300);
/// Default maximum number of locator cache entries.
pub(super) const DEFAULT_LOCATOR_CAPACITY: usize = 65_536;

struct LocatorEntry {
    backend_id: BackendId,
    inserted_at: Instant,
}

struct LocatorState {
    entries: LruCache<Uuid, LocatorEntry>,
    expirations: BTreeSet<(Instant, Uuid)>,
}

/// In-memory UUID-to-backend cache with TTL and LRU eviction.
pub struct LocatorCache {
    state: Mutex<LocatorState>,
    pub(super) ttl: Duration,
}

impl LocatorCache {
    /// Construct with the given TTL.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self::with_ttl_and_capacity(
            ttl,
            NonZeroUsize::new(DEFAULT_LOCATOR_CAPACITY)
                .expect("DEFAULT_LOCATOR_CAPACITY must be non-zero"),
        )
    }

    /// Construct with the given TTL and entry capacity.
    pub fn with_ttl_and_capacity(ttl: Duration, capacity: NonZeroUsize) -> Self {
        Self {
            state: Mutex::new(LocatorState {
                entries: LruCache::new(capacity),
                expirations: BTreeSet::new(),
            }),
            ttl,
        }
    }

    /// Construct with the default TTL (5 minutes).
    pub fn new() -> Self {
        Self::with_ttl(DEFAULT_LOCATOR_TTL)
    }

    /// Look up the backend that owns `id`.
    ///
    /// Returns `None` on a miss or when the entry has expired.
    /// Live hits refresh the LRU position without extending the entry's TTL.
    pub fn get(&self, id: Uuid) -> Option<BackendId> {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let is_live = state
            .entries
            .peek(&id)
            .is_some_and(|entry| now.saturating_duration_since(entry.inserted_at) < self.ttl);
        if !is_live {
            if let Some(entry) = state.entries.pop(&id) {
                state.expirations.remove(&(entry.inserted_at, id));
            }
            return None;
        }
        state.entries.get(&id).map(|entry| entry.backend_id.clone())
    }

    /// Remove the cache entry for `id`, if any.
    pub fn remove(&self, id: Uuid) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = state.entries.pop(&id) {
            state.expirations.remove(&(entry.inserted_at, id));
        }
    }

    /// Insert or refresh the owning backend for `id`, pruning expired entries first.
    pub fn insert(&self, id: Uuid, backend_id: BackendId) {
        self.insert_at(id, backend_id, Instant::now());
    }

    fn insert_at(&self, id: Uuid, backend_id: BackendId, now: Instant) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::purge_expired_at(&mut state, self.ttl, now);
        if let Some((replaced_id, replaced_entry)) = state.entries.push(
            id,
            LocatorEntry {
                backend_id,
                inserted_at: now,
            },
        ) {
            state
                .expirations
                .remove(&(replaced_entry.inserted_at, replaced_id));
        }
        state.expirations.insert((now, id));
    }

    /// Remove all entries whose TTL has elapsed.
    pub fn purge_expired(&self) {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::purge_expired_at(&mut state, self.ttl, now);
    }

    fn purge_expired_at(state: &mut LocatorState, ttl: Duration, now: Instant) {
        while state
            .expirations
            .first()
            .is_some_and(|(inserted_at, _)| now.saturating_duration_since(*inserted_at) >= ttl)
        {
            let (inserted_at, id) = state
                .expirations
                .pop_first()
                .expect("expiry checked through the same cache lock");
            let entry = state
                .entries
                .pop(&id)
                .expect("entry checked through the same cache lock");
            debug_assert_eq!(entry.inserted_at, inserted_at);
        }
    }

    /// Number of retained entries (including possibly-expired ones not yet purged).
    pub fn len(&self) -> usize {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.entries.len()
    }

    /// True if the cache has no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for LocatorCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insertion_prunes_expired_entries_without_capacity_pressure() {
        let ttl = Duration::from_secs(10);
        let cache = LocatorCache::with_ttl_and_capacity(ttl, NonZeroUsize::new(3).unwrap());
        let start = Instant::now();
        let expired = Uuid::new_v4();
        let live = Uuid::new_v4();
        let trigger = Uuid::new_v4();

        cache.insert_at(expired, BackendId::main(), start);
        cache.insert_at(live, BackendId::main(), start + Duration::from_secs(5));
        cache.insert_at(trigger, BackendId::main(), start + ttl);

        let state = cache.state.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(state.entries.len(), 2);
        assert!(state.entries.peek(&expired).is_none());
        assert!(state.entries.peek(&live).is_some());
        assert!(state.entries.peek(&trigger).is_some());
        assert_eq!(state.expirations.len(), 2);
    }

    #[test]
    fn capacity_eviction_removes_expiration_record() {
        let ttl = Duration::from_secs(10);
        let cache = LocatorCache::with_ttl_and_capacity(ttl, NonZeroUsize::new(1).unwrap());
        let start = Instant::now();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let third = Uuid::new_v4();

        cache.insert_at(first, BackendId::main(), start);
        cache.insert_at(second, BackendId::main(), start + Duration::from_secs(1));
        cache.insert_at(third, BackendId::main(), start + Duration::from_secs(11));

        let state = cache.state.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(state.entries.len(), 1);
        assert!(state.entries.peek(&third).is_some());
        assert_eq!(state.expirations.len(), 1);
    }

    #[test]
    fn refresh_replaces_expiration_record() {
        let ttl = Duration::from_secs(10);
        let cache = LocatorCache::with_ttl_and_capacity(ttl, NonZeroUsize::new(2).unwrap());
        let start = Instant::now();
        let refreshed = Uuid::new_v4();
        let other = Uuid::new_v4();

        cache.insert_at(refreshed, BackendId::main(), start);
        cache.insert_at(refreshed, BackendId::main(), start + Duration::from_secs(1));
        cache.insert_at(other, BackendId::main(), start + ttl);

        let state = cache.state.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(state.entries.len(), 2);
        assert!(state.entries.peek(&refreshed).is_some());
        assert!(state.entries.peek(&other).is_some());
        assert_eq!(state.expirations.len(), 2);
    }
}
