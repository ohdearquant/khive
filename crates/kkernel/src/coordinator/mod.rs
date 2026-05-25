//! SubstrateCoordinator — cross-backend dispatch layer (ADR-003, ADR-029).
//!
//! The coordinator lives inside `kkernel` as kernel-internal plumbing. Pack crates
//! do not depend on it (ADR-003 §anti-pattern-9). It owns:
//!
//! - Node-to-backend location cache (D2 — `Arc<DashMap<Uuid, BackendName>>`)
//! - Cross-backend `link()` mechanics (D3)
//! - Substrate-kind search fan-out with unweighted RRF (D4)
//! - Cross-backend traversal and curation semantics (D5)
//! - Partition tolerance / backend health map (D6)
//!
//! # Single-backend behaviour
//!
//! When only one backend is registered, every D1–D6 mechanism degenerates to its
//! trivial identity: no fan-out, no cross-backend routing, no health map misses.
//! Multi-backend complexity is opt-in via `khive.toml` (ADR-028).
//!
//! # Module structure (ADR-029 §coordinator-module-tree)
//!
//! ```text
//! kkernel::coordinator
//!   mod.rs          — SubstrateCoordinator + BackendRegistry (this file)
//! ```
//!
//! Future sub-modules (`edges`, `locator`, `search`, `traversal`, `curation`,
//! `health`) are reserved per ADR-029 but are not yet implemented; they will
//! land when the corresponding features are built out.

use std::collections::HashMap;
use std::sync::Arc;

use khive_runtime::{BackendId, KhiveRuntime};

// ---- BackendRegistry ----

/// A registered backend entry held by the [`SubstrateCoordinator`].
#[derive(Clone)]
pub struct BackendEntry {
    /// Unique identifier for this backend (matches `[[backends.name]]` in `khive.toml`).
    pub id: BackendId,
    /// The runtime instance operating over this backend.
    pub runtime: Arc<KhiveRuntime>,
}

/// Registry of all backends known to the coordinator.
///
/// Constructed once at boot from `khive.toml` (ADR-028) and immutable thereafter.
/// Keyed by [`BackendId`] for O(1) lookup.
#[derive(Default)]
pub struct BackendRegistry {
    backends: HashMap<String, BackendEntry>,
    primary: Option<String>,
}

impl BackendRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a backend. The first backend registered becomes the primary.
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

// ---- SubstrateCoordinator ----

/// Cross-backend dispatch layer (ADR-003 §four-invariants, ADR-029).
///
/// The coordinator owns all cross-backend operations:
/// - Node-to-backend resolution (D2 locator cache)
/// - Cross-backend `link()` routing (D3)
/// - Substrate-kind search fan-out with RRF (D4)
/// - Cross-backend traversal (D5)
/// - Partition tolerance (D6)
///
/// Pack handlers do NOT see the coordinator; they receive a single-backend
/// [`KhiveRuntime`] and operate within it. The coordinator routes across backends
/// above the pack layer.
///
/// # Current implementation status
///
/// v1 ships the `BackendRegistry`, `BackendId` concept, and the
/// `merge_entity` cross-backend guard. Full D2–D6 mechanics (locator cache,
/// fan-out search, cross-backend traversal, WAL cascade) are deferred to the
/// ADR-029 full implementation milestone.
pub struct SubstrateCoordinator {
    registry: BackendRegistry,
}

impl SubstrateCoordinator {
    /// Construct from a [`BackendRegistry`].
    pub fn new(registry: BackendRegistry) -> Self {
        Self { registry }
    }

    /// Construct with a single backend (single-backend deployment default).
    ///
    /// Uses `BackendId::main()` as the backend id. The coordinator degenerates
    /// to a pass-through; all cross-backend mechanisms are identity.
    pub fn single(runtime: Arc<KhiveRuntime>) -> Self {
        let mut registry = BackendRegistry::new();
        registry.register(BackendId::main(), runtime);
        Self { registry }
    }

    /// The underlying [`BackendRegistry`].
    pub fn registry(&self) -> &BackendRegistry {
        &self.registry
    }

    /// Resolve which backend owns `id` by checking the locator cache, then performing
    /// a parallel-fetch fallback across all backends.
    ///
    /// Returns `None` if no backend claims the UUID. In v1 this is a linear scan;
    /// the D2 lazy cache is a follow-up when the locator module is implemented.
    ///
    /// For a single-backend deployment this always returns the primary backend
    /// (or `None` if the UUID doesn't exist anywhere).
    pub fn primary_runtime(&self) -> Option<Arc<KhiveRuntime>> {
        self.registry.primary().map(|e| Arc::clone(&e.runtime))
    }

    /// List all registered backend ids.
    pub fn backend_ids(&self) -> Vec<BackendId> {
        self.registry.ids()
    }

    /// Number of registered backends.
    pub fn backend_count(&self) -> usize {
        self.registry.len()
    }

    /// True when this is a single-backend deployment.
    ///
    /// When `true`, all D1–D6 coordinator mechanisms degenerate to identity:
    /// no fan-out, no cross-backend routing, no partition concerns.
    pub fn is_single_backend(&self) -> bool {
        self.registry.len() <= 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_runtime::KhiveRuntime;

    fn memory_runtime() -> Arc<KhiveRuntime> {
        Arc::new(KhiveRuntime::memory().expect("memory runtime"))
    }

    #[test]
    fn single_coordinator_is_single_backend() {
        let coord = SubstrateCoordinator::single(memory_runtime());
        assert!(coord.is_single_backend());
        assert_eq!(coord.backend_count(), 1);
        assert_eq!(coord.backend_ids().len(), 1);
        assert_eq!(coord.backend_ids()[0].as_str(), "main");
    }

    #[test]
    fn registry_register_dedup() {
        let mut reg = BackendRegistry::new();
        let rt = memory_runtime();
        assert!(reg.register(BackendId::new("main"), Arc::clone(&rt)));
        assert!(!reg.register(BackendId::new("main"), Arc::clone(&rt)));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn registry_primary_is_first_registered() {
        let mut reg = BackendRegistry::new();
        let rt1 = memory_runtime();
        let rt2 = memory_runtime();
        reg.register(BackendId::new("main"), rt1);
        reg.register(BackendId::new("lore"), rt2);
        assert_eq!(reg.primary().unwrap().id.as_str(), "main");
    }

    #[test]
    fn multi_backend_coordinator_not_single() {
        let mut registry = BackendRegistry::new();
        registry.register(BackendId::new("main"), memory_runtime());
        registry.register(BackendId::new("lore"), memory_runtime());
        let coord = SubstrateCoordinator::new(registry);
        assert!(!coord.is_single_backend());
        assert_eq!(coord.backend_count(), 2);
    }

    #[test]
    fn backend_id_display() {
        let id = BackendId::new("archive");
        assert_eq!(id.to_string(), "archive");
        assert_eq!(id.as_str(), "archive");
    }

    #[test]
    fn backend_id_main_constant() {
        assert_eq!(BackendId::main().as_str(), BackendId::MAIN);
    }
}
