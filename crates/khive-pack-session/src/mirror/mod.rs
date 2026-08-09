//! Background live-mirror service for CLI transcripts and provider exports.
//!
//! Keeps raw ingest/deferred cursor machinery private, exposes parsing and
//! service modules, and re-exports the generation-safe line-tail entry point
//! plus public mirror types.

pub mod ingest;
pub mod parse;
pub mod service;

pub use ingest::{mirror_file, LineTailSource, MirrorSource, MirrorStats};
pub use parse::{parse_cc_line, parse_codex_line};
pub use service::{run_mirror_service, MirrorConfig};
