// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! Adapter error type.

use thiserror::Error;

/// An error produced by a format adapter.
///
/// Variants are fatal to the containing record/source and require atomic caller handling;
/// optional issues use warnings. See `crates/khive-vcs-adapters/docs/api/wire-records.md`.
#[derive(Debug, Error)]
pub enum AdapterError {
    /// A required field is missing from a record.
    #[error("record {index}: missing required field '{field}'")]
    MissingField { index: usize, field: String },

    /// A field has an unexpected type or value.
    #[error("record {index}: invalid value for field '{field}': {reason}")]
    InvalidField {
        index: usize,
        field: String,
        reason: String,
    },

    /// The source file cannot be parsed (structural failure).
    #[error("parse error: {0}")]
    Parse(String),

    /// An entity kind is unknown under strict schema mode.
    #[error("record {index}: unknown entity kind '{kind}'")]
    UnknownKind { index: usize, kind: String },

    /// An edge relation is not in the closed set of 15 canonical relations.
    ///
    /// This is always an error regardless of `--schema-mode`.
    #[error("record {index}: unknown edge relation '{relation}'")]
    UnknownRelation { index: usize, relation: String },

    /// A deferred format was requested.
    #[error("format '{format}' is not yet implemented (deferred to P1/P2)")]
    NotYetImplemented { format: String },
}
