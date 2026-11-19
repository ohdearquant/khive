use super::DocumentId;

/// DocumentId must serialize as a bare JSON string (enforced by serde transparent).
#[test]
fn document_id_serializes_as_plain_string() {
    let id = DocumentId::new("some-document-identifier");
    let json = serde_json::to_string(&id).expect("DocumentId serialize");
    assert_eq!(
        json, r#""some-document-identifier""#,
        "wire format drift detected in DocumentId — must be plain JSON string",
    );
}

#[test]
fn document_id_roundtrip() {
    let id = DocumentId::new("doc_abc_123");
    let json = serde_json::to_string(&id).expect("serialize");
    let back: DocumentId = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, id, "serde roundtrip must produce identical value");
}

#[test]
fn document_id_empty_string_roundtrip() {
    let id = DocumentId::new("");
    let json = serde_json::to_string(&id).expect("serialize");
    assert_eq!(
        json, r#""""#,
        "empty DocumentId must serialize as empty JSON string"
    );
    let back: DocumentId = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, id);
}

#[test]
fn document_id_unicode_roundtrip() {
    let id = DocumentId::new("doc_\u{4e2d}\u{6587}");
    let json = serde_json::to_string(&id).expect("serialize");
    let back: DocumentId = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, id);
}
