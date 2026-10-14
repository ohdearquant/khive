use super::*;
use crate::{Id128, Namespace};
#[cfg(feature = "serde")]
use alloc::string::ToString;

fn test_header() -> Header {
    Header::new(
        Id128::from_u128(1),
        Namespace::local(),
        Timestamp::from_secs(1700000000),
    )
}

#[test]
fn note_construction() {
    let note = Note {
        header: test_header(),
        kind: String::from("decision"),
        status: NoteStatus::Active,
        content: String::from("Use BGE-base for multilingual corpus"),
        properties: BTreeMap::new(),
        tags: alloc::vec!["retrieval".into()],
        salience: Some(0.8),
        decay_factor: Some(0.01),
        expires_at: None,
        deleted_at: None,
    };
    assert_eq!(note.kind, "decision");
    assert_eq!(note.tags.len(), 1);
}

#[test]
fn note_construction_uses_pack_owned_kind_string() {
    let note = Note {
        header: test_header(),
        kind: String::from("decision"),
        status: NoteStatus::Active,
        content: String::from("test"),
        properties: BTreeMap::new(),
        tags: alloc::vec![],
        salience: None,
        decay_factor: None,
        expires_at: None,
        deleted_at: None,
    };
    assert_eq!(note.kind, "decision");
}

#[test]
fn note_status_deleted_roundtrip() {
    use core::str::FromStr;
    assert_eq!(
        NoteStatus::from_str("deleted").unwrap(),
        NoteStatus::Deleted
    );
    assert_eq!(NoteStatus::Deleted.name(), "deleted");
}

#[test]
fn note_is_valid_checks_salience_range() {
    let mut note = Note {
        header: test_header(),
        kind: String::from("observation"),
        status: NoteStatus::Active,
        content: String::from("test"),
        properties: BTreeMap::new(),
        tags: alloc::vec![],
        salience: Some(1.5),
        decay_factor: None,
        expires_at: None,
        deleted_at: None,
    };
    assert!(!note.is_valid());
    note.salience = Some(0.5);
    assert!(note.is_valid());
}

#[test]
fn note_is_valid_checks_decay_non_negative() {
    let mut note = Note {
        header: test_header(),
        kind: String::from("observation"),
        status: NoteStatus::Active,
        content: String::from("test"),
        properties: BTreeMap::new(),
        tags: alloc::vec![],
        salience: None,
        decay_factor: Some(-0.1),
        expires_at: None,
        deleted_at: None,
    };
    assert!(!note.is_valid());
    note.decay_factor = Some(0.01);
    assert!(note.is_valid());
}

#[cfg(feature = "serde")]
#[test]
fn note_serde_rejects_salience_above_one() {
    let json = serde_json::json!({
        "id": "00000000-0000-0000-0000-000000000001",
        "namespace": "local",
        "created_at": 1700000000000000_u64,
        "updated_at": 1700000000000000_u64,
        "kind": "observation",
        "status": "active",
        "content": "test",
        "properties": {},
        "tags": [],
        "salience": 1.5,
        "decay_factor": null,
        "expires_at": null,
        "deleted_at": null
    });
    let result: Result<Note, _> = serde_json::from_value(json);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("[0.0, 1.0]"),
        "error should mention range: {err}"
    );
}

#[cfg(feature = "serde")]
#[test]
fn note_serde_rejects_negative_decay() {
    let json = serde_json::json!({
        "id": "00000000-0000-0000-0000-000000000001",
        "namespace": "local",
        "created_at": 1700000000000000_u64,
        "updated_at": 1700000000000000_u64,
        "kind": "observation",
        "status": "active",
        "content": "test",
        "properties": {},
        "tags": [],
        "salience": null,
        "decay_factor": -0.5,
        "expires_at": null,
        "deleted_at": null
    });
    let result: Result<Note, _> = serde_json::from_value(json);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("non-negative"),
        "error should mention non-negative: {err}"
    );
}

#[cfg(feature = "serde")]
#[test]
fn note_serde_accepts_valid_values() {
    let json = serde_json::json!({
        "id": "00000000-0000-0000-0000-000000000001",
        "namespace": "local",
        "created_at": 1700000000000000_u64,
        "updated_at": 1700000000000000_u64,
        "kind": "decision",
        "status": "active",
        "content": "test content",
        "properties": {},
        "tags": ["tag1"],
        "salience": 0.8,
        "decay_factor": 0.01,
        "expires_at": null,
        "deleted_at": null
    });
    let note: Note = serde_json::from_value(json).expect("valid note should deserialize");
    assert_eq!(note.salience, Some(0.8));
    assert_eq!(note.decay_factor, Some(0.01));
}
