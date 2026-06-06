// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! KG import/export format adapters (ADR-036). See `docs/protocol.md` for the
//! full pipeline description, parse strategy notes, and deferred format list.

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
