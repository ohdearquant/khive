use khive_pack_code::{ingest_findings_json, CodeIngestBatch, CodeIngestOptions};
use serde_json::Value;

const INPUT: &str = r#"{
    "audit": {"date":"2026-10-01","scope":"helpers","repo":"repo-A","branch":"main","commit":"abc","standards_file":"rules.md"},
    "findings": [{"id":"finding-1","title":"  Stable   Identity  ","severity":"low","confidence":"high",
        "evidence":[{"z":{"b":2,"a":1},"description":"line\n雪"},{"description":"second"}],
        "custom":{"z":[{"b":2,"a":1},true,null],"a":"雪"}}]
}"#;

const REORDERED: &str = r#"{
    "findings": [{"custom":{"a":"\u96ea","z":[{"a":1,"b":2},true,null]},
        "evidence":[{"description":"line\n\u96ea","z":{"a":1,"b":2}},{"description":"second"}],
        "confidence":"high","severity":"low","title":"  Stable   Identity  ","id":"finding-1"}],
    "audit":{"standards_file":"rules.md","commit":"abc","branch":"main","repo":"repo-A","scope":"helpers","date":"2026-10-01"}
}"#;

const EXTERNAL_ID: &str = r#"{"audit_status":null,"categories":null,"confidence":"high","evidence":[{"description":"line\n雪","z":{"a":1,"b":2}},{"description":"second"}],"failure_scenario":null,"id":"finding-1","impact":null,"kind":"code-finding","namespace":"local","normalized_title":"stable identity","priority":null,"project_id":"8ff4cdf5-e1a2-58b7-b4cf-2f4eb5bbff35","raw":{"custom":{"a":"雪","z":[{"a":1,"b":2},true,null]}},"recommendation":null,"refs":null,"repo":"repo-A","schema_version":2,"scope":"helpers","severity":"low","source_run":"fixed-run","standard":null,"verification":null}"#;

fn ingest(input: &[u8]) -> CodeIngestBatch {
    ingest_findings_json(
        input,
        CodeIngestOptions {
            namespace: "local",
            observed_at: "2026-10-01T12:00:00Z".parse().unwrap(),
            source_run: Some("fixed-run"),
        },
    )
    .unwrap()
}

#[test]
fn finding_identity_matches_literal_v1_v2_and_external_bytes() {
    for input in [INPUT, REORDERED] {
        let batch = ingest(input.as_bytes());
        assert_eq!(batch.entities.len(), 1);
        assert_eq!(batch.notes.len(), 1);
        assert_eq!(batch.edges.len(), 1);
        assert_eq!(
            batch.entities[0].id.to_string(),
            "8ff4cdf5-e1a2-58b7-b4cf-2f4eb5bbff35"
        );
        assert_eq!(
            batch.notes[0].id.to_string(),
            "bd8ab4b8-dfa5-57b4-8764-adf4737fc45d"
        );
        assert_eq!(
            batch.edges[0].id.to_string(),
            "a498f9f2-ff32-51b6-9ca6-97ab5df483be"
        );
        let properties = batch.notes[0].properties.as_ref().unwrap();
        assert_eq!(
            properties["legacy_id_v1"].as_str(),
            Some("ebfa0053-cb5e-5275-baf6-29d8a35cd74b")
        );
        assert_eq!(
            properties["external_id"].as_str().unwrap().as_bytes(),
            EXTERNAL_ID.as_bytes()
        );
        assert_eq!(batch.edges[0].source_id, batch.notes[0].id);
        assert_eq!(batch.edges[0].target_id, batch.entities[0].id);
    }
}

#[test]
fn finding_identity_preserves_array_order_and_repository_scope() {
    let baseline = ingest(INPUT.as_bytes());
    let mut doc: Value = serde_json::from_str(INPUT).unwrap();
    doc["findings"][0]["evidence"]
        .as_array_mut()
        .unwrap()
        .swap(0, 1);
    let reordered_array = ingest(&serde_json::to_vec(&doc).unwrap());
    assert_eq!(baseline.entities[0].id, reordered_array.entities[0].id);
    assert_ne!(baseline.notes[0].id, reordered_array.notes[0].id);
    assert_ne!(baseline.edges[0].id, reordered_array.edges[0].id);

    let mut doc: Value = serde_json::from_str(INPUT).unwrap();
    doc["audit"]["repo"] = Value::String("repo-B".into());
    let other_repo = ingest(&serde_json::to_vec(&doc).unwrap());
    assert_ne!(baseline.entities[0].id, other_repo.entities[0].id);
    assert_ne!(baseline.notes[0].id, other_repo.notes[0].id);
    assert_ne!(baseline.edges[0].id, other_repo.edges[0].id);
    assert_eq!(
        baseline.notes[0].properties.as_ref().unwrap()["legacy_id_v1"],
        other_repo.notes[0].properties.as_ref().unwrap()["legacy_id_v1"]
    );
}
