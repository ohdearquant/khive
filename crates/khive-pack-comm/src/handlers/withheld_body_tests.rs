use khive_runtime::RuntimeError;
use khive_storage::note::Note;
use khive_storage::{ContentRef, StorageCapability, StorageError};
use serde_json::{json, Value};

use super::{is_delivered_withheld_message, original_read_error};

fn note(kind: &str, properties: Value) -> Note {
    Note {
        version: 1,
        key: None,
        id: uuid::Uuid::new_v4(),
        namespace: "local".into(),
        kind: kind.into(),
        status: "active".into(),
        name: None,
        content: "External email body withheld [agent_addressed.override]".into(),
        salience: None,
        decay_factor: None,
        expires_at: None,
        properties: Some(properties),
        created_at: 0,
        updated_at: 0,
        deleted_at: None,
    }
}

fn delivered_withheld() -> Value {
    json!({
        "screening_body_withheld": true,
        "origin": "external",
        "trust": "untrusted",
        "to_actor": "actor:recipient",
    })
}

#[test]
fn only_a_delivered_withheld_external_message_has_an_original_to_read() {
    assert!(is_delivered_withheld_message(&note(
        "message",
        delivered_withheld()
    )));

    // A quarantine record is refused whichever spelling stores the flag.
    for quarantined in [json!(true), json!("true")] {
        let mut props = delivered_withheld();
        props["quarantined"] = quarantined.clone();
        assert!(
            !is_delivered_withheld_message(&note("message", props)),
            "quarantined={quarantined}"
        );
    }
    for (key, value) in [
        ("screening_body_withheld", json!(false)),
        ("screening_body_withheld", json!("true")),
        ("origin", json!("internal")),
        ("trust", json!("trusted")),
    ] {
        let mut props = delivered_withheld();
        props[key] = value.clone();
        assert!(
            !is_delivered_withheld_message(&note("message", props)),
            "{key}={value}"
        );
    }
    for key in ["screening_body_withheld", "origin", "trust"] {
        let mut props = delivered_withheld();
        props.as_object_mut().expect("object").remove(key);
        assert!(
            !is_delivered_withheld_message(&note("message", props)),
            "without {key}"
        );
    }
    assert!(!is_delivered_withheld_message(&note(
        "observation",
        delivered_withheld()
    )));
}

#[test]
fn original_read_errors_never_name_the_content_reference() {
    let hex = "b".repeat(64);
    let content_ref = ContentRef::from_hex(hex.clone()).expect("fixture content ref");
    let id = uuid::Uuid::new_v4();
    let too_large = || StorageError::BlobTooLarge {
        content_ref: content_ref.clone(),
        max_bytes: 10,
        observed_at_least: 11,
    };
    let missing = || StorageError::NotFound {
        capability: StorageCapability::Blob,
        resource: "blob",
        key: hex.clone(),
    };
    let mismatch = || StorageError::BlobSizeMismatch {
        content_ref: content_ref.clone(),
        metadata_bytes: 3,
        actual_bytes: 4,
    };
    for error in [too_large(), missing(), mismatch()] {
        assert!(
            error.to_string().contains(&hex),
            "fixture: the storage error names the reference: {error}"
        );
        let mapped = original_read_error(id, error).to_string();
        assert!(!mapped.contains(&hex), "{mapped}");
        assert!(mapped.contains(&id.to_string()), "{mapped}");
    }
    let refused = original_read_error(id, too_large());
    assert!(
        matches!(&refused, RuntimeError::InvalidInput(message) if message.contains("10-byte read limit")),
        "{refused:?}"
    );
    assert!(matches!(
        original_read_error(id, missing()),
        RuntimeError::NotFound(_)
    ));
    assert!(matches!(
        original_read_error(id, mismatch()),
        RuntimeError::Internal(_)
    ));
}
