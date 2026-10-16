//! Wire round-trip of native SQLite FULL evidence on a terminal writer failure.

use super::*;

#[cfg(unix)]
#[tokio::test]
async fn terminal_native_full_codes_round_trip_without_widening_retry_policy() {
    let dir = tempfile::tempdir().unwrap();
    let client =
        EventsSplitClient::new(dir.path().join("never-bound.sock")).expect("client builds");
    let store = ForwardingEventStore::new("test", client);
    let extended = rusqlite::ffi::SQLITE_FULL | (3 << 8);
    let response = storage_error_response(&StorageError::WriterTaskTerminated {
        request_state: khive_storage::WriterTaskRequestState::SideEffectsUnknown,
        sqlite_full_codes: Some((rusqlite::ffi::SQLITE_FULL, extended)),
    });
    let bytes = serde_json::to_vec(&response).unwrap();
    let parsed: EventsResponse = serde_json::from_slice(&bytes).unwrap();
    let error = store.unexpected("append_events_idempotent", parsed);
    assert!(matches!(&error, StorageError::WriterTaskTerminated {
        request_state: khive_storage::WriterTaskRequestState::SideEffectsUnknown,
        sqlite_full_codes: Some((primary, code)),
    } if *primary == rusqlite::ffi::SQLITE_FULL && *code == extended));
    assert!(!error.is_retryable());
    assert_eq!(error.capability(), None);
    let value = crate::runtime_error_value(
        crate::RuntimeError::Storage(error),
        crate::DomainDisposition::Unknown,
    );
    assert_eq!(value["stage"], "sqlite_disk_full");
    assert_eq!(value["sqlite_primary_code"], rusqlite::ffi::SQLITE_FULL);
    assert_eq!(value["sqlite_extended_code"], extended);
    assert_eq!(value["request_state"], "side_effects_unknown");
    assert_eq!(value["task_terminated"], true);
    assert_eq!(value["retryable"], false);

    let old = br#"{"kind":"error","message":"died","retryable":false,"writer_task_failure":{"kind":"task_terminated","request_state":"side_effects_unknown"}}"#;
    let parsed: EventsResponse = serde_json::from_slice(old).unwrap();
    let error = store.unexpected("append", parsed);
    assert!(matches!(
        &error,
        StorageError::WriterTaskTerminated {
            sqlite_full_codes: None,
            ..
        }
    ));
    let response = storage_error_response(&error);
    let value = serde_json::to_value(&response).unwrap();
    assert!(value["writer_task_failure"]
        .get("sqlite_full_codes")
        .is_none());
    let projected = crate::runtime_error_value(
        crate::RuntimeError::Storage(error),
        crate::DomainDisposition::Unknown,
    );
    assert_eq!(projected["stage"], "writer_task_terminated");
    assert!(projected.get("sqlite_primary_code").is_none());
}
