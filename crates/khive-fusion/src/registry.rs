//! Runtime registry for dispatching `FusionStrategy::Custom` by name.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use khive_score::DeterministicScore;

/// A runtime-registered fusion implementation, dispatched by name from
/// `FusionStrategy::Custom { name, params }`.
///
/// Implementations own their own score semantics; the registry only handles
/// name resolution. `params` carries the opaque JSON payload from the
/// selecting [`crate::FusionStrategy::Custom`].
pub trait CustomFusion<Id>: Send + Sync {
    /// Combine `sources` into a single ranked list.
    fn fuse(
        &self,
        sources: Vec<Vec<(Id, DeterministicScore)>>,
        params: &serde_json::Value,
    ) -> Vec<(Id, DeterministicScore)>;
}

impl<Id, F> CustomFusion<Id> for F
where
    F: Fn(Vec<Vec<(Id, DeterministicScore)>>, &serde_json::Value) -> Vec<(Id, DeterministicScore)>
        + Send
        + Sync,
{
    fn fuse(
        &self,
        sources: Vec<Vec<(Id, DeterministicScore)>>,
        params: &serde_json::Value,
    ) -> Vec<(Id, DeterministicScore)> {
        (self)(sources, params)
    }
}

/// Returned by [`FusionRegistry::dispatch`] when `name` has no registration.
///
/// Lookup fails closed: an unrecognized name is always an error, never a
/// silent fall-back to RRF or any other default strategy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownFusionStrategy(pub String);

impl fmt::Display for UnknownFusionStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no fusion strategy registered under name '{}'", self.0)
    }
}

impl std::error::Error for UnknownFusionStrategy {}

/// Name -> custom fusion implementation table.
///
/// Built by a caller (e.g. the runtime, or a pack at construction time) and
/// passed alongside a [`crate::FusionStrategy::Custom`] selection to resolve
/// it. This is the seam a learned-sparse (e.g. SPLADE) retrieval leg plugs
/// into without either crate depending on the other.
pub struct FusionRegistry<Id> {
    strategies: HashMap<String, Arc<dyn CustomFusion<Id>>>,
}

impl<Id> Default for FusionRegistry<Id> {
    fn default() -> Self {
        Self {
            strategies: HashMap::new(),
        }
    }
}

impl<Id> FusionRegistry<Id> {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a custom fusion implementation under `name`, replacing any
    /// prior registration for the same name.
    pub fn register(&mut self, name: impl Into<String>, strategy: impl CustomFusion<Id> + 'static) {
        self.strategies.insert(name.into(), Arc::new(strategy));
    }

    /// True if `name` has a registered implementation.
    pub fn contains(&self, name: &str) -> bool {
        self.strategies.contains_key(name)
    }

    /// Dispatch `sources`/`params` to the implementation registered under
    /// `name`, or fail closed with [`UnknownFusionStrategy`].
    pub fn dispatch(
        &self,
        name: &str,
        sources: Vec<Vec<(Id, DeterministicScore)>>,
        params: &serde_json::Value,
    ) -> Result<Vec<(Id, DeterministicScore)>, UnknownFusionStrategy> {
        match self.strategies.get(name) {
            Some(strategy) => Ok(strategy.fuse(sources, params)),
            None => Err(UnknownFusionStrategy(name.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(items: Vec<(&'static str, f64)>) -> Vec<(&'static str, DeterministicScore)> {
        items
            .into_iter()
            .map(|(id, s)| (id, DeterministicScore::from_f64(s)))
            .collect()
    }

    #[test]
    fn register_then_dispatch_round_trips() {
        let mut registry: FusionRegistry<&'static str> = FusionRegistry::new();
        registry.register(
            "reverse",
            |sources: Vec<Vec<(&'static str, DeterministicScore)>>, _: &serde_json::Value| {
                let mut flat: Vec<_> = sources.into_iter().flatten().collect();
                flat.reverse();
                flat
            },
        );

        let sources = vec![make(vec![("a", 0.1), ("b", 0.2), ("c", 0.3)])];
        let out = registry
            .dispatch("reverse", sources, &serde_json::Value::Null)
            .unwrap();
        assert_eq!(
            out.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec!["c", "b", "a"]
        );
    }

    #[test]
    fn dispatch_unknown_name_fails_closed() {
        let registry: FusionRegistry<&'static str> = FusionRegistry::new();
        let err = registry
            .dispatch("nonexistent", vec![], &serde_json::Value::Null)
            .unwrap_err();
        assert_eq!(err, UnknownFusionStrategy("nonexistent".to_string()));
    }

    #[test]
    fn contains_reflects_registration() {
        let mut registry: FusionRegistry<&'static str> = FusionRegistry::new();
        assert!(!registry.contains("x"));
        registry.register("x", |_, _: &serde_json::Value| Vec::new());
        assert!(registry.contains("x"));
    }
}
