//! Objective function framework — scoring, selection, composition.

pub mod builtin;
pub mod compose;
mod context;
pub mod error;
mod selection;
mod traits;

pub use context::ObjectiveContext;
pub use error::{ObjectiveError, ObjectiveResult};
pub use selection::Selection;
pub use traits::{objective_fn, DeterministicObjective, Objective};

#[cfg(test)]
mod tests;
