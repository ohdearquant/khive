//! Offline, read-only `khive.repo.v1` repository showcase exporter.
//!
//! The exporter and its model are behind the default `export` feature. With
//! default features off the crate provides only [`git_safety`].

#[cfg(feature = "export")]
mod aggregate;
#[cfg(feature = "export")]
mod export;
pub mod git_safety;
#[cfg(feature = "export")]
mod join;
#[cfg(feature = "export")]
mod model;
#[cfg(feature = "export")]
mod read;

#[cfg(feature = "export")]
pub use export::{
    canonical_bytes, export, export_canonical_bytes, json_schema, write_canonical_atomic,
    ExportError,
};
#[cfg(feature = "export")]
pub use model::*;
