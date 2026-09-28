use chrono::{DateTime, Utc};
use khive_pack_code::{
    ingest_clippy_json_lines, ClippyAdapterError, ClippyProvenance, CodeIngestBatch,
    CodeIngestOptions,
};
use serde_json::{json, Value};

const SOURCE_A: &str = r#"let value = Ok::<_, ()>(r"a  b c").unwrap();"#;
const SOURCE_B: &str = r#"let value = Ok::<_, ()>(r"a b  c").unwrap();"#;

fn diagnostic(source: &str, line: u64) -> Value {
    // ASCII fixtures keep byte and character columns equal. End is exclusive.
    let column_start = source.find(".unwrap()").expect("fixture expression") as u64 + 1;
    let column_end = column_start + ".unwrap()".len() as u64;
    json!({
        "reason": "compiler-message",
        "package_id": "example 0.1.0",
        "message": {
            "message": "used `unwrap()` on a `Result` value",
            "code": {"code": "clippy::unwrap_used", "explanation": null},
            "level": "warning",
            "spans": [{
                "file_name": "src/lib.rs",
                "line_start": line,
                "line_end": line,
                "column_start": column_start,
                "column_end": column_end,
                "is_primary": true,
                "text": [{
                    "text": source,
                    "highlight_start": column_start,
                    "highlight_end": column_end
                }]
            }],
            "children": []
        }
    })
}

fn bytes(records: &[Value]) -> Vec<u8> {
    records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes()
}

fn ingest(records: &[Value], commit: &str) -> Result<CodeIngestBatch, ClippyAdapterError> {
    let observed_at: DateTime<Utc> = "2026-09-26T00:00:00Z"
        .parse()
        .expect("synthetic observation time");
    ingest_clippy_json_lines(
        &bytes(records),
        ClippyProvenance {
            repo: "example",
            branch: "main",
            commit,
            scope: "example-crate",
        },
        CodeIngestOptions {
            namespace: "local",
            observed_at,
            source_run: None,
        },
    )
}

fn props(batch: &CodeIngestBatch, index: usize) -> &Value {
    batch.notes[index]
        .properties
        .as_ref()
        .expect("finding properties")
}

fn build_outcome(batch: &CodeIngestBatch) -> &Value {
    &batch.entities[0]
        .properties
        .as_ref()
        .expect("project properties")["audit_extra"]["clippy_build_outcome"]
}

#[test]
fn literal_whitespace_edits_change_fingerprint() {
    assert_ne!(SOURCE_A, SOURCE_B);
    assert_eq!(SOURCE_A.len(), SOURCE_B.len());
    let a = diagnostic(SOURCE_A, 12);
    let b = diagnostic(SOURCE_B, 12);
    assert_eq!(
        a["message"]["spans"][0]["column_start"],
        b["message"]["spans"][0]["column_start"]
    );
    assert_eq!(
        a["message"]["spans"][0]["column_end"],
        b["message"]["spans"][0]["column_end"]
    );
    let before = ingest(&[a], "synthetic-commit-a").expect("first valid diagnostic");
    let after = ingest(&[b], "synthetic-commit-b").expect("second valid diagnostic");
    assert_eq!(before.notes.len(), 1);
    assert_eq!(after.notes.len(), 1);
    assert_ne!(
        props(&before, 0)["finding_id"],
        props(&after, 0)["finding_id"],
        "semantic raw-string edits must not share a stable source fingerprint"
    );
    assert_ne!(
        props(&before, 0)["raw"]["fingerprint"],
        props(&after, 0)["raw"]["fingerprint"]
    );
}

#[test]
fn conflicting_source_records_are_not_silently_coalesced() {
    let a = diagnostic(SOURCE_A, 12);
    let b = diagnostic(SOURCE_B, 12);
    assert_eq!(
        ingest(std::slice::from_ref(&a), "synthetic-commit")
            .unwrap()
            .notes
            .len(),
        1
    );
    assert_eq!(
        ingest(std::slice::from_ref(&b), "synthetic-commit")
            .unwrap()
            .notes
            .len(),
        1
    );
    // Either retain distinct evidence or explicitly refuse it. It is not an exact replay.
    match ingest(&[a, b], "synthetic-commit") {
        Ok(batch) => {
            assert_eq!(
                batch.notes.len(),
                2,
                "different literal values were silently coalesced"
            );
            assert_ne!(
                props(&batch, 0)["finding_id"],
                props(&batch, 1)["finding_id"]
            );
        }
        Err(ClippyAdapterError::Line { line, reason }) => {
            assert_eq!(line, 2);
            assert!(
                reason.contains("ambiguous") || reason.contains("conflict"),
                "refusal must explain conflicting evidence, got {reason}"
            );
        }
        Err(other) => panic!("unexpected non-conflict refusal: {other}"),
    }
}

#[test]
fn exact_duplicate_records_collapse() {
    let record = diagnostic(SOURCE_A, 12);
    let once = ingest(std::slice::from_ref(&record), "synthetic-commit").unwrap();
    let twice = ingest(&[record.clone(), record], "synthetic-commit").unwrap();
    assert_eq!(once.notes.len(), 1);
    assert_eq!(twice.notes.len(), 1);
    assert_eq!(once.notes[0].id, twice.notes[0].id);
    assert_eq!(once.edges.len(), twice.edges.len());
}

#[test]
fn line_shift_keeps_stable_fingerprint_but_changes_evidence() {
    let before = ingest(&[diagnostic(SOURCE_A, 12)], "synthetic-commit").unwrap();
    let after = ingest(&[diagnostic(SOURCE_A, 32)], "synthetic-commit").unwrap();
    assert_eq!(
        props(&before, 0)["finding_id"],
        props(&after, 0)["finding_id"]
    );
    assert_ne!(props(&before, 0)["evidence"], props(&after, 0)["evidence"]);
    assert_ne!(before.notes[0].id, after.notes[0].id);
}

#[test]
fn non_whitespace_literal_edit_changes_fingerprint() {
    let changed = SOURCE_A.replace("a  b c", "d  b c");
    let before = ingest(&[diagnostic(SOURCE_A, 12)], "synthetic-commit").unwrap();
    let after = ingest(&[diagnostic(&changed, 12)], "synthetic-commit").unwrap();
    assert_ne!(
        props(&before, 0)["finding_id"],
        props(&after, 0)["finding_id"]
    );
}

#[test]
fn same_fingerprint_at_distinct_lines_is_explicitly_refused() {
    let err = ingest(
        &[diagnostic(SOURCE_A, 12), diagnostic(SOURCE_A, 32)],
        "synthetic-commit",
    )
    .expect_err("same stable fingerprint at distinct spans is ambiguous");
    match err {
        ClippyAdapterError::Line { line, reason } => {
            assert_eq!(line, 2);
            assert!(reason.contains("ambiguous Clippy fingerprint"), "{reason}");
        }
        other => panic!("expected a line-specific ambiguity, got {other}"),
    }
}

#[test]
fn all_documented_severity_levels_remain_mapped() {
    for (level, expected) in [
        ("error", "high"),
        ("warning", "medium"),
        ("note", "info"),
        ("help", "info"),
        ("failure-note", "info"),
    ] {
        let mut record = diagnostic(SOURCE_A, 12);
        record["message"]["level"] = json!(level);
        let batch = ingest(&[record], "synthetic-commit").unwrap();
        assert_eq!(props(&batch, 0)["severity"], expected);
    }
}

#[test]
fn lexical_path_and_ambiguous_span_refusals_remain() {
    for path in [
        "/outside/lib.rs",
        "../lib.rs",
        r"C:\outside\lib.rs",
        "\0.rs",
    ] {
        let mut record = diagnostic(SOURCE_A, 12);
        record["message"]["spans"][0]["file_name"] = json!(path);
        assert!(
            ingest(&[record], "synthetic-commit").is_err(),
            "accepted {path:?}"
        );
    }
    let mut record = diagnostic(SOURCE_A, 12);
    let primary = record["message"]["spans"][0].clone();
    record["message"]["spans"]
        .as_array_mut()
        .unwrap()
        .push(primary);
    let err = ingest(&[record], "synthetic-commit").unwrap_err();
    assert!(
        err.to_string().contains("more than one primary span"),
        "{err}"
    );
}

#[test]
fn late_unknown_record_refuses_the_whole_conversion() {
    let err = ingest(
        &[
            diagnostic(SOURCE_A, 12),
            json!({"reason": "unknown-cargo-record"}),
        ],
        "synthetic-commit",
    )
    .expect_err("late invalid record must not return a partial batch");
    match err {
        ClippyAdapterError::Line { line, reason } => {
            assert_eq!(line, 2);
            assert!(reason.contains("unsupported Cargo reason"), "{reason}");
        }
        other => panic!("expected line-specific error, got {other}"),
    }
}

#[test]
fn failed_build_marker_is_not_a_success_attestation() {
    let batch = ingest(
        &[json!({"reason": "build-finished", "success": false})],
        "synthetic-commit",
    )
    .expect("a failed build still returns its diagnostic batch");
    assert_eq!(batch.entities.len(), 1);
    assert!(batch.notes.is_empty());
    assert!(batch.edges.is_empty());
    assert_eq!(build_outcome(&batch), "finished_failed");
}

#[test]
fn successful_and_unfinished_streams_have_distinct_build_outcomes() {
    let finding = diagnostic(SOURCE_A, 12);
    let complete = ingest(
        &[
            finding.clone(),
            json!({"reason": "build-finished", "success": true}),
        ],
        "synthetic-commit",
    )
    .expect("successful terminal marker");
    let unfinished = ingest(&[finding], "synthetic-commit")
        .expect("partial diagnostics remain available without an attestation");
    assert_eq!(complete.notes.len(), 1);
    assert_eq!(unfinished.notes.len(), 1);
    assert_eq!(build_outcome(&complete), "finished_ok");
    assert_eq!(build_outcome(&unfinished), "no_marker");
}

#[test]
fn malformed_or_nonterminal_build_marker_is_refused() {
    for records in [
        vec![json!({"reason": "build-finished"})],
        vec![json!({"reason": "build-finished", "success": "false"})],
        vec![
            json!({"reason": "build-finished", "success": true}),
            diagnostic(SOURCE_A, 12),
        ],
    ] {
        let error = ingest(&records, "synthetic-commit")
            .expect_err("the terminal marker must be typed and final");
        let message = error.to_string();
        assert!(
            message.contains("build-finished"),
            "marker refusal must explain the defect: {message}"
        );
    }
}
