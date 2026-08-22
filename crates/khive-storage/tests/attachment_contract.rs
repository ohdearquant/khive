use khive_storage::{
    Attachment, AttachmentSubstrate, ContentRef, NewAttachment, StorageCapability, StorageError,
};
use uuid::Uuid;

fn new_attachment(role: &str, size_bytes: Option<u64>) -> NewAttachment {
    NewAttachment {
        role: role.to_string(),
        content_ref: ContentRef::from_hex("a".repeat(64)).expect("canonical ref"),
        media_type: Some("application/octet-stream".to_string()),
        size_bytes,
    }
}

#[test]
fn attachment_role_rejects_empty_and_control_characters() {
    for role in ["", "content\nshadow", "fann\u{0000}network"] {
        let error = new_attachment(role, Some(1))
            .validate()
            .expect_err("invalid role must be rejected");
        assert!(matches!(
            error,
            StorageError::InvalidInput {
                capability: StorageCapability::Attachments,
                ..
            }
        ));
    }
}

#[test]
fn attachment_size_must_fit_sqlite_integer() {
    new_attachment("content", Some(i64::MAX as u64))
        .validate()
        .expect("SQLite's maximum signed integer must be accepted");

    let error = new_attachment("content", Some(i64::MAX as u64 + 1))
        .validate()
        .expect_err("size above SQLite INTEGER must be rejected");
    assert!(matches!(
        error,
        StorageError::InvalidInput {
            capability: StorageCapability::Attachments,
            ..
        }
    ));
}

#[test]
fn from_new_preserves_attachment_metadata_and_identity() {
    let record_uuid = Uuid::new_v4();
    let new = new_attachment("fann-network", Some(42));
    let expected_ref = new.content_ref.clone();

    let attachment = Attachment::from_new(record_uuid, AttachmentSubstrate::Entity, new, 123_456);

    assert_eq!(attachment.record_uuid, record_uuid);
    assert_eq!(attachment.substrate, AttachmentSubstrate::Entity);
    assert_eq!(attachment.role, "fann-network");
    assert_eq!(attachment.content_ref, expected_ref);
    assert_eq!(
        attachment.media_type.as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(attachment.size_bytes, Some(42));
    assert_eq!(attachment.created_at, 123_456);
}

#[test]
fn attachment_substrate_has_stable_lowercase_wire_values() {
    assert_eq!(
        serde_json::to_string(&AttachmentSubstrate::Entity).unwrap(),
        "\"entity\""
    );
    assert_eq!(
        serde_json::to_string(&AttachmentSubstrate::Note).unwrap(),
        "\"note\""
    );
    assert_eq!(
        serde_json::from_str::<AttachmentSubstrate>("\"entity\"").unwrap(),
        AttachmentSubstrate::Entity
    );
    assert_eq!(
        serde_json::from_str::<AttachmentSubstrate>("\"note\"").unwrap(),
        AttachmentSubstrate::Note
    );
}
