//! Asking for fixes must preserve bytes when validation finds nothing fixable.

use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

const ALPHA: &str =
    r#"{ "name": "Alpha", "kind": "concept", "id": "11111111-1111-1111-1111-111111111111" }"#;
const BETA: &str =
    r#"{ "name": "Beta", "kind": "concept", "id": "22222222-2222-2222-2222-222222222222" }"#;

fn fixture(entities: &str, edges: &str) -> TempDir {
    let directory = TempDir::new().unwrap();
    let kg = directory.path().join(".khive/kg");
    std::fs::create_dir_all(&kg).unwrap();
    std::fs::write(kg.join("entities.ndjson"), entities).unwrap();
    std::fs::write(kg.join("edges.ndjson"), edges).unwrap();
    directory
}

fn validate(directory: &TempDir, strict: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
    for (key, _) in std::env::vars_os() {
        if key
            .to_str()
            .is_some_and(|key| key.starts_with("KHIVE_") && key != "KHIVE_TEST_HARNESS")
        {
            command.env_remove(key);
        }
    }
    command
        .current_dir(directory.path())
        .env("HOME", directory.path())
        .env(
            "KHIVE_VOLUME_LOCK_DIR",
            directory.path().join("volume-locks"),
        )
        .args(["kg", "validate", "--repo"])
        .arg(directory.path())
        .args(["--fix", "--no-rules", "--format", "json"]);
    if strict {
        command.arg("--strict");
    }
    command.output().expect("validate private KG fixture")
}

fn report(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid report: {error}; stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn assert_bytes(directory: &TempDir, entities: &str, edges: &str) {
    let kg = directory.path().join(".khive/kg");
    assert_eq!(
        std::fs::read(kg.join("entities.ndjson")).unwrap(),
        entities.as_bytes()
    );
    assert_eq!(
        std::fs::read(kg.join("edges.ndjson")).unwrap(),
        edges.as_bytes()
    );
    assert!(!kg.join("notes.ndjson").exists());
}

#[test]
fn fix_preserves_valid_sorted_and_empty_inputs_byte_for_byte() {
    let sorted = format!("\n{ALPHA}\r\n \n{BETA}\n");
    let edge = " { \"relation\": \"extends\", \"source\": \"11111111-1111-1111-1111-111111111111\", \"target\": \"22222222-2222-2222-2222-222222222222\" } \n";
    for (entities, edges) in [(sorted.as_str(), edge), ("", ""), (" \r\n\t\n", "\n \t")] {
        let directory = fixture(entities, edges);
        let output = validate(&directory, true);
        let report = report(&output);
        assert!(output.status.success(), "{report}");
        assert_eq!(report["summary"]["passed"], true);
        assert!(!String::from_utf8_lossy(&output.stderr).contains("applied fix"));
        assert_bytes(&directory, entities, edges);
    }
}

#[test]
fn fix_preserves_unfixable_only_failures_and_their_exit_status() {
    let unknown_kind = ALPHA.replace("concept", "not-a-registered-kind");
    for entities in [unknown_kind.as_str(), "malformed JSON\n"] {
        let directory = fixture(entities, " \n");
        let output = validate(&directory, false);
        let report = report(&output);
        assert_eq!(output.status.code(), Some(1), "{report}");
        assert_eq!(report["summary"]["passed"], false);
        assert!(report["summary"]["errors"].as_u64().unwrap() > 0);
        assert!(!String::from_utf8_lossy(&output.stderr).contains("applied fix"));
        assert_bytes(&directory, entities, " \n");
    }
}

#[test]
fn fix_still_sorts_violations_and_a_second_fix_preserves_the_result() {
    let directory = fixture(&format!("{BETA}\n{ALPHA}\n"), "");
    let output = validate(&directory, false);
    let initial = report(&output);
    assert!(output.status.success(), "{initial}");
    assert!(initial["rules"]
        .as_array()
        .unwrap()
        .iter()
        .any(|rule| { rule["id"] == "sort-order" && rule["passed"] == false }));
    assert!(String::from_utf8_lossy(&output.stderr).contains("applied fix"));
    let kg = directory.path().join(".khive/kg");
    let entities = std::fs::read_to_string(kg.join("entities.ndjson")).unwrap();
    let edges = std::fs::read_to_string(kg.join("edges.ndjson")).unwrap();
    let ids: Vec<String> = entities
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(
        ids,
        [
            "11111111-1111-1111-1111-111111111111",
            "22222222-2222-2222-2222-222222222222"
        ]
    );
    let output = validate(&directory, true);
    let final_report = report(&output);
    assert!(output.status.success(), "{final_report}");
    assert_eq!(final_report["summary"]["warnings"], 0);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("applied fix"));
    assert_bytes(&directory, &entities, &edges);
}
