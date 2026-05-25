// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! KG import/export format adapters (ADR-036).
//!
//! Adapters are pure transforms in the two-stage pipeline:
//!
//! ```text
//! source file
//!     | adapter (pure transform — no DB access)
//! intermediate NDJSON (entities + edges, in-memory or temp file)
//!     | khive kg import (validates + loads)
//! working.db
//! ```
//!
//! P0 (shipped): [`FormatAdapter`] trait, [`EntityRecord`], [`EdgeRecord`],
//! and the [`AdapterError`] type.
//!
//! P1 (deferred): BibTeX, Turtle/N-Triples, JSON-LD adapters.
//! P2 (deferred): GraphML, GEXF, Markdown adapters.

mod error;
pub use error::AdapterError;

mod record;
pub use record::{EdgeRecord, EntityRecord};

mod adapter;
pub use adapter::FormatAdapter;

mod json_adapter;
pub use json_adapter::JsonFormatAdapter;

/// Phase P0: format names accepted by the v0.5 adapter registry.
pub const PHASE0_FORMATS: &[&str] = &["csv", "tsv", "json", "ndjson"];
