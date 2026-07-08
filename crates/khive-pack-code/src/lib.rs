//! pack-code — code ontology pack for khive (ADR-085).
//!
//! Registers four concept subtypes (`module`, `function`, `datatype`,
//! `interface`) via `khive-pack-kg`'s entity type registry, additive
//! `EDGE_RULES` over the closed relation set, and the `finding` audit note
//! kind. Contributes no verbs; `findings.json` ingest is an internal Rust
//! API (see [`ingest`]), not an MCP wire surface. Opt-in only — not part of
//! the default pack set.

mod error;
mod hook;
pub mod ingest;
mod pack;
pub(crate) mod vocab;

pub use error::CodeIngestError;
pub use ingest::{ingest_findings_json, CodeIngestBatch, CodeIngestOptions, CODE_INGEST_NAMESPACE};
pub use pack::CodePack;
