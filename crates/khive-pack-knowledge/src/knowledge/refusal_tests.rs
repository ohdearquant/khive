use super::super::schema::UpsertAtomsParams;
use super::*;

fn old_atom() -> Atom {
    Atom {
        id: uuid::Uuid::new_v4(),
        namespace: "other".into(),
        slug: "old-slug".into(),
        name: "old-name".into(),
        content: "short legacy body".into(),
        tags: r#"["kept"]"#.into(),
        properties: Some(r#"{"old":true}"#.into()),
        status: Some("deprecated".into()),
        source_uri: Some("https://example.test/old".into()),
        source_type: Some("manual".into()),
        finalized: true,
        created_at: 10,
        updated_at: 20,
        deleted_at: None,
    }
}

fn submitted(value: Value) -> AtomWrite {
    let mut params: UpsertAtomsParams = serde_json::from_value(json!({"atoms": [value]})).unwrap();
    params.atoms.remove(0)
}

#[test]
fn refusal_candidate_preserves_omitted_patch_fields_and_clears_explicit_nulls() {
    let old = old_atom();
    let omitted = submitted(json!({"slug":" old-slug ","name":"New","content":" new body "}));
    let value = candidate(&omitted, &old);
    assert_eq!(
        value,
        json!({"slug":"old-slug","name":"New","content":"new body",
            "tags":[],"properties":null,"source_uri":"https://example.test/old",
            "source_type":"manual","finalized":true})
    );
    let cleared = submitted(json!({"slug":"old-slug","name":"New","content":"new body",
            "source_uri":null,"source_type":null,"finalized":null}));
    let value = candidate(&cleared, &old);
    assert_eq!(value["source_uri"], Value::Null);
    assert_eq!(value["source_type"], Value::Null);
    assert_eq!(value["finalized"], false);
    let blanks = submitted(json!({"slug":"old-slug","name":"New","content":"new body",
            "source_uri":"  ","source_type":"  "}));
    assert_eq!(
        candidate(&blanks, &old)["source_uri"],
        "https://example.test/old"
    );
}

#[test]
fn properties_only_candidate_retains_legacy_content_without_revalidation() {
    let old = old_atom();
    let input = submitted(json!({"id":old.id,"properties":{"new":true}}));
    assert_eq!(
        candidate(&input, &old),
        json!({"slug":"old-slug","name":"old-name",
            "content":"short legacy body","tags":["kept"],"properties":{"new":true},
            "source_uri":"https://example.test/old","source_type":"manual","finalized":true})
    );
    assert!(validate_submission(&input, 0, false).is_ok());
}

#[test]
fn refusal_candidate_excludes_row_identity_times_and_derived_lifecycle() {
    let mut old = old_atom();
    let input = submitted(json!({"slug":"old-slug","name":"New","content":"new body"}));
    let before = candidate(&input, &old);
    old.id = uuid::Uuid::new_v4();
    old.namespace = "changed".into();
    old.status = Some("reviewed".into());
    old.created_at += 1;
    old.updated_at += 1;
    assert_eq!(candidate(&input, &old), before);
}
