use std::path::Path;
use std::process::{Command, Output};

use serde_json::{json, Value};
use tempfile::TempDir;
use uuid::Uuid;

const TIMESTAMP: &str = "2026-01-01T00:00:00Z";

fn command(tmp: &TempDir) -> Command {
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
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("KHIVE_VOLUME_LOCK_DIR", tmp.path().join("volume-locks"));
    command
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn import(tmp: &TempDir, archive: &Value, db: &Path) -> Value {
    let source = tmp.path().join("input.json");
    std::fs::write(&source, serde_json::to_vec(archive).unwrap()).unwrap();
    let output = command(tmp)
        .args(["kg", "import"])
        .arg(source)
        .args(["--format", "archive", "--force", "--db"])
        .arg(db)
        .output()
        .unwrap();
    assert_success(&output);
    serde_json::from_slice(&output.stdout).unwrap()
}

fn export(tmp: &TempDir, db: &Path) -> Value {
    let target = tmp.path().join("export.json");
    let output = command(tmp)
        .args(["kg", "export", "--db"])
        .arg(db)
        .arg(&target)
        .output()
        .unwrap();
    assert_success(&output);
    serde_json::from_slice(&std::fs::read(target).unwrap()).unwrap()
}

fn entity(id: Uuid, name: &str) -> Value {
    json!({
        "id":id,"kind":"document","name":name,
        "created_at":TIMESTAMP,"updated_at":TIMESTAMP,"tags":[]
    })
}

fn edge(id: u128, source: Uuid, target: Uuid, relation: &str) -> Value {
    json!({
        "edge_id":Uuid::from_u128(id),"source":source,"target":target,
        "relation":relation,"weight":0.7,
        "created_at":TIMESTAMP,"updated_at":TIMESTAMP
    })
}

fn archive(entities: Vec<Value>, edges: Vec<Value>) -> Value {
    json!({
        "format":"khive-kg","version":"0.1","namespace":"local",
        "exported_at":TIMESTAMP,"entities":entities,"edges":edges
    })
}

#[test]
fn cli_reports_all_three_skip_causes_and_exports_only_the_retained_edge() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("target.db");
    let source = Uuid::from_u128(1);
    let target = Uuid::from_u128(2);
    let absent = Uuid::from_u128(3);
    let mut valid = edge(101, source, target, "links_to");
    valid["properties"] = json!({"retained":true});
    let input = archive(
        vec![entity(source, "Source"), entity(target, "Target")],
        vec![
            edge(100, absent, source, "links_to"),
            valid.clone(),
            edge(102, source, absent, "depends_on"),
            edge(103, source, target, "annotates"),
        ],
    );
    let summary = import(&tmp, &input, &db);
    assert_eq!(
        summary,
        json!({
            "entities_imported":2,"edges_imported":1,"edges_skipped":3,
            "entities_inserted":2,"entities_updated":0,"entities_skipped":0,
            "edges_inserted":1,"edges_updated":0,
            "embedding_truncation":{"truncated":0,"discarded_bytes":0},
            "skipped_edges":[
                {"edge_id":Uuid::from_u128(100),"source":absent,"target":source,
                 "relation":"links_to","cause":"missing_source"},
                {"edge_id":Uuid::from_u128(102),"source":source,"target":absent,
                 "relation":"depends_on","cause":"missing_target"},
                {"edge_id":Uuid::from_u128(103),"source":source,"target":target,
                 "relation":"annotates","cause":"endpoint_contract",
                 "error":format!("invalid input: annotates source {source} must be a note")}
            ]
        })
    );
    let exported = export(&tmp, &db);
    assert_eq!(exported["edges"], json!([valid]));
    let mut entity_ids: Vec<Uuid> = exported["entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entity| Uuid::parse_str(entity["id"].as_str().unwrap()).unwrap())
        .collect();
    entity_ids.sort();
    assert_eq!(entity_ids, vec![source, target]);
}

#[test]
fn cli_omits_empty_details_for_all_valid_and_empty_imports() {
    let a = Uuid::from_u128(1);
    let b = Uuid::from_u128(2);
    for (input, entities, edges) in [
        (archive(vec![], vec![]), 0, 0),
        (
            archive(
                vec![entity(a, "A"), entity(b, "B")],
                vec![edge(101, a, b, "links_to")],
            ),
            2,
            1,
        ),
    ] {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("target.db");
        assert_eq!(
            import(&tmp, &input, &db),
            json!({
                "entities_imported":entities,"edges_imported":edges,"edges_skipped":0,
                "entities_inserted":entities,"entities_updated":0,"entities_skipped":0,
                "edges_inserted":edges,"edges_updated":0,
                "embedding_truncation":{"truncated":0,"discarded_bytes":0}
            })
        );
        let exported = export(&tmp, &db);
        assert_eq!(exported["entities"].as_array().unwrap().len(), entities);
        assert_eq!(exported["edges"].as_array().unwrap().len(), edges);
        if edges == 1 {
            assert_eq!(exported["edges"][0]["edge_id"], json!(Uuid::from_u128(101)));
        }
    }
}

#[test]
fn cli_preserves_duplicate_skip_entries_and_both_missing_source_precedence() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("target.db");
    let present = Uuid::from_u128(1);
    let absent = Uuid::from_u128(3);
    let other_absent = Uuid::from_u128(4);
    let repeated = edge(111, absent, present, "links_to");
    let input = archive(
        vec![entity(present, "Present")],
        vec![
            edge(110, present, absent, "depends_on"),
            repeated.clone(),
            edge(112, absent, present, "links_to"),
            repeated,
            edge(113, absent, other_absent, "extends"),
        ],
    );
    assert_eq!(
        import(&tmp, &input, &db),
        json!({
            "entities_imported":1,"edges_imported":0,"edges_skipped":5,
            "entities_inserted":1,"entities_updated":0,"entities_skipped":0,
            "edges_inserted":0,"edges_updated":0,
            "embedding_truncation":{"truncated":0,"discarded_bytes":0},
            "skipped_edges":[
                {"edge_id":Uuid::from_u128(110),"source":present,"target":absent,
                 "relation":"depends_on","cause":"missing_target"},
                {"edge_id":Uuid::from_u128(111),"source":absent,"target":present,
                 "relation":"links_to","cause":"missing_source"},
                {"edge_id":Uuid::from_u128(112),"source":absent,"target":present,
                 "relation":"links_to","cause":"missing_source"},
                {"edge_id":Uuid::from_u128(111),"source":absent,"target":present,
                 "relation":"links_to","cause":"missing_source"},
                {"edge_id":Uuid::from_u128(113),"source":absent,"target":other_absent,
                 "relation":"extends","cause":"missing_source"}
            ]
        })
    );
    let exported = export(&tmp, &db);
    assert_eq!(exported["edges"], json!([]));
    assert_eq!(exported["entities"].as_array().unwrap().len(), 1);
}
