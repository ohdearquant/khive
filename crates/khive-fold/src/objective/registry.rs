//! Objective registry for dynamic dispatch.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::{Objective, ObjectiveContext, ObjectiveError, ObjectiveResult, Selection};

/// A type-erased objective wrapper.
pub struct RegisteredObjective<T> {
    /// Name of the objective
    pub name: String,
    /// Description
    pub description: Option<String>,
    /// The objective implementation
    objective: Box<dyn Objective<T>>,
}

impl<T> RegisteredObjective<T> {
    /// Create a new registered objective
    pub fn new(name: impl Into<String>, objective: Box<dyn Objective<T>>) -> Self {
        Self {
            name: name.into(),
            description: None,
            objective,
        }
    }

    /// Add a description
    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Score a candidate
    pub fn score(&self, candidate: &T, context: &ObjectiveContext) -> f64 {
        self.objective.score(candidate, context)
    }

    /// Select from candidates, returning the best match or an error.
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

/// Registry of named objectives.
pub struct ObjectiveRegistry<T> {
    objectives: RwLock<HashMap<String, Arc<RegisteredObjective<T>>>>,
    default: RwLock<Option<String>>,
}

impl<T> Default for ObjectiveRegistry<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> ObjectiveRegistry<T> {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self {
            objectives: RwLock::new(HashMap::new()),
            default: RwLock::new(None),
        }
    }

    /// Register an objective.
    ///
    /// Returns the previously registered objective if one existed with the same name.
    pub fn register(
        &self,
        name: impl Into<String>,
        objective: Box<dyn Objective<T>>,
    ) -> Option<Arc<RegisteredObjective<T>>> {
        let name = name.into();
        let registered = Arc::new(RegisteredObjective::new(name.clone(), objective));

        let mut objectives = self.objectives.write();
        objectives.insert(name, registered)
    }

    /// Register an objective with description.
    ///
    /// Returns the previously registered objective if one existed with the same name.
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

        let mut objectives = self.objectives.write();
        objectives.insert(name, registered)
    }

    /// Set the default objective
    pub fn set_default(&self, name: impl Into<String>) -> ObjectiveResult<()> {
        let name = name.into();

        let objectives = self.objectives.read();
        if !objectives.contains_key(&name) {
            return Err(ObjectiveError::NotFound(name));
        }
        drop(objectives);

        let mut default = self.default.write();
        *default = Some(name);
        Ok(())
    }

    /// Get an objective by name
    pub fn get(&self, name: &str) -> ObjectiveResult<Arc<RegisteredObjective<T>>> {
        let objectives = self.objectives.read();
        objectives
            .get(name)
            .cloned()
            .ok_or_else(|| ObjectiveError::NotFound(name.to_string()))
    }

    /// Get the default objective
    pub fn get_default(&self) -> ObjectiveResult<Arc<RegisteredObjective<T>>> {
        let default = self.default.read();
        match default.as_ref() {
            Some(name) => {
                let name: String = name.clone();
                drop(default);
                self.get(&name)
            }
            None => Err(ObjectiveError::NotFound("No default set".to_string())),
        }
    }

    /// List all registered objective names.
    ///
    /// Returns names in sorted order for deterministic output.
    pub fn list(&self) -> Vec<String> {
        let objectives = self.objectives.read();
        let mut names: Vec<String> = objectives.keys().cloned().collect();
        names.sort();
        names
    }

    /// Check if an objective is registered
    pub fn contains(&self, name: &str) -> bool {
        let objectives = self.objectives.read();
        objectives.contains_key(name)
    }

    /// Score using a named objective
    pub fn score(
        &self,
        name: &str,
        candidate: &T,
        context: &ObjectiveContext,
    ) -> ObjectiveResult<f64> {
        let objective = self.get(name)?;
        Ok(objective.score(candidate, context))
    }

    /// Select using a named objective, returning the best match or an error.
    pub fn select<'a>(
        &self,
        name: &str,
        candidates: &'a [T],
        context: &ObjectiveContext,
    ) -> ObjectiveResult<Selection<&'a T>> {
        let objective = self.get(name)?;
        objective.select(candidates, context)
    }

    /// Select using the default objective, returning the best match or an error.
    pub fn select_default<'a>(
        &self,
        candidates: &'a [T],
        context: &ObjectiveContext,
    ) -> ObjectiveResult<Selection<&'a T>> {
        let objective = self.get_default()?;
        objective.select(candidates, context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objective_fn;

    #[test]
    fn test_register_and_get() {
        let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();

        let obj = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
        let old = registry.register("max", Box::new(obj));

        assert!(old.is_none());
        assert!(registry.contains("max"));
        assert!(!registry.contains("min"));
    }

    #[test]
    fn test_register_overwrites() {
        let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();

        let obj1 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
        let obj2 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| -(*n as f64));

        let old1 = registry.register("test", Box::new(obj1));
        assert!(old1.is_none());

        let old2 = registry.register("test", Box::new(obj2));
        assert!(old2.is_some());

        let candidates = vec![1, 5, 3];
        let selection = registry
            .select("test", &candidates, &ObjectiveContext::new())
            .unwrap();
        assert_eq!(*selection.item, 1);
    }

    #[test]
    fn test_select_by_name() {
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
    fn test_default_objective() {
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
    fn test_list_objectives_sorted() {
        let registry: ObjectiveRegistry<i32> = ObjectiveRegistry::new();

        let obj1 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
        let obj2 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| -(*n as f64));
        let obj3 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| (*n as f64).abs());

        registry.register("zebra", Box::new(obj1));
        registry.register("alpha", Box::new(obj2));
        registry.register("middle", Box::new(obj3));

        let names = registry.list();
        assert_eq!(names.len(), 3);
        assert_eq!(names, vec!["alpha", "middle", "zebra"]);
    }
}
