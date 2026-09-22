//! pack-gtd — GTD (Getting Things Done) verb pack for khive.
//!
//! Adds the `task` note kind, five task-management verbs, and a read-only
//! timestamp census over the notes substrate.

mod census;
pub(crate) mod dependency;
pub mod handlers;
pub mod hook;
mod pack;
pub mod schema;
pub(crate) mod task_create;
pub(crate) mod vocab;

pub use pack::GtdPack;
pub(crate) use vocab::GTD_SCHEMA_PLAN_STMTS;
