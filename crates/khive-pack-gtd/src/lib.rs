//! pack-gtd — GTD (Getting Things Done) verb pack for khive.
//!
//! Adds the `task` note kind and five verbs (`assign`, `next`, `complete`, `tasks`,
//! `transition`) with GTD lifecycle semantics over the notes substrate.

pub mod handlers;
pub mod hook;
mod pack;
pub mod schema;
pub(crate) mod vocab;

pub use pack::GtdPack;
pub(crate) use vocab::GTD_SCHEMA_PLAN_STMTS;
