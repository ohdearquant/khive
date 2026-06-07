//! KG versioning — content-addressed snapshot hashing and NDJSON-to-SQLite sync.
//!
//! Git-native v1: KG state lives as sorted NDJSON files in a git repo. Retains
//! `types` (snapshot IDs), `hash` (SHA-256), `sync` (rebuild library), and `error`.

pub mod error;
pub mod hash;
pub mod sync;
pub mod types;

pub use error::VcsError;
pub use types::{SnapshotCoverage, SnapshotId, VcsState, KG_V1_COVERAGE};
