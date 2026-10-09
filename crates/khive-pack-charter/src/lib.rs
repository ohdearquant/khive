//! Charter recording schema and transaction-scoped transition primitives.
//!
//! This foundation exposes no verbs or action admission. See ADR-193 M1.

mod pack;
mod schema;
pub mod transition;

pub use pack::CharterPack;
