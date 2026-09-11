//! Backend registry for the SubstrateCoordinator.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use khive_runtime::{BackendId, KhiveRuntime};
use khive_types::SubstrateKind;

/// Invalid backend registration metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendRegistrationError {
    /// An explicit declaration must name at least one served substrate kind.
    EmptyServedKinds { backend_id: BackendId },
}

impl fmt::Display for BackendRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyServedKinds { backend_id } => write!(
                formatter,
                "backend {backend_id:?}: served kinds must not be empty when declared"
            ),
        }
    }
}

impl std::error::Error for BackendRegistrationError {}

/// A registered backend entry held by the [`BackendRegistry`].
#[derive(Clone)]
pub struct BackendEntry {
    /// Unique identifier for this backend.
    pub id: BackendId,
    /// The runtime instance operating over this backend.
    pub runtime: Arc<KhiveRuntime>,
    /// Closed declaration of served substrate kinds, or `None` for the
    /// conservative legacy behavior of serving every substrate.
    pub served_kinds: Option<BTreeSet<SubstrateKind>>,
}

impl BackendEntry {
    /// Whether registration metadata permits dispatch for `kind`.
    pub fn serves(&self, kind: SubstrateKind) -> bool {
        self.served_kinds
            .as_ref()
            .is_none_or(|served| served.contains(&kind))
    }
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
        self.register_with_served_kinds(id, runtime, None)
            .expect("absent served-kind metadata is always valid")
    }

    /// Register a backend with an optional closed served-kind declaration.
    ///
    /// `None` conservatively includes the backend in every dispatch. An
    /// explicit empty declaration is invalid rather than meaning "serve
    /// nothing", so configuration mistakes fail closed at registration.
    pub fn register_with_served_kinds(
        &mut self,
        id: BackendId,
        runtime: Arc<KhiveRuntime>,
        served_kinds: Option<BTreeSet<SubstrateKind>>,
    ) -> Result<bool, BackendRegistrationError> {
        if served_kinds.as_ref().is_some_and(BTreeSet::is_empty) {
            return Err(BackendRegistrationError::EmptyServedKinds { backend_id: id });
        }
        let key = id.as_str().to_string();
        if self.backends.contains_key(&key) {
            return Ok(false);
        }
        if self.primary.is_none() {
            self.primary = Some(key.clone());
        }
        self.backends.insert(
            key,
            BackendEntry {
                id,
                runtime,
                served_kinds,
            },
        );
        Ok(true)
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
        self.backends
            .values()
            .map(|entry| entry.id.clone())
            .collect()
    }
}
