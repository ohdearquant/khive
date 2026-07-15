//! Code concept vocabulary, finding-note lifecycle, and deterministic audit ingest (ADR-085).

mod db_target;
mod error;
mod handlers;
mod hook;
pub mod imports;
pub mod ingest;
pub mod manifest;
mod pack;
pub mod source_ingest;
pub(crate) mod vocab;

pub use error::CodeIngestError;
pub use ingest::{ingest_findings_json, CodeIngestBatch, CodeIngestOptions, CODE_INGEST_NAMESPACE};
pub use pack::CodePack;
pub use source_ingest::{CodeSourceIngestError, CodeSourceIngestOptions, CodeSourceIngestReport};
