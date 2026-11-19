use super::{preflight_secret_gate, CodeIngestBatch, Note};
use serde_json::json;

#[test]
fn ingest_preflight_scans_note_fields_before_reserved_key_with_indexed_location() {
    let secret = "AKIAFAKEKEY1234567890";
    let mut batch = CodeIngestBatch {
        entities: vec![],
        notes: vec![
            Note::new("local", "finding", "ordinary first note"),
            Note::new("local", "finding", secret)
                .with_name(secret)
                .with_properties(json!({
                    "khive:secret_gate": true,
                    "nested": [null, {"value": secret}]
                })),
        ],
        edges: vec![],
    };
    for field in ["content", "name", "properties"] {
        let error = preflight_secret_gate(&batch).unwrap_err().to_string();
        assert!(error.contains(&format!("in note[1].{field}")), "{error}");
        assert!(!error.contains("runtime-owned"), "{error}");
        match field {
            "content" => batch.notes[1].content = "ordinary second note".to_owned(),
            "name" => batch.notes[1].name = None,
            _ => batch.notes[1].properties = Some(json!({"khive:secret_gate": true})),
        }
    }
    let error = preflight_secret_gate(&batch).unwrap_err().to_string();
    assert!(
        error.contains("khive:secret_gate") && error.contains("runtime-owned"),
        "{error}"
    );
    batch.notes[1].properties = Some(json!({"nested": [null, true, 7, "docs/guide.md"]}));
    assert!(preflight_secret_gate(&batch).unwrap().is_empty());
}
