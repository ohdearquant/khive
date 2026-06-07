//! Backend registry for the SubstrateCoordinator.

use std::collections::BTreeMap;
use std::sync::Arc;

use khive_runtime::{BackendId, KhiveRuntime};

/// A registered backend entry held by the [`BackendRegistry`].
#[derive(Clone)]
pub struct BackendEntry {
    /// Unique identifier for this backend.
    pub id: BackendId,
    /// The runtime instance operating over this backend.
    pub runtime: Arc<KhiveRuntime>,
}

/// Registry of all backends known to the coordinator.
///
/// Constructed once at boot and immutable thereafter.
/// Keyed by [`BackendId`] for deterministic ordering.
#[derive(Default)]
pub struct BackendRegistry {
    backends: BTreeMap<String, BackendEntry>,
    primary: Option<String>,
}

impl BackendRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a backend. The first registered becomes the primary.
    ///
    /// Returns `false` if a backend with the same `id` was already registered.
    pub fn register(&mut self, id: BackendId, runtime: Arc<KhiveRuntime>) -> bool {
        let key = id.as_str().to_string();
        if self.backends.contains_key(&key) {
            return false;
        }
        if self.primary.is_none() {
            self.primary = Some(key.clone());
        }
        self.backends.insert(key, BackendEntry { id, runtime });
        true
    }

    /// Look up a backend by id.
    pub fn get(&self, id: &BackendId) -> Option<&BackendEntry> {
        self.backends.get(id.as_str())
    }

    /// The primary backend (first registered). `None` only if the registry is empty.
    pub fn primary(&self) -> Option<&BackendEntry> {
        self.primary.as_deref().and_then(|k| self.backends.get(k))
    }

    /// Iterate over all registered backends.
    pub fn iter(&self) -> impl Iterator<Item = &BackendEntry> {
        self.backends.values()
    }

    /// Number of registered backends.
    pub fn len(&self) -> usize {
        self.backends.len()
    }

    /// True if no backends have been registered.
    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }

    /// List all registered [`BackendId`]s.
    pub fn ids(&self) -> Vec<BackendId> {
        self.backends.keys().map(BackendId::new).collect()
    }
}
