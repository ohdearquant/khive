use chrono::{DateTime, Utc};
use khive_pack_code::{
    ingest_clippy_json_lines, ClippyProvenance, CodeIngestOptions, CLIPPY_PRODUCER_ID,
};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/clippy-mini.jsonl");

fn provenance() -> ClippyProvenance<'static> {
    ClippyProvenance {
        repo: "example",
        branch: "main",
        commit: "example-commit",
        scope: "example-crate",
    }
}

fn options() -> CodeIngestOptions<'static> {
    let observed_at: DateTime<Utc> = "2026-09-25T12:00:00Z".parse().expect("valid timestamp");
    CodeIngestOptions {
        namespace: "local",
        observed_at,
        source_run: None,
    }
}

fn properties(note: &khive_storage::Note) -> &Value {
    note.properties.as_ref().expect("finding properties")
}

#[test]
fn maps_fixture_to_validated_findings_without_persistence() {
    let batch = ingest_clippy_json_lines(FIXTURE.as_bytes(), provenance(), options())
        .expect("valid fixture");
    assert_eq!(batch.entities.len(), 1);
    assert_eq!(batch.notes.len(), 2);
    assert_eq!(batch.edges.len(), 2);
    assert_eq!(
        batch.entities[0].properties.as_ref().unwrap()["audit_extra"]["producer_id"],
        CLIPPY_PRODUCER_ID
    );

    let warning = properties(&batch.notes[0]);
    assert_eq!(warning["severity"], "medium");
    assert_eq!(warning["standard"], "clippy::needless_borrow");
    assert_eq!(warning["raw"]["producer_id"], CLIPPY_PRODUCER_ID);
    assert_eq!(warning["evidence"][0]["path"], "src/lib.rs");
    assert_eq!(warning["evidence"][0]["line"], 12);
    assert_eq!(warning["evidence"][0]["column_start"], 5);
    assert_eq!(warning["evidence"][0]["column_end"], 13);
    assert_eq!(warning["failure_scenario"], "this borrow is unnecessary");

    let error = properties(&batch.notes[1]);
    assert_eq!(error["severity"], "high");
    assert_eq!(error["standard"], "clippy::unwrap_used");
    assert_eq!(error["evidence"][0]["line"], 25);
    assert_eq!(
        error["failure_scenario"],
        "used `unwrap()` on a `Result` value"
    );
    for (note, edge) in batch.notes.iter().zip(&batch.edges) {
        assert_eq!(edge.source_id, note.id);
        assert_eq!(edge.target_id, batch.entities[0].id);
    }
}

#[test]
fn fingerprint_survives_unrelated_lines_added_elsewhere() {
    let first = ingest_clippy_json_lines(FIXTURE.as_bytes(), provenance(), options())
        .expect("first ingest");
    let shifted = FIXTURE
        .lines()
        .map(|line| {
            let mut record: Value = serde_json::from_str(line).expect("fixture line");
            if record["reason"] == "compiler-message"
                && record["message"]["code"]["code"]
                    .as_str()
                    .is_some_and(|code| code.starts_with("clippy::"))
            {
                let span = &mut record["message"]["spans"][0];
                let start = span["line_start"].as_u64().unwrap();
                let end = span["line_end"].as_u64().unwrap();
                span["line_start"] = json!(start + 20);
                span["line_end"] = json!(end + 20);
            }
            record.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let moved = ingest_clippy_json_lines(shifted.as_bytes(), provenance(), options())
        .expect("shifted ingest");
    assert_eq!(first.notes.len(), 2);
    assert_eq!(moved.notes.len(), first.notes.len());
    for (before, after) in first.notes.iter().zip(&moved.notes) {
        assert_eq!(
            properties(before)["finding_id"],
            properties(after)["finding_id"]
        );
        assert_eq!(
            properties(before)["raw"]["fingerprint"],
            properties(after)["raw"]["fingerprint"]
        );
        assert_ne!(
            properties(before)["evidence"],
            properties(after)["evidence"]
        );
    }
    let replay = ingest_clippy_json_lines(FIXTURE.as_bytes(), provenance(), options())
        .expect("exact replay");
    assert_eq!(first.notes[0].id, replay.notes[0].id);
}

#[test]
fn malformed_lines_and_incomplete_lints_are_refused_with_line_reasons() {
    for bad in [
        "not-json",
        "{}",
        "{\"reason\":\"compiler-message\",\"message\":{\"code\":{\"code\":\"clippy::x\"}}}",
        "{\"reason\":\"unknown\"}",
        "{\"reason\":\"build-finished\"}\n\n{\"reason\":\"build-finished\"}",
    ] {
        let error = ingest_clippy_json_lines(bad.as_bytes(), provenance(), options())
            .expect_err("bad input must fail");
        let reason = error.to_string();
        assert!(reason.contains("line "), "must name line: {reason}");
        assert!(reason.len() > 25, "must explain refusal: {reason}");
    }
}

#[test]
fn primary_span_must_stay_inside_repository() {
    let outside = FIXTURE.replace("src/lib.rs", "/outside/src/lib.rs");
    let error = ingest_clippy_json_lines(outside.as_bytes(), provenance(), options())
        .expect_err("absolute diagnostic path must fail");
    assert!(error.to_string().contains("relative repository path"));
}

#[test]
fn invalid_utf8_names_the_affected_line() {
    let mut bytes = b"{\"reason\":\"build-finished\"}\n".to_vec();
    bytes.push(0xff);
    let error = ingest_clippy_json_lines(&bytes, provenance(), options())
        .expect_err("invalid UTF-8 must fail");
    assert!(error.to_string().contains("line 2: invalid UTF-8"));
}

#[test]
fn repo_scope_and_local_source_text_affect_identity() {
    let original =
        ingest_clippy_json_lines(FIXTURE.as_bytes(), provenance(), options()).expect("original");
    let changed_text = FIXTURE.replace("consume(&value);", "consume(value);");
    let edited = ingest_clippy_json_lines(changed_text.as_bytes(), provenance(), options())
        .expect("edited source");
    assert_ne!(
        properties(&original.notes[0])["finding_id"],
        properties(&edited.notes[0])["finding_id"]
    );
    let elsewhere = ingest_clippy_json_lines(
        FIXTURE.as_bytes(),
        ClippyProvenance {
            repo: "other",
            ..provenance()
        },
        options(),
    )
    .expect("other repo");
    assert_ne!(original.entities[0].id, elsewhere.entities[0].id);
    assert_ne!(original.notes[0].id, elsewhere.notes[0].id);
    assert_ne!(
        properties(&original.notes[0])["finding_id"],
        properties(&elsewhere.notes[0])["finding_id"]
    );
}
