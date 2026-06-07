//! Objective registry for dynamic dispatch.
//!
//! Runtime infrastructure: named registration, lookup, defaults.
//! Lives in khive-runtime (not khive-fold) because it depends on runtime types.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use parking_lot::RwLock;

use khive_fold::objective::{
    Objective, ObjectiveContext, ObjectiveError, ObjectiveResult, Selection,
};

/// A type-erased objective wrapper.
pub struct RegisteredObjective<T: Send + Sync> {
    pub name: String,
    pub description: Option<String>,
    objective: Box<dyn Objective<T>>,
}

impl<T: Send + Sync> fmt::Debug for RegisteredObjective<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegisteredObjective")
            .field("name", &self.name)
            .field("description", &self.description)
            .finish_non_exhaustive()
    }
}

impl<T: Send + Sync> RegisteredObjective<T> {
    /// Create a new registered objective with the given name and no description.
    pub fn new(name: impl Into<String>, objective: Box<dyn Objective<T>>) -> Self {
        Self {
            name: name.into(),
            description: None,
            objective,
        }
    }

    /// Attach a human-readable description to this registered objective.
    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Raw score (no precision weighting). Use `select()` for ranked selection with salience boost.
    pub fn score(&self, candidate: &T, context: &ObjectiveContext) -> f64 {
        self.objective.score(candidate, context)
    }

    /// Select the best candidate according to this objective's ranking.
    ///
    /// Returns `ObjectiveError::NoMatch` when the candidate slice is empty.
    pub fn select<'a>(
        &self,
        candidates: &'a [T],
        context: &ObjectiveContext,
    ) -> ObjectiveResult<Selection<&'a T>> {
        self.objective
            .select(candidates, context)
            .into_iter()
            .next()
            .ok_or_else(|| ObjectiveError::NoMatch("No candidate selected".into()))
    }
}

struct RegistryInner<T: Send + Sync> {
    objectives: HashMap<String, Arc<RegisteredObjective<T>>>,
    default: Option<String>,
}

/// Registry of named objectives.
///
/// Thread-safe: all operations are behind a single `RwLock`.
pub struct ObjectiveRegistry<T: Send + Sync> {
    inner: RwLock<RegistryInner<T>>,
}

impl<T: Send + Sync> fmt::Debug for ObjectiveRegistry<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.inner.read();
        f.debug_struct("ObjectiveRegistry")
            .field("count", &inner.objectives.len())
            .field("default", &inner.default)
            .finish()
    }
}

impl<T: Send + Sync> Default for ObjectiveRegistry<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Send + Sync> ObjectiveRegistry<T> {
    /// Create an empty registry with no objectives and no default set.
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(RegistryInner {
                objectives: HashMap::new(),
                default: None,
            }),
        }
    }

    /// Register a named objective; returns the previous entry if the name was already taken.
    pub fn register(
        &self,
        name: impl Into<String>,
        objective: Box<dyn Objective<T>>,
    ) -> Option<Arc<RegisteredObjective<T>>> {
        let name = name.into();
        let registered = Arc::new(RegisteredObjective::new(name.clone(), objective));
        self.inner.write().objectives.insert(name, registered)
    }

    /// Register a named objective with a human-readable description.
    pub fn register_with_desc(
        &self,
        name: impl Into<String>,
        description: impl Into<String>,
        objective: Box<dyn Objective<T>>,
    ) -> Option<Arc<RegisteredObjective<T>>> {
        let name = name.into();
        let registered = Arc::new(
            RegisteredObjective::new(name.clone(), objective).with_description(description),
        );
        self.inner.write().objectives.insert(name, registered)
    }

    /// Set the named objective as the registry default.
    ///
    /// Returns `ObjectiveError::NotFound` when no objective with that name is registered.
    pub fn set_default(&self, name: impl Into<String>) -> ObjectiveResult<()> {
        let name = name.into();
        let mut inner = self.inner.write();
        if !inner.objectives.contains_key(&name) {
            return Err(ObjectiveError::NotFound(name));
        }
        inner.default = Some(name);
        Ok(())
    }

    /// Retrieve a registered objective by name.
    ///
    /// Returns `ObjectiveError::NotFound` when the name is not registered.
    pub fn get(&self, name: &str) -> ObjectiveResult<Arc<RegisteredObjective<T>>> {
        self.inner
            .read()
            .objectives
            .get(name)
            .cloned()
            .ok_or_else(|| ObjectiveError::NotFound(name.to_string()))
    }

    /// Retrieve the current default objective.
    ///
    /// Returns `ObjectiveError::NotFound` when no default has been set.
    pub fn get_default(&self) -> ObjectiveResult<Arc<RegisteredObjective<T>>> {
        let inner = self.inner.read();
        match inner.default.as_ref() {
            Some(name) => inner
                .objectives
                .get(name)
                .cloned()
                .ok_or_else(|| ObjectiveError::NotFound(name.clone())),
            None => Err(ObjectiveError::NotFound("No default set".to_string())),
        }
    }

    /// List all registered objective names in sorted order.
    pub fn list(&self) -> Vec<String> {
        let inner = self.inner.read();
        let mut names: Vec<String> = inner.objectives.keys().cloned().collect();
        names.sort();
        names
    }

    /// Return `true` if an objective with the given name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.inner.read().objectives.contains_key(name)
    }

    /// Raw score via a named objective (no precision weighting).
    pub fn score(
        &self,
        name: &str,
        candidate: &T,
        context: &ObjectiveContext,
    ) -> ObjectiveResult<f64> {
        let objective = self.get(name)?;
        Ok(objective.score(candidate, context))
    }

    /// Select the best candidate using the named objective.
    ///
    /// Returns `ObjectiveError::NoMatch` when the candidate slice is empty.
    pub fn select<'a>(
        &self,
        name: &str,
        candidates: &'a [T],
        context: &ObjectiveContext,
    ) -> ObjectiveResult<Selection<&'a T>> {
        let objective = self.get(name)?;
        objective
            .select(candidates, context)
            .into_iter()
            .next()
            .ok_or_else(|| ObjectiveError::NoMatch("No candidate selected".into()))
    }

    /// Select the best candidate using the current default objective.
    ///
    /// Returns `ObjectiveError::NotFound` when no default is set.
    pub fn select_default<'a>(
        &self,
        candidates: &'a [T],
        context: &ObjectiveContext,
    ) -> ObjectiveResult<Selection<&'a T>> {
        let objective = self.get_default()?;
        objective
            .select(candidates, context)
            .into_iter()
            .next()
            .ok_or_else(|| ObjectiveError::NoMatch("No candidate selected".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_fold::objective::objective_fn;

    #[test]
    fn register_and_get() {
        let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();
        let obj = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
        let old = registry.register("max", Box::new(obj));
        assert!(old.is_none());
        assert!(registry.contains("max"));
        assert!(!registry.contains("min"));
    }

    #[test]
    fn register_overwrites() {
        let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();
        let obj1 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
        let obj2 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| -(*n as f64));
        assert!(registry.register("test", Box::new(obj1)).is_none());
        assert!(registry.register("test", Box::new(obj2)).is_some());

        let candidates = vec![1, 5, 3];
        let selection = registry
            .select("test", &candidates, &ObjectiveContext::new())
            .unwrap();
        assert_eq!(*selection.item, 1);
    }

    #[test]
    fn select_by_name() {
        let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();
        let obj = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
        registry.register("max", Box::new(obj));

        let candidates = vec![1, 5, 3];
        let selection = registry
            .select("max", &candidates, &ObjectiveContext::new())
            .unwrap();
        assert_eq!(*selection.item, 5);
    }

    #[test]
    fn default_objective() {
        let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();
        let obj = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
        registry.register("max", Box::new(obj));
        registry.set_default("max").unwrap();

        let candidates = vec![1, 5, 3];
        let selection = registry
            .select_default(&candidates, &ObjectiveContext::new())
            .unwrap();
        assert_eq!(*selection.item, 5);
    }

    #[test]
    fn list_objectives_sorted() {
        let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();
        let obj1 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
        let obj2 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| -(*n as f64));
        let obj3 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| (*n as f64).abs());
        registry.register("zebra", Box::new(obj1));
        registry.register("alpha", Box::new(obj2));
        registry.register("middle", Box::new(obj3));

        let names = registry.list();
        assert_eq!(names, vec!["alpha", "middle", "zebra"]);
    }

    #[test]
    fn get_nonexistent_returns_error() {
        let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();
        let result = registry.get("nope");
        assert!(matches!(result, Err(ObjectiveError::NotFound(ref s)) if s == "nope"));
    }

    #[test]
    fn get_default_without_setting_returns_error() {
        let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();
        let result = registry.get_default();
        assert!(matches!(result, Err(ObjectiveError::NotFound(_))));
    }

    #[test]
    fn set_default_nonexistent_returns_error() {
        let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();
        let result = registry.set_default("ghost");
        assert!(matches!(result, Err(ObjectiveError::NotFound(ref s)) if s == "ghost"));
    }

    #[test]
    fn score_via_registry() {
        let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();
        let obj = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64 * 2.0);
        registry.register("double", Box::new(obj));

        let score = registry
            .score("double", &5, &ObjectiveContext::new())
            .unwrap();
        assert!((score - 10.0).abs() < 1e-12);
    }

    #[test]
    fn select_default_via_registry() {
        let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();
        let obj = objective_fn(|n: &i32, _ctx: &ObjectiveContext| -(*n as f64));
        registry.register("min", Box::new(obj));
        registry.set_default("min").unwrap();

        let candidates = vec![1, 5, 3];
        let selection = registry
            .select_default(&candidates, &ObjectiveContext::new())
            .unwrap();
        assert_eq!(*selection.item, 1);
    }

    #[test]
    fn debug_impls() {
        let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();
        let obj = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
        registry.register("test", Box::new(obj));
        let debug = format!("{:?}", registry);
        assert!(debug.contains("ObjectiveRegistry"));
        assert!(debug.contains("count: 1"));

        let registered = registry.get("test").unwrap();
        let debug = format!("{:?}", registered);
        assert!(debug.contains("RegisteredObjective"));
        assert!(debug.contains("test"));
    }

    #[test]
    fn concurrent_read_write() {
        let registry = Arc::new(ObjectiveRegistry::<i32>::new());

        std::thread::scope(|s| {
            for i in 0..8 {
                let reg = Arc::clone(&registry);
                s.spawn(move || {
                    let name = format!("obj_{i}");
                    let obj =
                        objective_fn(move |n: &i32, _ctx: &ObjectiveContext| *n as f64 + i as f64);
                    reg.register(name.clone(), Box::new(obj));

                    assert!(reg.contains(&name));

                    let candidates = vec![1, 2, 3];
                    let _ = reg.select(&name, &candidates, &ObjectiveContext::new());
                });
            }
        });

        assert_eq!(registry.list().len(), 8);
    }
}
