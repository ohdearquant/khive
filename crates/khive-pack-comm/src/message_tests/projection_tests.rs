fn make_note(namespace: &str, content: &str, props: Option<Value>) -> Note {
    let mut n = Note::new(namespace, "message", content);
    n.properties = props;
    n
}

#[test]
fn promotes_from_to_subject_when_present() {
    let note = make_note(
        "local",
        "hello",
        Some(json!({
            "from_actor": "lambda:khive",
            "to_actor": "lambda:casey",
            "subject": "Status update",
            "direction": "inbound",
            "read": false,
        })),
    );
    let v = note_to_message_json(&note);
    assert_eq!(v["from"], json!("lambda:khive"));
    assert_eq!(v["to"], json!("lambda:casey"));
    assert_eq!(v["subject"], json!("Status update"));
    assert_eq!(v["direction"], json!("inbound"));
    assert_eq!(v["read"], json!(false));
    assert!(v["content"].is_string());
    assert!(v["properties"].is_object());
}

#[test]
fn message_ids_are_round_trippable_with_separate_compact_display_id() {
    let note = make_note("local", "hello", None);
    let full_id = note.id.as_hyphenated().to_string();
    let v = note_to_message_json(&note);

    assert_eq!(v["id"], json!(full_id));
    assert_eq!(v["full_id"], v["id"]);
    assert_eq!(v["short_id"], json!(short_id(note.id)));
}

#[test]
fn from_falls_back_to_namespace_when_from_actor_absent() {
    let note = make_note(
        "legacy-ns",
        "old message",
        Some(json!({ "to_actor": "lambda:casey" })),
    );
    let v = note_to_message_json(&note);
    assert_eq!(v["from"], json!("legacy-ns"));
}

#[test]
fn preview_is_single_line_and_truncated_for_long_content() {
    let long_body = "word ".repeat(40);
    let note = make_note("local", long_body.trim(), None);
    let v = note_to_message_json(&note);
    let preview = v["preview"].as_str().expect("preview is a string");
    assert!(!preview.contains('\n'), "preview must be single-line");
    assert!(
        preview.ends_with('\u{2026}'),
        "long preview must end with ellipsis"
    );
    let without_ellipsis: &str = &preview[..preview.len() - '\u{2026}'.len_utf8()];
    assert!(
        without_ellipsis.chars().count() <= 80,
        "preview body must not exceed 80 chars before ellipsis"
    );
}

#[test]
fn preview_not_truncated_for_short_content() {
    let note = make_note("local", "short message", None);
    let v = note_to_message_json(&note);
    let preview = v["preview"].as_str().expect("preview is a string");
    assert_eq!(preview, "short message");
    assert!(!preview.ends_with('\u{2026}'));
}

#[test]
fn preview_collapses_whitespace_and_newlines() {
    let note = make_note("local", "line one\n  line two\n\nline three", None);
    let v = note_to_message_json(&note);
    let preview = v["preview"].as_str().expect("preview is a string");
    assert_eq!(preview, "line one line two line three");
}

#[test]
fn properties_and_content_still_present() {
    let note = make_note(
        "local",
        "body text",
        Some(json!({ "from_actor": "x", "custom": 42 })),
    );
    let v = note_to_message_json(&note);
    assert_eq!(v["content"], json!("body text"));
    assert_eq!(v["properties"]["custom"], json!(42));
}

#[test]
fn projection_derives_id_aliases_from_id_field() {
    let note = make_note("local", "projection fixture", None);
    let view = note_to_message_json(&note);
    let mut bare = view.clone();
    bare.as_object_mut()
        .expect("message view is an object")
        .retain(|key, _| key != "short_id" && key != "full_id");
    assert!(
        bare.get("short_id").is_none() && bare.get("full_id").is_none(),
        "fixture must actually drop the alias keys"
    );

    let fields = vec![
        "id".to_string(),
        "short_id".to_string(),
        "full_id".to_string(),
    ];
    let projected = project_message_json(bare, Some(&fields));
    let id = view["id"].as_str().expect("id is a string");
    assert_eq!(projected["id"], view["id"]);
    assert_eq!(projected["full_id"], json!(id));
    assert_eq!(projected["short_id"], json!(&id[..8]));
}

#[test]
fn null_defaults_when_no_properties() {
    let note = make_note("local", "no props", None);
    let v = note_to_message_json(&note);
    assert_eq!(v["to"], Value::Null);
    assert_eq!(v["subject"], Value::Null);
    assert_eq!(v["direction"], Value::Null);
    assert_eq!(v["read"], json!(false));
    assert_eq!(v["from"], json!("local"));
}
