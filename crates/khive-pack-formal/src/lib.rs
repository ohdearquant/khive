//! pack-formal — formal-math ontology pack for khive.
//!
//! Registers typed edge endpoint rules for six formal-math concept subtypes
//! (theorem, definition, structure, instance, axiom, goal). Pure ontology: no verbs.

mod pack;
pub(crate) mod vocab;

pub use pack::FormalPack;
