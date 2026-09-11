//! Empty input must be explicit without rejecting valid graphs with no edges.

use std::process::{Command, Output};

use serde_json::{json, Value};
use tempfile::TempDir;

fn fixture(entities: &str, edges: &str, notes: Option<&str>) -> TempDir {
    let tmp = TempDir::new().expect("create private fixture");
    let kg_dir = tmp.path().join(".khive/kg");
    std::fs::create_dir_all(&kg_dir).expect("create KG directory");
    std::fs::write(kg_dir.join("entities.ndjson"), entities).expect("write entities");
    std::fs::write(kg_dir.join("edges.ndjson"), edges).expect("write edges");
    if let Some(notes) = notes {
        std::fs::write(kg_dir.join("notes.ndjson"), notes).expect("write notes");
    }
    tmp
}

fn validate(tmp: &TempDir, format: &str, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kkernel"))
        .args(["kg", "validate", "--repo"])
        .arg(tmp.path())
        .args(["--format", format, "--no-rules"])
        .args(extra)
        .output()
        .expect("run validator against private fixture")
}

fn report(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON report: {error}; stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn kg_validate_marks_zero_record_inputs_empty_even_in_strict_mode() {
    for (entities, edges, notes) in [("", "", None), (" \n\t\n", "\r\n ", Some("\n\t"))] {
        let tmp = fixture(entities, edges, notes);
        for extra in [&[][..], &["--strict"][..]] {
            let output = validate(&tmp, "json", extra);
            let report = report(&output);
            assert!(output.status.success(), "{report}");
            assert_eq!(report["summary"]["passed"], true);
            assert_eq!(report["summary"]["empty"], true);
            assert_eq!(report["summary"]["entities"], 0);
            assert_eq!(report["summary"]["edges"], 0);
            assert!(report["rules"]
                .as_array()
                .unwrap()
                .iter()
                .all(|rule| rule["passed"] == true));
        }
    }
}

#[test]
fn kg_validate_entity_only_and_note_only_graphs_are_nonempty() {
    let entity = json!({
        "id": "11111111-1111-1111-1111-111111111111",
        "kind": "concept",
        "name": "Private fixture entity"
    })
    .to_string();
    let note = json!({
        "id": "22222222-2222-2222-2222-222222222222",
        "kind": "observation",
        "content": "Private fixture note"
    })
    .to_string();

    for (entities, notes, entity_count) in
        [(entity.as_str(), None, 1), ("", Some(note.as_str()), 0)]
    {
        let tmp = fixture(entities, "", notes);
        let output = validate(&tmp, "json", &["--strict"]);
        let report = report(&output);
        assert!(output.status.success(), "{report}");
        assert_eq!(report["summary"]["passed"], true);
        assert_eq!(report["summary"]["empty"], false);
        assert_eq!(report["summary"]["entities"], entity_count);
        assert_eq!(report["summary"]["edges"], 0);
    }
}

#[test]
fn kg_validate_nonempty_invalid_edges_remain_failed_and_nonempty() {
    let edge = json!({
        "source": "11111111-1111-1111-1111-111111111111",
        "target": "22222222-2222-2222-2222-222222222222",
        "relation": "extends"
    })
    .to_string();
    for edges in [edge.as_str(), "malformed NDJSON\n"] {
        let tmp = fixture("", edges, None);
        let output = validate(&tmp, "json", &[]);
        let report = report(&output);
        assert_eq!(output.status.code(), Some(1), "{report}");
        assert_eq!(report["summary"]["passed"], false);
        assert_eq!(report["summary"]["empty"], false);
        assert_eq!(report["summary"]["edges"], 1);
    }
}

#[test]
fn kg_validate_reports_empty_graph_in_text_quiet_and_github_formats() {
    let tmp = fixture("", "", None);
    for extra in [&[][..], &["--quiet"][..]] {
        let output = validate(&tmp, "text", extra);
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).expect("text output");
        assert!(stdout.contains("; empty graph"), "{stdout}");
    }

    let output = validate(&tmp, "github", &[]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("GitHub output"),
        "::notice ::Empty graph: no entity, edge, or note records read\n"
    );
}
