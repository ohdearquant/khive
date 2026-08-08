//! Code concept vocabulary, finding-note lifecycle, and deterministic audit ingest (ADR-085).

mod db_target;
mod error;
// L2 symbol-tier scanner/extractor (ADR-085 Amendment 2 B2), wired into
// `source_ingest`'s L2 sweep via `parse_rust_file`.
pub(crate) mod extractor;
mod handlers;
mod hook;
pub mod imports;
pub mod ingest;
pub mod manifest;
mod pack;
pub(crate) mod scanner_rust;
pub mod source_ingest;
pub(crate) mod vocab;

pub use error::CodeIngestError;
pub use ingest::{ingest_findings_json, CodeIngestBatch, CodeIngestOptions, CODE_INGEST_NAMESPACE};
pub use pack::CodePack;
pub use source_ingest::{
    CodeSourceIngestError, CodeSourceIngestL2Report, CodeSourceIngestOptions,
    CodeSourceIngestReport,
};
