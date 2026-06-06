//! Index alias management for zero-downtime HNSW index migration.
//!
//! Blue-green swap via atomic pointer exchange; concurrent readers drain safely.
//! See `docs/alias.md` for the migration steps and concurrency model.

mod drain;
pub mod error;
mod manager;
pub mod validation;

pub use drain::ReaderGuard;
pub use manager::{IndexAliasManager, MigrationReport};
pub use validation::{IndexValidator, NoopValidator, RecallValidator};
