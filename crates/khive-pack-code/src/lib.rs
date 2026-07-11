//! pack-code — code ontology pack for khive (ADR-085).
//!
//! Registers four concept subtypes (`module`, `function`, `datatype`,
//! `interface`) via `khive-pack-kg`'s entity type registry, additive
//! `EDGE_RULES` over the closed relation set, and the `finding` audit note
//! kind. Currently contributes no verbs (ADR-085 D1; Amendment 2's accepted
//! `code.ingest` source-ingest verb is unimplemented); `findings.json`
//! ingest runs through the `kkernel code-ingest` admin CLI path, not an MCP
//! wire surface (ADR-085 Amendment 3). In the default pack set as of
//! Amendment 3, so the `finding` note kind is live on the production
//! surface.

mod error;
mod hook;
pub mod ingest;
mod pack;
pub(crate) mod vocab;

pub use error::CodeIngestError;
pub use ingest::{ingest_findings_json, CodeIngestBatch, CodeIngestOptions, CODE_INGEST_NAMESPACE};
pub use pack::CodePack;
