//! Read-only Clippy JSON-line conversion into the existing finding ingest format.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};
use thiserror::Error;
use uuid::Uuid;

use crate::ingest::{
    ingest_findings_json, CodeIngestBatch, CodeIngestOptions, CODE_INGEST_NAMESPACE,
};
use crate::CodeIngestError;

pub const CLIPPY_PRODUCER_ID: &str = "cargo-clippy/json/v1";

#[derive(Clone, Copy, Debug)]
pub struct ClippyProvenance<'a> {
    pub repo: &'a str,
    pub branch: &'a str,
    pub commit: &'a str,
    pub scope: &'a str,
}

#[derive(Debug, Error)]
pub enum ClippyAdapterError {
    #[error("Clippy JSON stream is empty")]
    EmptyStream,
    #[error("Clippy JSON line {line}: {reason}")]
    Line { line: usize, reason: String },
    #[error("Clippy provenance {field} must be non-blank")]
    Provenance { field: &'static str },
    #[error("Clippy JSON serialization: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error(transparent)]
    Ingest(#[from] CodeIngestError),
}

fn line_error(line: usize, reason: impl Into<String>) -> ClippyAdapterError {
    ClippyAdapterError::Line {
        line,
        reason: reason.into(),
    }
}

fn object<'a>(
    value: &'a Value,
    line: usize,
    path: &str,
) -> Result<&'a Map<String, Value>, ClippyAdapterError> {
    value
        .as_object()
        .ok_or_else(|| line_error(line, format!("{path} must be an object")))
}

fn string<'a>(
    value: Option<&'a Value>,
    line: usize,
    path: &str,
) -> Result<&'a str, ClippyAdapterError> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| line_error(line, format!("{path} must be a non-blank string")))
}

fn positive_integer(
    value: Option<&Value>,
    line: usize,
    path: &str,
) -> Result<u64, ClippyAdapterError> {
    value
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| line_error(line, format!("{path} must be a positive integer")))
}

fn relative_path(raw: &str, line: usize) -> Result<String, ClippyAdapterError> {
    let slash_path = raw.replace('\\', "/");
    if slash_path.starts_with('/') || slash_path.contains('\0') || slash_path.contains(':') {
        return Err(line_error(
            line,
            "message.spans[].file_name must be a relative repository path",
        ));
    }
    let mut components = Vec::new();
    for component in slash_path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                return Err(line_error(
                    line,
                    "message.spans[].file_name must not traverse above the repository",
                ))
            }
            other => components.push(other),
        }
    }
    if components.is_empty() {
        return Err(line_error(
            line,
            "message.spans[].file_name must name a repository file",
        ));
    }
    Ok(components.join("/"))
}

fn normalized_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn finding_from_message(
    envelope: &Map<String, Value>,
    line: usize,
    repo: &str,
) -> Result<Option<Value>, ClippyAdapterError> {
    let message = object(
        envelope
            .get("message")
            .ok_or_else(|| line_error(line, "message is required"))?,
        line,
        "message",
    )?;
    let title = normalized_text(string(message.get("message"), line, "message.message")?);
    let level = string(message.get("level"), line, "message.level")?;
    let spans = message
        .get("spans")
        .and_then(Value::as_array)
        .ok_or_else(|| line_error(line, "message.spans must be an array"))?;
    let code = match message.get("code") {
        Some(Value::Null) => return Ok(None),
        Some(value) => object(value, line, "message.code")?,
        None => return Err(line_error(line, "message.code is required")),
    };
    let rule = string(code.get("code"), line, "message.code.code")?;
    if !rule.starts_with("clippy::") {
        return Ok(None);
    }
    let severity = match level {
        "error" => "high",
        "warning" => "medium",
        "note" | "help" | "failure-note" => "info",
        other => {
            return Err(line_error(
                line,
                format!("message.level {other:?} is unsupported"),
            ))
        }
    };
    let mut primary = spans
        .iter()
        .filter(|span| span.get("is_primary") == Some(&Value::Bool(true)));
    let span = primary
        .next()
        .ok_or_else(|| line_error(line, "message.spans must contain one primary span"))?;
    if primary.next().is_some() {
        return Err(line_error(
            line,
            "message.spans contains more than one primary span",
        ));
    }
    let span = object(span, line, "message.spans[]")?;
    let path = relative_path(
        string(span.get("file_name"), line, "message.spans[].file_name")?,
        line,
    )?;
    let line_start = positive_integer(span.get("line_start"), line, "message.spans[].line_start")?;
    let line_end = positive_integer(span.get("line_end"), line, "message.spans[].line_end")?;
    let column_start = positive_integer(
        span.get("column_start"),
        line,
        "message.spans[].column_start",
    )?;
    let column_end = positive_integer(span.get("column_end"), line, "message.spans[].column_end")?;
    if line_end < line_start || (line_end == line_start && column_end < column_start) {
        return Err(line_error(line, "primary span end precedes its start"));
    }
    let source_lines = span
        .get("text")
        .and_then(Value::as_array)
        .filter(|lines| !lines.is_empty())
        .ok_or_else(|| line_error(line, "message.spans[].text must be a non-empty array"))?;
    let mut source_text = Vec::with_capacity(source_lines.len());
    for source_line in source_lines {
        let source_line = object(source_line, line, "message.spans[].text[]")?;
        source_text.push(
            source_line
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| line_error(line, "message.spans[].text[].text must be a string"))?,
        );
    }
    let snippet = normalized_text(&source_text.join("\n"));
    if snippet.is_empty() {
        return Err(line_error(
            line,
            "primary span has no source text for a stable fingerprint",
        ));
    }
    let fingerprint_parts = (
        CLIPPY_PRODUCER_ID,
        repo,
        path.as_str(),
        rule,
        title.as_str(),
        snippet.as_str(),
        column_start,
        column_end,
    );
    let fingerprint = Uuid::new_v5(
        &CODE_INGEST_NAMESPACE,
        &serde_json::to_vec(&fingerprint_parts)?,
    );
    let id = format!("clippy-v1:{fingerprint}");
    let mut finding = json!({
        "id": id,
        "title": format!("{rule}: {title}"),
        "severity": severity,
        "confidence": "high",
        "categories": ["clippy"],
        "standard": rule,
        "evidence": [{
            "path": path,
            "line": line_start,
            "end_line": line_end,
            "column_start": column_start,
            "column_end": column_end,
            "description": title,
        }],
        "producer_id": CLIPPY_PRODUCER_ID,
        "fingerprint": fingerprint.to_string(),
        "rule": rule,
    });
    if severity == "medium" || severity == "high" {
        finding["failure_scenario"] = json!(title);
    }
    Ok(Some(finding))
}

/// Convert Cargo's Clippy JSON-lines output to validated, unpersisted finding records.
///
/// Only `clippy::` compiler messages become findings. Other documented Cargo records and
/// non-Clippy compiler messages are ignored. Invalid JSON and incomplete Clippy records fail
/// the whole conversion with an input-line reason. The finding fingerprint omits line numbers;
/// the versioned note ID still follows `ingest_findings_json`'s content identity contract.
pub fn ingest_clippy_json_lines(
    input: &[u8],
    provenance: ClippyProvenance<'_>,
    options: CodeIngestOptions<'_>,
) -> Result<CodeIngestBatch, ClippyAdapterError> {
    for (field, value) in [
        ("repo", provenance.repo),
        ("branch", provenance.branch),
        ("commit", provenance.commit),
        ("scope", provenance.scope),
    ] {
        if value.trim().is_empty() {
            return Err(ClippyAdapterError::Provenance { field });
        }
    }
    if input.is_empty() {
        return Err(ClippyAdapterError::EmptyStream);
    }
    let input = std::str::from_utf8(input).map_err(|error| {
        let line = input[..error.valid_up_to()]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count()
            + 1;
        line_error(line, "invalid UTF-8")
    })?;

    let mut findings = Vec::new();
    let mut seen = BTreeMap::new();
    for (index, raw) in input.lines().enumerate() {
        let line_number = index + 1;
        if raw.trim().is_empty() {
            return Err(line_error(line_number, "empty JSON line"));
        }
        let record: Value = serde_json::from_str(raw)
            .map_err(|error| line_error(line_number, format!("invalid JSON: {error}")))?;
        let envelope = object(&record, line_number, "record")?;
        match string(envelope.get("reason"), line_number, "reason")? {
            "compiler-message" => {
                if let Some(finding) = finding_from_message(envelope, line_number, provenance.repo)?
                {
                    let id = finding["id"]
                        .as_str()
                        .expect("adapter constructs a string id");
                    if let Some(previous) = seen.insert(id.to_owned(), finding.clone()) {
                        if previous != finding {
                            return Err(line_error(line_number, format!("ambiguous Clippy fingerprint {id}: distinct diagnostic records share one fingerprint")));
                        }
                        continue;
                    }
                    findings.push(finding);
                }
            }
            "compiler-artifact"
            | "build-script-executed"
            | "build-finished"
            | "future-incompat-report" => {}
            reason => {
                return Err(line_error(
                    line_number,
                    format!("unsupported Cargo reason {reason:?}"),
                ))
            }
        }
    }

    let source_run = format!("{CLIPPY_PRODUCER_ID}:{}", provenance.commit);
    let document = json!({
        "audit": {
            "date": options.observed_at.format("%Y-%m-%d").to_string(),
            "scope": provenance.scope,
            "repo": provenance.repo,
            "branch": provenance.branch,
            "commit": provenance.commit,
            "standards_file": "Clippy lint registry",
            "producer_id": CLIPPY_PRODUCER_ID,
        },
        "findings": findings,
    });
    Ok(ingest_findings_json(
        &serde_json::to_vec(&document)?,
        CodeIngestOptions {
            namespace: options.namespace,
            observed_at: options.observed_at,
            source_run: options
                .source_run
                .filter(|value| !value.trim().is_empty())
                .or(Some(&source_run)),
        },
    )?)
}
