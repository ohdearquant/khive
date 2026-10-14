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
#[path = "registry_tests.rs"]
mod tests;
