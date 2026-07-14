//! Code concept vocabulary, finding-note lifecycle, and deterministic audit ingest (ADR-085).

mod error;
mod hook;
pub mod ingest;
mod pack;
pub(crate) mod vocab;

pub use error::CodeIngestError;
pub use ingest::{ingest_findings_json, CodeIngestBatch, CodeIngestOptions, CODE_INGEST_NAMESPACE};
pub use pack::CodePack;
