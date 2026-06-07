//! UUID-to-backend locator cache with lazy TTL eviction.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use khive_runtime::BackendId;
use uuid::Uuid;

/// Default TTL for locator cache entries (5 minutes).
pub(super) const DEFAULT_LOCATOR_TTL: Duration = Duration::from_secs(300);

struct LocatorEntry {
    backend_id: BackendId,
    inserted_at: Instant,
}

/// In-memory UUID-to-backend cache with lazy TTL eviction.
pub struct LocatorCache {
    entries: RwLock<HashMap<Uuid, LocatorEntry>>,
    pub(super) ttl: Duration,
}

impl LocatorCache {
    /// Construct with the given TTL.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
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
    /// Expired entries are removed under a write lock.
    pub fn get(&self, id: Uuid) -> Option<BackendId> {
        let now = Instant::now();
        {
            let guard = self.entries.read().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = guard.get(&id) {
                if now.duration_since(entry.inserted_at) < self.ttl {
                    return Some(entry.backend_id.clone());
                }
            } else {
                return None;
            }
        }
        let mut guard = self.entries.write().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = guard.get(&id) {
            if now.duration_since(entry.inserted_at) < self.ttl {
                return Some(entry.backend_id.clone());
            }
        }
        guard.remove(&id);
        None
    }

    /// Remove the cache entry for `id`, if any.
    pub fn remove(&self, id: Uuid) {
        let mut guard = self.entries.write().unwrap_or_else(|e| e.into_inner());
        guard.remove(&id);
    }

    /// Insert or refresh the owning backend for `id`.
    pub fn insert(&self, id: Uuid, backend_id: BackendId) {
        let mut guard = self.entries.write().unwrap_or_else(|e| e.into_inner());
        guard.insert(
            id,
            LocatorEntry {
                backend_id,
                inserted_at: Instant::now(),
            },
        );
    }

    /// Remove all entries whose TTL has elapsed.
    pub fn purge_expired(&self) {
        let now = Instant::now();
        let mut guard = self.entries.write().unwrap_or_else(|e| e.into_inner());
        guard.retain(|_, entry| now.duration_since(entry.inserted_at) < self.ttl);
    }

    /// Number of live entries (including possibly-expired ones not yet purged).
    pub fn len(&self) -> usize {
        let guard = self.entries.read().unwrap_or_else(|e| e.into_inner());
        guard.len()
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
