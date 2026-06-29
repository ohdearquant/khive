//! Background live-mirror service for Claude Code session transcripts.
//!
//! Exposes the three sub-modules and re-exports the public surface used by
//! `SessionPack::warm()` and tests.

pub mod ingest;
pub mod parse;
pub mod service;

pub use ingest::MirrorStats;
pub use parse::parse_cc_line;
pub use service::{run_mirror_service, MirrorConfig};
