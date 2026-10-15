use super::{check_note_fields_at, scan};
use crate::atomic_message::{create_notes_atomic, AtomicNoteSpec};
use crate::{KhiveRuntime, Namespace, RuntimeError};
use serde_json::json;

const SECRET: &str = "AKIAFAKEKEY1234567890";

fn assert_secret(error: RuntimeError, location: &str) {
    let RuntimeError::SecretDetected(matched) = error else {
        panic!("expected secret refusal at {location}, got {error:?}");
    };
    let mut expected = scan(SECRET).expect("known credential fixture");
    expected.location = Some(location.to_owned());
    assert_eq!(matched, expected);
}

#[test]
fn note_fields_preserve_first_refusal_and_structured_property_location() {
    let properties = json!({"nested": [null, {"value": SECRET}]});
    for (content, name, field) in [
        (SECRET, Some(SECRET), "content"),
        ("ordinary text", Some(SECRET), "name"),
        ("ordinary text", None, "properties"),
    ] {
        let error = check_note_fields_at(content, name, Some(&properties), "note[3]")
            .expect_err("first secret-bearing field must refuse");
        assert_secret(error, &format!("note[3].{field}"));
    }
}

#[test]
fn note_fields_accept_optional_and_benign_structured_values_without_reservation_policy() {
    assert!(check_note_fields_at("ordinary text", None, None, "note").is_ok());
    for properties in [
        json!(null),
        json!({"nested": [null, true, 7, {"path": "docs/guide.md"}]}),
        json!({"khive:secret_gate": true}),
    ] {
        assert!(
            check_note_fields_at("ordinary text", Some("title"), Some(&properties), "note").is_ok()
        );
    }
}

#[tokio::test]
async fn note_writers_keep_reserved_key_before_field_scan() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    for reserved in [true, false] {
        let properties = reserved.then(|| json!({"khive:secret_gate": true}));
        let errors = [
            runtime
                .create_note(
                    &token,
                    "observation",
                    Some(SECRET),
                    SECRET,
                    None,
                    properties.clone(),
                    vec![],
                )
                .await
                .unwrap_err(),
            runtime
                .try_create_note(
                    &token,
                    "observation",
                    Some(SECRET),
                    SECRET,
                    properties.clone(),
                )
                .await
                .unwrap_err(),
            create_notes_atomic(
                &runtime,
                vec![AtomicNoteSpec {
                    token: &token,
                    id: None,
                    kind: "observation",
                    name: Some(SECRET),
                    content: SECRET,
                    properties,
                }],
            )
            .await
            .unwrap_err(),
        ];
        for (index, error) in errors.into_iter().enumerate() {
            if reserved {
                assert!(matches!(error, RuntimeError::InvalidInput(ref message)
                    if message.contains("khive:secret_gate") && message.contains("runtime-owned")));
            } else {
                assert_secret(
                    error,
                    if index == 2 {
                        "note[0].content"
                    } else {
                        "note.content"
                    },
                );
            }
        }
    }
    assert!(runtime
        .list_notes(&token, None, 10, 0)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn atomic_note_scan_locates_later_record_before_any_write() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    let error = create_notes_atomic(
        &runtime,
        vec![
            AtomicNoteSpec {
                token: &token,
                id: None,
                kind: "observation",
                name: None,
                content: "ordinary first note",
                properties: None,
            },
            AtomicNoteSpec {
                token: &token,
                id: None,
                kind: "observation",
                name: Some(SECRET),
                content: "ordinary second note",
                properties: Some(json!({"nested": [SECRET]})),
            },
        ],
    )
    .await
    .unwrap_err();
    assert_secret(error, "note[1].name");
    assert!(runtime
        .list_notes(&token, None, 10, 0)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn proposal_note_preparation_keeps_field_scan_before_reserved_key() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    let mut args = json!({"kind": "observation", "content": SECRET,
        "properties": {"khive:secret_gate": true}});
    let error = crate::atomic_prepare::prepare_add_note(&runtime, &token, &args)
        .await
        .unwrap_err();
    assert_secret(error, "note.content");
    args["content"] = json!("ordinary text");
    let error = crate::atomic_prepare::prepare_add_note(&runtime, &token, &args)
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::InvalidInput(ref message)
        if message.contains("khive:secret_gate") && message.contains("runtime-owned")));
}
