//! `CodeIngestError` — fail-closed validation errors for `findings.json` ingest.

use thiserror::Error;

/// Fail-closed ingest errors that name the invalid field, accepted values, or shape.
///
/// See `crates/khive-pack-code/docs/api/findings-ingest.md`.
#[derive(Debug, Error)]
pub enum CodeIngestError {
    #[error("findings.json must be a JSON object with required field findings: array")]
    InvalidRoot,

    #[error("missing required field {path}")]
    MissingField { path: &'static str },

    #[error("field {path} must be {expected}")]
    InvalidType {
        path: String,
        expected: &'static str,
    },

    #[error("invalid {field} {value:?}; valid: {valid}")]
    InvalidValue {
        field: &'static str,
        value: String,
        valid: &'static str,
    },

    #[error("finding {id:?} with severity {severity:?} requires non-empty failure_scenario")]
    MissingFailureScenario { id: String, severity: String },

    #[error(
        "invalid evidence at findings[{finding_index}].evidence[{evidence_index}]; valid: string or object with path,line,description"
    )]
    InvalidEvidence {
        finding_index: usize,
        evidence_index: usize,
    },

    #[error("invalid source_run; provide options.source_run or audit.date and audit.commit")]
    MissingSourceRun,

    #[error("json parse: {0}")]
    Json(#[from] serde_json::Error),
}
